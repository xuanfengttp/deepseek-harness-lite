# KV Cache 优化设计

> 版本: v2 — 增加动态内容注入、分块协调、追加策略讨论

DeepSeek API（及兼容的 OpenAI 协议 API）支持前缀缓存：如果本次请求的前 N 个 token 与上次完全一致，那 N 个 token 的 KV cache 直接复用，不重新计算。命中率越高，TTFT 越低、token 费用越省。

Lite 通过三层设计最大化缓存命中，并以「追加优先」原则处理动态内容。

---

## 核心原理

KV cache 是**前缀匹配**的。请求上下文从头开始逐 token 匹配：

```
Token 序列: [A][B][C][D][E][F][G]
                                    ↑ 上次请求到这里

本次请求:   [A][B][C][D][E][F][G][H][I]
                                    ↑ 命中     ↑ miss

命中 = 不重新计算，直接复用 KV
miss  = 重新计算 attention KV
```

**黄金法则：任何在中间插入或修改内容的操作，都会使其后所有 token 变成 miss。**

---

## 第一层：System Prompt 分层排序

`src/prompt.rs` 将 system prompt 拆成 5 个有序 section，**固定内容在前，动态内容在后**：

```
┌──────────────────────────────────────────────────────┐
│ Section              │ order │ 变化频率     │ 缓存价值 │
├──────────────────────┼───────┼───────────────┼──────────┤
│ harness:identity     │ -100  │ 永不变        │ 最高     │
│ behavior-rules       │  -90  │ 永不变        │ 最高     │
│ tools (tool guidance)│  -80  │ 半固定        │ 高       │
│ persona (skill body) │    0  │ 切 skill 时变 │ 中       │
│ custom-prompt        │   10  │ 改设置时变    │ 低       │
└──────────────────────┴───────┴───────────────┴──────────┘
```

改 persona 或 custom-prompt 只 invalidate 后缀，前面的 identity + rules + tools 的 KV cache 不受影响。

```rust
// src/prompt.rs
pub const ORDER_IDENTITY: i32 = -100;
pub const ORDER_RULES: i32 = -90;
pub const ORDER_TOOLS: i32 = -80;
pub const ORDER_PERSONA: i32 = 0;
pub const ORDER_CUSTOM: i32 = 10;
```

## 第二层：启动预热

`src/main.rs:preheat_kv_cache()` 在进程启动时发一个最小请求，让 API 提前缓存 system prompt + tools schema：

```rust
let request = LlmRequest {
    system: assembled.system,       // 完整 system prompt
    messages: vec![User("ready")],  // 最小 user message
    tools: assembled.tools,         // 完整工具定义
    max_tokens: 1,                  // 只要 1 个 token
    think: ThinkLevel::Off,         // 不思考
};
```

- fire-and-forget：响应丢弃，不显示给用户
- 错误不阻断：preheat 失败只 log warn

## 第三层：Compaction 保持前缀稳定

`src/compaction.rs` + `src/session.rs` 在上下文接近窗口阈值时触发压缩。

### 压缩前

```
[system] [tools_schema] [user1] [asst1] [tool1] [asst2] ... [userN] [asstN]
                                                        ↑ 触发阈值
```

### 压缩后

```
[system] [tools_schema] [Summary] [keep_recent 的最近消息]
```

**关键设计**：

1. Summary 放在消息列表头部（紧跟 system + tools 之后），后续请求的 `[system] [tools_schema]` 前缀仍能命中缓存
2. Summary 请求用独立上下文 — 不把整个对话发给 LLM 去压缩
3. 保留最近 `keep_recent_turns × 6` 个事件不压缩

---

## 动态内容注入问题

### 问题场景

实际运行中，以下内容可能动态注入上下文：

| 动态内容 | 当前注入位置 | 对缓存的影响 |
|----------|-------------|-------------|
| Skill 切换 | system prompt persona (order=0) | invalidate persona 及之后 |
| Memory recall 结果 | tool result，追加在消息尾部 | ✅ 不破坏前缀 |
| Todo 模式 guidance 注入 | user message，追加在消息尾部 | ✅ 不破坏前缀 |
| 工具配置变更 | system prompt tools (order=-80) | invalidate tools 及之后 |

**危险场景**：如果将动态内容插入消息流中间（例如在历史消息之间插入 memory context），会从插入点开始打断所有后续 token 的缓存：

```
[system] [tools] [user1] [asst1] [MEMORY注入] [asst2] [tool2] [asst3]
                                    ↑ 从这里开始全部 miss
```

### 设计原则：Append-Only Context

**所有动态内容一律追加在消息尾部，永不插入中间。**

| 内容类型 | 注入方式 | 理由 |
|----------|---------|------|
| Memory recall | 通过 `memory_recall` 工具调用，结果作为 tool result 追加 | 工具结果天然在尾部 |
| Skill guidance (todo 模式) | 作为 user message 追加 (`agent.rs:262`) | 追加不破坏前缀 |
| Workflow 步骤指令 | 作为 user message 追加 | 同上 |
| 角色切换提示 | 作为 user message 追加，不改 system prompt | 避免 invalidate system 缓存 |

**例外：Skill 切换**。切换 skill 时 system prompt 的 persona section 变化，invalidate 从 persona 开始的后缀。这是不可避免的 — skill 切换是低频操作，且 identity + rules + tools 前缀仍命中。

### 不做的事情

- ❌ 不在消息中间插入 memory context
- ❌ 不在 system prompt 和消息之间插入动态 block
- ❌ 不修改已有消息内容（即使发现旧消息有误）

---

## 分块协调问题

### 思路

模型部署端（如 vLLM）的 KV cache 按固定大小的 block 管理（默认 16 token/block）。block 是 prefix matching 的最小粒度：一个 block 内的 token 序列完全一致才复用该 block 的 KV，否则整个 block miss。

如果 agent 知道分词器和 block 大小，就可以让 section 边界对齐 block 边界。这样当后面的 section 变化时，前面 section 占用的 block 不受影响，可以独立复用。

### 自部署场景下分词器可行

在自部署场景（如 vLLM + DeepSeek 模型）中：

- **分词器已知** — 用同款分词器（如 `tiktoken-rs` 或模型发布的 tokenizer.json），agent 可以精确计数 token
- **Block 大小已知** — vLLM 配置项 `block_size`（默认 16），自部署可以直接读取
- **运行时对齐可行** — 在 section 之间补充换行/空格，让 token 数对齐 block 边界，对模型理解几乎无影响

### 为什么当前不做

| 因素 | 分析 |
|------|------|
| **边际收益低** | section 边界处最多浪费 `block_size - 1 = 15` 个 token 的缓存。整个 system prompt 约 300-500 token，浪费率最多 15/300 ≈ 5%。且只有后续 section 变化时才触发这个浪费 — 正常 append-only 运行中前缀完全一致，无浪费 |
| **收益场景窄** | 只有 skill 切换（persona 变化）时，block 对齐能让 identity+rules+tools 的 block 独立复用。但 skill 切换是低频操作，切换后命中率本就要重新爬升 |
| **复杂度引入** | 需要：(1) Rust 引入分词器依赖（增加二进制体积）；(2) 运行时 tokenize system prompt 计算 token 数；(3) 动态填充对齐字符；(4) block_size 作为配置项 |
| **已有收益足够** | 三层设计（分层排序 + 预热 + 压缩保前缀）在实测中已达 85-99% 命中率，block 对齐的 5% 边际收益难以体现 |

### 可选的后续优化

如果未来命中率有明确瓶颈（如 skill 频繁切换导致长期低于 70%），可以引入 block 对齐作为可选优化：

```toml
# config/default.yaml
[cache]
align_blocks = true          # 开启 block 对齐
block_size = 16              # vLLM block_size
tokenizer = "deepseek-v3"    # 分词器类型
```

实现路径：启动时 tokenize system prompt 各 section，在边界处补 `\n` 填充到 block 整数倍。填充字符不影响模型理解（就是多了几个空行）。

当前阶段优先保证前缀稳定（已实现），block 对齐作为低优先级储备方案。

---

## 追加策略 + 压缩时重排

### 核心思路

正常运行期间：**所有新内容追加在尾部，永不修改已有部分**。这是缓存最优策略 — 每次请求只 miss 新增的尾部内容，前缀全部命中。

压缩触发时：**这是唯一的重排窗口**，重新结构化整个上下文。

### 正常运行（两次压缩之间）

```
请求 1: [system] [tools] [user1]
                                    ← 命中 [system][tools], miss [user1]

请求 2: [system] [tools] [user1] [asst1] [tool1] [asst2]
                                    ← 命中到 [asst2] 前, miss [asst2]

请求 3: [system] [tools] [user1] [asst1] [tool1] [asst2] [user2]
                                    ← 命中到 [user2] 前, miss [user2]
```

每次只 miss 最后一轮新增内容。命中率随对话进行持续升高。

### 压缩时（唯一的重排点）

```
压缩前: [system] [tools] [user1] [asst1] [tool1] ... [userN] [asstN]
                                                    ↑ 超过阈值

压缩后: [system] [tools] [Summary] [userN-2] [asstN-2] [toolN-2] [userN] [asstN]
                        ↑ 新内容     ↑ 保留的最近消息
```

- `[system] [tools]` 前缀仍命中（预热 + 之前积累的缓存）
- `[Summary]` 是新内容，miss（不可避免）
- `[Summary]` 之后的消息在下次请求时开始积累缓存
- 压缩后恢复 append-only，命中率重新爬升

### 为什么不在压缩前重排

压缩前重排（比如把重要消息提前）会立即 invalidate 所有被移动消息之后的缓存，且这些消息马上就要被压缩掉了 — 重排的缓存代价没有回收窗口。不如保持原序，等压缩时一次性重排。

### 压缩后 Summary 的位置选择

| 方案 | 缓存效果 | 语义效果 |
|------|---------|---------|
| Summary 在头部（system 之后） | ✅ 最近消息在尾部，追加新消息不破坏 Summary 缓存 | ✅ 摘要作为背景，最近对话是主线 |
| Summary 在尾部（消息最后） | ❌ 每次追加新消息都 invalidate Summary 缓存 | ❌ 摘要在最新消息后面，语义混乱 |

**选择：Summary 在头部**。这是当前实现（`session.rs:apply_compaction`），既缓存友好又语义正确。

---

## 完整请求结构

```
┌─────────────────────────────────────────────────────┐
│ 1. system message                                    │  ← KV cache 命中区
│    ├─ identity (order=-100, 永不变)                  │     [预热建立]
│    ├─ behavior-rules (order=-90, 永不变)             │
│    ├─ tool guidance (order=-80, 半固定)              │
│    ├─ persona / skill body (order=0, 动态)           │     [skill 切换时变]
│    └─ custom-prompt (order=10, 动态)                 │     [设置变更时变]
├─────────────────────────────────────────────────────┤
│ 2. tools schema (半固定)                             │  [skill 切换时变]
├─────────────────────────────────────────────────────┤
│ 3. conversation messages (append-only)               │  ← 追加在尾部
│    ├─ [CompactionSummary] (如有, 在头部)             │     [压缩时重置]
│    ├─ User → Assistant → Tool → Assistant → ...      │     [每轮追加]
│    └─ 最新一轮                                        │     [唯一 miss 区]
└─────────────────────────────────────────────────────┘
```

## 命中率观察

前端底部状态栏实时显示 `缓存命中 XX%`，数据来自 API 返回的 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`（模型返回，非 agent 估算）。

| 阶段 | 预期命中率 | 原因 |
|------|-----------|------|
| 首次对话 | 60-80% | system + tools 前缀命中（预热效果），对话内容 miss |
| 第二轮起 | 85-99% | 前缀 + 之前所有消息命中，只 miss 新增的最后一轮 |
| compaction 后 | 70-85% | system + tools 仍命中，Summary 是新的所以 miss，之后继续累积 |
| skill 切换后 | 50-70% | identity + rules 仍命中，从 tools/persona 开始 miss，之后重新爬升 |

## 设计原则总结

1. **固定在前，动态在后** — system prompt 分层排序，固定内容最大化前缀缓存
2. **Append-only** — 所有动态内容追加在尾部，永不插入中间
3. **压缩时重排** — 压缩是唯一的重排窗口，Summary 放头部，最近消息在尾部
4. **前缀稳定优先，block 对齐储备** — 当前靠前缀稳定已达 85-99% 命中率，block 对齐（自部署可行）作为低优先储备方案，边际收益约 5%
5. **预热建缓存** — 启动时发最小请求，提前建立 system + tools 前缀缓存

## 相关代码

| 文件 | 职责 |
|------|------|
| `src/prompt.rs` | section 分层排序，`ORDER_*` 常量 |
| `src/main.rs` | `preheat_kv_cache()` 启动预热 |
| `src/compaction.rs` | `needs_compaction()` + `compact()` 独立上下文摘要 |
| `src/session.rs` | `apply_compaction()` Summary 插入头部 |
| `src/agent.rs` | `run_llm_step()` 组装请求 + 触发 compaction + todo guidance 追加 |
| `src/llm.rs` | `ApiUsage` 解析 `prompt_cache_hit_tokens` |
| `web/index.html` | `sessionStats.cacheHitTokens` 累加 + 命中率计算显示 |
