# KV Cache 优化设计

DeepSeek API（及兼容的 OpenAI 协议 API）支持前缀缓存：如果本次请求的前 N 个 token 与上次完全一致，那 N 个 token 的 KV cache 直接复用，不重新计算。命中率越高，TTFT 越低、token 费用越省。

Lite 通过三层设计最大化缓存命中。

---

## 1. System Prompt 分层排序

`src/prompt.rs` 将 system prompt 拆成 5 个有序 section，**固定内容在前，动态内容在后**：

```
┌──────────────────────────────────────────────────────┐
│ Section              │ order │ 变化频率     │ 缓存价值 │
├──────────────────────┼───────┼───────────────┼──────────┤
│ harness:identity     │ -100  │ 永不变        │ 最高     │
│ behavior-rules       │  -90  │ 永不变        │ 最高     │
│ tools (tool guidance │  -80  │ 半固定        │ 高       │
│ persona (skill body) │    0  │ 切 skill 时变 │ 中       │
│ custom-prompt        │   10  │ 改设置时变    │ 低       │
└──────────────────────┴───────┴───────────────┴──────────┘
```

**原理**：KV cache 是前缀匹配的。改 persona 或 custom-prompt 只 invalidate 后缀，前面的 identity + rules + tools 的 KV cache 不受影响。

```rust
// src/prompt.rs
pub const ORDER_IDENTITY: i32 = -100;  // "You are an AI agent. Working directory: {{cwd}}."
pub const ORDER_RULES: i32 = -90;      // 行为规则
pub const ORDER_TOOLS: i32 = -80;      // 工具使用指导
pub const ORDER_PERSONA: i32 = 0;      // skill body
pub const ORDER_CUSTOM: i32 = 10;      // 用户自定义 prompt
```

sections 按 `order` 升序排列后拼接，生成最终 system message。

## 2. KV Cache 预热

`src/main.rs:preheat_kv_cache()` 在进程启动时发一个最小请求，让 API 提前缓存 system prompt + tools schema：

```rust
let request = LlmRequest {
    system: assembled.system,       // 完整 system prompt
    messages: vec![User("ready")],  // 最小 user message
    tools: assembled.tools,         // 完整工具定义
    max_tokens: 1,                  // 只要 1 个 token，省输出
    think: ThinkLevel::Off,         // 不思考，省 reasoning
};
```

- **fire-and-forget**：响应被 drain 但丢弃，不显示给用户
- **错误不阻断**：preheat 失败只 log warn，不影响正常启动
- 之后真正的用户对话请求来了，前缀直接命中缓存

## 3. Compaction 保持前缀稳定

`src/compaction.rs` + `src/session.rs` 在上下文接近窗口阈值时触发压缩。

### 压缩前

```
[system] [tools_schema] [user1] [asst1] [tool1] [asst2] ... [userN] [asstN]
                                                        ↑ 触发阈值 (context_window × 0.7)
```

### 压缩后

```
[system] [tools_schema] [Summary] [keep_recent_turns 的最近消息]
```

**关键设计**：

1. **Summary 放在消息列表头部**（紧跟 system + tools 之后），最近消息在尾部。后续请求的前缀 `[system] [tools_schema]` 仍能命中缓存。

2. **Summary 请求用独立上下文** — 不把整个对话发给 LLM 去压缩，而是把旧消息渲染成纯文本，用一个单独的 LLM 调用生成摘要。防止摘要请求本身膨胀上下文。

3. **保留最近 `keep_recent_turns × 6` 个事件**不压缩，保证当前任务上下文完整。

```rust
// src/agent.rs
let keep_events = self.keep_recent_turns * 6;
self.session.apply_compaction(result.summary, keep_events);
```

## 完整请求结构

每次 LLM 请求的上下文布局（从上到下）：

```
┌─────────────────────────────────────────────────┐
│ 1. system message                                │  ← KV cache 命中区
│    ├─ identity (order=-100, 永不变)              │
│    ├─ behavior-rules (order=-90, 永不变)         │
│    ├─ tool guidance (order=-80, 半固定)          │
│    ├─ persona / skill body (order=0, 动态)       │
│    └─ custom-prompt (order=10, 动态)             │
├─────────────────────────────────────────────────┤
│ 2. tools schema (半固定)                         │  ← skill 不切就不变
├─────────────────────────────────────────────────┤
│ 3. conversation messages (追加在尾部)             │  ← 追加不破坏前缀
│    ├─ [CompactionSummary] (如有)                 │
│    ├─ User → Assistant → Tool → Assistant → ...  │
│    └─ 最新一轮                                    │
└─────────────────────────────────────────────────┘
```

## 命中率观察

前端底部状态栏实时显示 `缓存命中 XX%`，数据来自 API 返回的 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`。

| 阶段 | 预期命中率 | 原因 |
|------|-----------|------|
| 首次对话 | 60-80% | system + tools 前缀命中（预热效果），对话内容 miss |
| 第二轮起 | 85-99% | 前缀 + 之前所有消息命中，只 miss 新增的最后一轮 |
| compaction 后 | 70-85% | system + tools 仍命中，Summary 是新的所以 miss，之后继续累积 |

## 相关代码

| 文件 | 职责 |
|------|------|
| `src/prompt.rs` | section 分层排序，`ORDER_*` 常量 |
| `src/main.rs` | `preheat_kv_cache()` 启动预热 |
| `src/compaction.rs` | `needs_compaction()` + `compact()` 独立上下文摘要 |
| `src/session.rs` | `apply_compaction()` Summary 插入头部 |
| `src/agent.rs` | `run_llm_step()` 组装请求 + 触发 compaction |
| `src/llm.rs` | `ApiUsage` 解析 `prompt_cache_hit_tokens` |
| `web/index.html` | `sessionStats.cacheHitTokens` 累加 + 命中率计算显示 |
