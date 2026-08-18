# DSH 上游 v0.1.0-rc.7 变更分析报告

> 对比范围：`fb82698709`（rc.6 release）→ `99f6f02fec`（rc.7 release）
> 非合并提交：67 个，538 文件变更，+8181/-1623 行
> 参考代码已同步至 `reference/` 目录

---

## 一、官方变更说明（rc.7 Release Notes）

### 新增功能
1. 各插件可自行注册设置卡片（plugin-owned settings surface）
2. Codex 与 Claude Code 子代理任务接入 Job Panel
3. MCP/ACP 支持持久化图片附件，PTC Mode 可转发嵌套图片

### 问题修复
1. 修复极简模式下持久 Bash 调用卡顿
2. 修复大历史消息分页栈溢出
3. 修复 max-tokens 截断导致会话无法继续
4. 修复 Safari 输入框光标与文本错位
5. 升级 node-pty 1.2 beta，改善 PTY 平台兼容性

### 体验优化
1. 优化 Cordis 动态插件面板
2. DeepSeek 模型新增 `low` 推理强度，默认仍为 `high`
3. 英文内置预设 `Code mode` 更名为 `PTC mode`
4. 提问卡片支持折叠并保留草稿

---

## 二、逐项分析：跟不跟进

### ✅ 应该跟进

#### 1. `reasoning_content` 回传规则（关键！）
**上游变更**：`packages/llm/llm-deepseek/src/serialize.ts` 第 96-99 行

官方 DeepSeek thinking mode 有一个**回传规则**：当 assistant 轮次包含 tool_calls 时，**必须**将 `reasoning_content` 一起发回 API。不含 tool_calls 的普通轮次则丢弃 reasoning（节省 token）。

```typescript
// Official passback rule (guides/thinking_mode.mdx): reasoning_content
// must return on tool-call turns; it is ignored on plain turns, so we
// drop it there to save tokens.
...toolCalls.length > 0 && reasoning.length > 0 ? { reasoning_content: reasoning } : {},
```

**Lite 现状**：我们刚实现了 thinking 持久化（存入 `SessionEvent::AssistantMessage.thinking`），但 `derive_messages()` 和 `ApiMessage` 完全忽略 thinking 字段。`Message::Assistant` 没有 reasoning_content 字段，`ApiMessage` 也没有。

**影响**：在 thinking mode + tool call 的多步对话中，DeepSeek API 可能因为缺少 `reasoning_content` 回传而行为异常或报错。这是**协议级**的问题，不是体验问题。

**建议**：
- `ApiMessage` 增加 `reasoning_content: Option<String>` 字段（`skip_serializing_if = "Option::is_none"`）
- `derive_messages()` 在构造 `Message::Assistant` 时，当 `tool_calls` 非空且 `thinking` 非空时，携带 reasoning_content
- 或者更简单：在 `llm.rs` 的 `build_request` 中，从 `Message::Assistant` 序列化时，检查原始 event 的 thinking 字段
- 难点：`Message::Assistant` 目前不携带 thinking，需要决定是在 `Message` 枚举加字段，还是在 `ApiMessage` 构造时从 event log 查找

**工作量**：中等。需要在 `Message` 或 `ApiMessage` 层面增加 reasoning_content 通道，并确保只在 tool-call 轮次发送。

---

#### 2. max-tokens 截断导致会话无法继续
**上游变更**：`7e95a00c8a` — fix(llm): align replay state with assembled content and degrade unusable state

**问题描述**：当模型响应因 `max_tokens` 截断且包含 tool call 时，持久化的内容与回放元数据不一致，导致下一次请求在历史重建时失败（`INVALID_REPLAY_STATE`），会话永久卡死。

**上游修复**：
- 写入侧：finish chunk 的 replayState 变为类型化 `ReplayEnvelope`，BlockAssembler 对 blocks 和 entries 统一做 keep/drop 决策
- 读取侧：持久化内容是权威的，对不可用状态（foreign kind、其他版本、畸形元数据、content/block 不匹配）降级为 provider-neutral 转换 + 诊断日志，而不是直接失败

**Lite 现状**：Lite 没有 BlockAssembler/ReplayEnvelope 这套机制（太重），但核心问题类似——如果模型在 thinking mode 下因 max_tokens 截断，`thinking_content` 可能不完整，`full_content` 也可能不完整。

**建议**：
- Lite 的风险较低（没有复杂的 replay state），但仍应处理 `finish_reason: "length"` 的情况
- 在 agent loop 中检测到 `finish_reason == "length"` 时，记录警告日志
- 确保截断的 assistant message 仍能被持久化和重建（当前实现已经满足——我们存的就是收到的内容）
- **优先级低**，但值得加一个 finish_reason 检测和日志

**工作量**：小。主要是加 finish_reason 判断和日志。

---

#### 3. `low` 推理强度支持
**上游变更**：`226600147e` — feat(llm-deepseek): support low reasoning effort

上游新增了 `reasoning_effort: 'low'` 选项，映射关系：
- `off` → `thinking: {type: 'disabled'}`（不发 `reasoning_effort`）
- `low` → `thinking: {type: 'enabled'}` + `reasoning_effort: 'low'`
- `high` → `thinking: {type: 'enabled'}` + `reasoning_effort: 'high'`
- `max` → `thinking: {type: 'enabled'}` + `reasoning_effort: 'max'`

**Lite 现状**：Lite 的 `reasoning_effort` 只有 `"high"` 和 `None`（think=false 时不发，think=true 时发 `"high"`）。没有 `low`/`max` 选项。

**建议**：
- 在 `LlmRequest` 中增加 `reasoning_effort` 的枚举选项（`None`/`"low"`/`"high"`/`"max"`）
- 配置文件或 skill 的 `think` 字段可以接受 `true/false` 或 `"low"/"high"/"max"`
- 对嵌入式设备，`low` 推理强度很有价值——减少 reasoning token，加快响应
- **优先级中**，对网络设备场景实用

**工作量**：小。改 `LlmRequest`、skill 解析、配置读取。

---

### ⏸️ 暂不跟进

#### 4. 插件自行注册设置卡片
**原因**：Lite 是单二进制，没有插件系统，设置通过 `config/default.yaml` + Web 设置面板直接管理。这个功能是 DSH 多插件架构的 UI 需要，Lite 不适用。

#### 5. Codex/Claude Code 子代理接入 Job Panel
**原因**：Lite 的 SubagentTool 是轻量级的，没有 Job Panel UI。这个功能依赖 DSH 的 Job Panel 基础设施，Lite 没有。

#### 6. MCP/ACP 持久化图片附件
**原因**：Lite 面向网络设备的文本交互场景，不支持图片。MCP/ACP 协议也不在 Lite 的范围内。

#### 7. PTC Mode 嵌套图片转发
**原因**：同上，Lite 不处理图片内容。

#### 8. node-pty 1.2 beta 升级
**原因**：Lite 是 Rust，不使用 node-pty。Lite 的 shell 工具直接用 `tokio::process`。

#### 9. Safari 输入框光标错位修复
**原因**：这是 DSH Web 前端（React/Cordis）的 Safari 兼容问题。Lite 的 Web 前端是单个 `index.html`，使用原生 textarea，不受此问题影响。但如果 Lite 未来遇到 Safari 兼容问题，可以参考上游的 `safari.ts` 修复方案。

#### 10. 持久 Bash 卡顿修复
**原因**：Lite 没有持久 Bash/PTY 会话。Lite 的 shell 工具是单次执行模式。

#### 11. 大历史分页栈溢出
**原因**：这是 DSH Web 前端 JavaScript 的递归分页问题。Lite 的 Web 前端用简单的分页逻辑，不存在递归栈溢出风险。但如果 Lite 的历史列表增长到很大，可以注意避免递归渲染。

#### 12. Cordis 动态插件面板优化
**原因**：Lite 不使用 Cordis 插件系统。

#### 13. Code mode → PTC mode 重命名
**原因**：Lite 没有预设（preset）系统。Lite 的 skill 是 YAML 文件，用户自定义。

#### 14. 提问卡片折叠 + 草稿保留
**原因**：这是 DSH `ask-user` 工具的 UI 交互。Lite 的 `ask_user_question` 工具已有基本的提问 UI，折叠/草稿是体验优化，**优先级低**，可以后续按需跟进。

---

## 三、总结

| 项目 | 优先级 | 工作量 | 跟进 |
|------|--------|--------|------|
| reasoning_content 回传规则 | **高** | 中 | ✅ 立即 |
| low 推理强度支持 | 中 | 小 | ✅ 近期 |
| max-tokens 截断处理 | 低 | 小 | ✅ 可选 |
| 插件设置卡片 | — | — | ⏸️ 不适用 |
| 子代理 Job Panel | — | — | ⏸️ 不适用 |
| 图片附件 | — | — | ⏸️ 不适用 |
| node-pty 升级 | — | — | ⏸️ 不适用 |
| Safari 修复 | — | — | ⏸️ 不适用 |
| 持久 Bash 修复 | — | — | ⏸️ 不适用 |
| 分页栈溢出 | — | — | ⏸️ 不适用 |
| Cordis 面板 | — | — | ⏸️ 不适用 |
| PTC mode 重命名 | — | — | ⏸️ 不适用 |
| 提问卡片折叠 | 低 | 中 | ⏸️ 暂缓 |

### 建议执行顺序

1. **reasoning_content 回传**（协议级修复，影响 thinking mode + tool call 的正确性）
2. **low 推理强度**（嵌入式场景实用，减 token 加速响应）
3. **max-tokens 截断日志**（防御性，小改动）
