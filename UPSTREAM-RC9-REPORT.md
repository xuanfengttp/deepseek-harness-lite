# DSH 上游 v0.1.2-rc.1 变更分析报告

> 对比范围：`dsh-v0.1.0-rc.8` → `upstream/master`（含 v0.1.3-alpha.1 预览）
> 非合并提交：1620 个，8524 文件变更
> 跨越版本：v0.1.1-rc.1, v0.1.1-rc.2, v0.1.2-alpha.1~5, v0.1.2-rc.1, v0.1.3-alpha.1
> 同步目标：**v0.1.2-rc.1**（稳定 RC），v0.1.3-alpha.1 破坏性变更为预览参考
> 分析方法：4 个 subagent 并行深度分析 llm-deepseek / session+compaction / agent-loop+subagent / 跨模块

## 一、官方变更说明（按版本）

### v0.1.1-rc.1
- 新增多模态视觉模型 `DeepSeek-V4-Flash-Vision-Exp`
- 修复 Bubblewrap 沙箱 /proc 绕过
- 优化缓存命中率 99.x% 精度显示

### v0.1.1-rc.2
- Files API 优先上传图像并复用
- 图像预处理（自动缩放+格式转换）

### v0.1.2-rc.1（主要同步目标）
- **子代理模型选择**：Agent 自选或调用方指定 provider/model/reasoning/max_tokens
- **父子代理 `send_message` 双向通信**，取代单向 `report`
- **连接状态显示** + 断线自动重试
- **图片发送后立即显示**，压缩上传后台进行
- **上下文压缩会计入图片占用**
- **会话日志截断尾部自动修复**
- **修复系统提示词 workflow 分区顺序**
- **调整提示词顺序，Shell 指南稳定在其他工具指南之前**
- **移除 SQLite Session 后端**
- `Session.events` 被按需读取 API 取代
- 文件编辑工具接受 null 占位值
- Web PTC Mode 默认不提供 workflow 工具

### v0.1.3-alpha.1（预览，本次不同步）
- 所有出站网络请求遵循 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`
- **DeepSeek 流式工具调用续传分片用空值覆盖调用 ID 或名称**（关键 bug fix）
- Session format v2（破坏性）
- SessionHandle / session lock（破坏性）

---

## 二、✅ 应该跟进（无需讨论，明确需要修复/对齐）

### 1. 流式工具调用空值覆盖修复 ★最高优先
- **上游变更**（commit `a1271a4903`）：续传 SSE delta 中 `id`/`name` 为空字符串或 `null` 时，不应覆盖已建立的调用身份。新增 `acceptIdentity()` 只接受非空字符串。**注意：上游在 `b03261caad` 中撤销了配套的 `MALFORMED_TOOL_CALL` 拒绝**（因为会覆盖 provider 安全的 max-tokens finish 为 5 次重试），最终只保留 `acceptIdentity`
- **Lite 现状**：`src/llm.rs:550-556` — `if let Some(id) = tc.id { entry.0 = id; }` — serde 把 `null` 映射为 `None`（安全），但 `""` 映射为 `Some("")`（**会覆盖为空字符串**）。这就是上游修复的同一个 bug
- **影响**：某些 OpenAI 兼容网关续传 delta 发送空 id/name → 工具调用失败（`unknown tool ""`），空 callId 写入 session 日志导致无法重新打开
- **工作量**：**S** — 两个 `if !s.is_empty()` 守卫。**不要**加 MALFORMED_TOOL_CALL 拒绝

### 2. `cached_tokens` OpenAI 兼容别名
- **上游变更**：`mapUsage` 中 `cacheRead = usage.prompt_tokens_details?.cached_tokens ?? usage.prompt_cache_hit_tokens`。有些网关只发 `prompt_tokens_details.cached_tokens` 而非 `prompt_cache_hit_tokens`
- **Lite 现状**：`src/llm.rs:156-164` — `ApiUsage` 只读 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`，不读 `prompt_tokens_details.cached_tokens`
- **影响**：某些兼容网关的缓存命中数据不被 Lite 读取，前端命中率显示 0%
- **工作量**：**S** — 在 `ApiUsage` 加 `prompt_tokens_details` 嵌套解析 + fallback

### 3. derive_messages surface-replace 语义 ★核心价值
- **上游变更**：compaction 从"append 一条摘要消息"改为"replace 一段历史区间"。`user/message` 带 `surfaceOp:{op:'replace', start, end}` + `sourceEventSeqs` 覆盖旧区间——旧消息从派生历史中移除
- **Lite 现状**：`apply_compaction` 用 `CompactionSummary` append 语义——summary + keep_recent 条 append。**历史消息仍在 derive_messages 输出中**，语义不清（模型同时看到旧历史和摘要）
- **影响**：压缩后模型上下文仍包含被压缩的历史，压缩没真正减小上下文。这是 Lite 与上游最大的语义偏差
- **工作量**：**M**（~80-120 行）— 给 `SessionEvent` 加 `CompactionReplace{summary, shadowed_start, shadowed_end}` 变体，derive_messages 跳过被 shadowed 的 seq 区间。保留 append-only log，只改投影逻辑

### 4. 原子写入 Windows 重试 + session-index 原子写
- **上游变更**（commit `3e56eaaa0f`）：Windows 上 `rename` 可能因文件被占用暂时失败，增加重试
- **Lite 现状**：
  - `src/session.rs:294` checkpoint 做 temp+fsync+rename，但 rename 失败不重试
  - `src/session_manager.rs:319` 的 `save_index` 用 `std::fs::write` 直接写入（**非原子**，崩溃可截断索引）
  - `src/memory.rs:165` 也用 `std::fs::write` 直接写入
- **影响**：Windows 杀毒软件扫描时 checkpoint rename 失败丢数据；session-index 崩溃时被截断
- **工作量**：**S** — rename 加 3 次重试 + 100ms 间隔；save_index 和 memory 也走 atomic-rename

### 5. 会话日志截断自动修复（逻辑层）
- **上游变更**（v0.1.2-rc.1）：物理层 JSONL torn 尾修复 + 逻辑层 `interruptedTurnClosers`（扫描日志找 open turn，未配对 tool-call → 合成 error tool-result，补 `turn/end{reason:interrupted}`）
- **Lite 现状**：checkpoint 整文件 atomic rename，**无物理 torn 尾问题**。但**无逻辑层修复**：崩溃时若停在 open turn（tool-call 后无 tool-result），resume 后 `derive_messages` 返回 dangling tool-call，模型收到不合法 transcript
- **影响**：崩溃恢复后 transcript 不合法，模型可能报错或行为异常
- **工作量**：**S**（~40-60 行）— 在 `deserialize/load` 后检查最后一个 turn 是否未闭合，合成 `TurnEnd{reason:Aborted}` + 未配对 tool-call 的 error result

### 6. 提示词分区顺序稳定化（防御性）
- **上游变更**（commits `fdf60301f2`, `25428f8e08`, `43ac97b554`）：修复 workflow 分区顺序不稳定，集中管理 section orders
- **Lite 现状**：`src/prompt.rs:156` 用 `sort_by_key(|s| s.order)`，5 个 section order 值各不相同（-100, -90, -80, 0, 10），**当前无此问题**。但未来如果 skill/插件添加同 order 值的 section，可能出现顺序不稳定
- **影响**：当前无，防御性
- **工作量**：**S** — sort key 中加 name 作为 tie-breaker

### 7. 文件编辑工具接受 null 占位值
- **上游变更**（v0.1.2-rc.1）：文件编辑工具未使用字段接受 `null` 占位
- **Lite 现状**：`src/tools/file.rs` 的 `file_write`，如果模型发 `null` 作为可选参数，可能解析失败
- **影响**：某些模型在可选参数发 `null` 而非省略
- **工作量**：**S** — 检查 file_write 参数解析，对 null 做容错

### 8. 压缩计入图片占用
- **上游变更**（v0.1.2-rc.1）：compaction 时计入图片 token。上游用 route 定价 + DeepSeek v4 图片计价器
- **Lite 现状**：`src/compaction.rs:44` — `estimated_tokens = message_count * 200`，完全不计图片。一张 4K 截图可能数千 token
- **影响**：多图会话上下文溢出但 compaction 不触发
- **工作量**：**S** — 给 `ImageBlock` 加 `fn estimated_tokens()`（保守估计，如 base64 长度/4 或固定 384），在 `needs_compaction` 中计入

### 9. Compaction 内存防重入标志
- **上游变更**：compaction 有 durable lock bracket（`compaction/start` + `compaction/end`）
- **Lite 现状**：`compaction.rs` 无防重入，理论上异步 summarize 期间可能再触发一次
- **影响**：对同一段历史做两次摘要
- **工作量**：**S**（<10 行）— 加 `compacting: bool` 内存标志

### 10. SessionSnapshot 加 version 字段
- **上游变更**：Session format 有显式 version（v0→v1→v2 迁移链）
- **Lite 现状**：`SessionSnapshot` 无版本号，deserialize 靠"先试 JSON 再试 bincode"兜底
- **影响**：未来格式变更无法检测旧版本
- **工作量**：**S** — 加 `version: u64` 字段，旧数据 version=0，fail-closed 而非 bincode 静默兜底

### 11. 模型探测健壮性改进
- **上游变更**：`GET /v1/models` 探测，bounded read、401/403 → "check API key" 提示、JSON 解析容错
- **Lite 现状**：`server.rs::fetch_models_blocking` 已做 `GET /v1/models`，但无 bounded read、无 401/403 提示
- **影响**：API key 错误时报 "unreachable" 而非 "check API key"
- **工作量**：**S** — 加 bounded read + 401/403 提示

### 12. 流式请求实际重试/退避 ★发现隐藏 bug
- **上游变更**：agent-loop 的 `step()` 有 `while(true)` 重试循环 + retryPolicy（rc.8 就有，未变）
- **Lite 现状**：`src/llm.rs:12` 文档注释声称"retries on transient network errors with simple exponential backoff"，但 `do_stream_request` **实际不重试**——连接/握手错误直接返回失败（lines 444-471）。**文档与实现不符**
- **影响**：网络抖动（常见于 SSH 隧道/跳板机场景）直接导致对话中断，无重试
- **工作量**：**S** — 在 `do_stream_request` 加 3 次重试 + exponential backoff（100ms/400ms/1600ms）

### 13. Turn 取消/中断机制 ★高价值
- **上游变更**（v0.1.3-alpha.1）：`Agent.cancel(cause)` 中断当前 turn，stream loop 每个 chunk 检查 `signal.throwIfAborted()`，中断内容保留为 `interrupted:true` 的 assistant message
- **Lite 现状**：**无取消机制**。`run_turn`/`run_llm_step` 和 `handle_chat` 无法中断进行中的 turn。但 `TurnEndReason::Aborted` **已定义但未使用**（grep cancel/abort 在 loop 中无结果）
- **影响**：模型卡在长输出时用户无法中断，必须等完成或杀进程
- **工作量**：**M** — `run_llm_step` 的 stream loop 加 `CancellationToken` 检查 + `/api/cancel` 端点 + 前端中断按钮。已有 mpsc channel 和 `TurnEndReason::Aborted`，基础设施已有

### 14. 确定性工具排序
- **上游变更**：rc.8 已有规范排序（alphabetical + config override），本窗口无变化
- **Lite 现状**：`ToolRegistry::definitions()` 用 `HashMap` 迭代，**工具顺序非确定性**——每次启动 prompt 中工具顺序可能不同，影响 KV cache 前缀稳定性
- **影响**：工具顺序不稳定 → system prompt 中 tool guidance section 不稳定 → KV cache 命中率受影响
- **工作量**：**S** — `definitions()` 按 name 排序

---

## 三、🔍 需要讨论（由用户决定是否跟进）

### 15. HTTPS + TLS 支持
- **上游变更**（v0.1.3-alpha.1）：所有出站请求遵循代理环境变量。上游用 Node `fetch` + undici，Lite 用 `hyper` raw TCP
- **Lite 现状**：`src/llm.rs:439-440` — **不支持 HTTPS**，只支持 HTTP 连本地推理。`Cargo.toml` 用 `hyper`/`hyper-util`（**不是 reqwest**）
- **跟进的利**：连接远程 API（如 `api.deepseek.com`）；通过跳板机/代理访问设备
- **跟进的弊**：需要引入 TLS 依赖（`rustls` + `tokio-rustls`，纯 Rust 无 OpenSSL），增加二进制体积 ~1-2 MB
- **我的建议**：**跟进**。网络设备运维经常需要远程访问，HTTPS 是前提
- **工作量**：**M**

### 16. 代理支持（HTTP_PROXY/HTTPS_PROXY）
- **上游变更**（v0.1.3-alpha.1）：解析环境变量，安装 undici 全局 dispatcher，NO_PROXY 合并 loopback
- **Lite 现状**：零代理感知
- **依赖**：**依赖 #15 HTTPS 先落地**
- **我的建议**：**跟进**，但排在 HTTPS 之后。HTTP 代理简单（发 absolute-form 请求），HTTPS 需要 CONNECT 隧道
- **工作量**：**M**

### 17. 图像预处理（缩放 + 格式转换）
- **上游变更**（v0.1.1-rc.2）：sharp 做 resize/format/color 转换，pixel budget（默认 640k px）+ byte budget（1 MiB）
- **Lite 现状**：直接发 base64 原图，无预处理、无大小限制
- **跟进的利**：避免大图超限（4K 截图 base64 后可能 >5MB）；减少 token 消耗
- **跟进的弊**：需引入 `image` crate（~500KB），增加二进制体积，与 Lite 的 `opt-level="z"` + `lto` + `strip` 体积优化冲突
- **我的建议**：**跟进**。网络设备截图经常 4K，不处理直接失败。用 `image` crate 做 max 2048px 缩放 + JPEG 转换
- **工作量**：**M**

### 18. read_image 工具（工具返回图片块）
- **上游变更**：`read_image` 工具读取图片文件，返回 `image` content block（非纯文本），块进入模型上下文
- **Lite 现状**：无 `read_image` 工具。`Message::Tool { content: String }` 结构上不能携带图片。`file_read` 只返回文本
- **注意**：这**不是纯 UI**变更，是协议层缺口
- **跟进的利**：模型可以读取本地图片文件分析（如设备截图）
- **跟进的弊**：需要扩展 `Message::Tool` 携带图片块 + `llm.rs` 从 tool result 发 image content parts
- **我的建议**：**待议**。取决于是否有"读图片文件→分析"的工作流需求
- **工作量**：**M-L**

### 19. 子代理模型选择
- **上游变更**（v0.1.2-rc.1）：Agent 自选或调用方指定 provider/model/reasoning/max_tokens。opt-in via config + allowlist，`list_subagent_models` 发现工具
- **Lite 现状**：子代理用父代理相同 `model_config`（`subagent.rs:199-209`），无 per-delegation route
- **我的建议**：**待议**。最小实现：subagent 工具加 optional `provider`/`model`/`reasoning_effort` 参数，路由到匹配 preset（S）。完整 allowlist + settings 是 M
- **工作量**：**S**（仅参数路由）/ **M**（含 allowlist）

### 20. 回答末尾显示 token 用量和耗时
- **上游变更**（v0.1.2-rc.1）：回答末尾显示 token 用量 + 耗时，可展开详细统计
- **Lite 现状**：底部状态栏显示缓存命中率，但不在回答末尾显示
- **我的建议**：**跟进**。已有 `ApiUsage` 数据，只需前端展示
- **工作量**：**S**

### 21. 连接状态显示 + 断线自动重试
- **上游变更**（v0.1.2-rc.1）：界面显示连接状态，断线自动重试/重连。后端 retry loop 在 rc.8 就有
- **Lite 现状**：后端**文档声称有重试但实际没有**（见 ✅ #12），前端无连接状态
- **我的建议**：**部分跟进**。后端重试已在 ✅ #12。前端补连接状态指示器 + SSE 断线自动重连
- **工作量**：**M**

### 22. `thinking` 顶层 wire 字段
- **上游变更**：`serialize.ts` 同时发 `thinking:{type:'enabled'|'disabled'}` 和 `reasoning_effort`
- **Lite 现状**：只发 `reasoning_effort`（Off 时省略）
- **我的建议**：**待议**。`reasoning_effort` 是 OpenAI 兼容标准拼写，已可用。`thinking` 顶层字段仅在内网端点需要
- **工作量**：**S**

### 23. `reasoning_tokens` 用量字段
- **上游变更**：从 `completion_tokens_details.reasoning_tokens` 读取思考 token
- **Lite 现状**：`ApiUsage` 无此字段
- **我的建议**：**跟进**。如果要在前端展示思考 token 消耗
- **工作量**：**S**

### 24. EMPTY_RESPONSE 空响应守卫
- **上游变更**：`stop` finish + 零 block → `EMPTY_RESPONSE` 错误（可重试）而非成功空消息
- **Lite 现状**：返回 `Done { content: "" }` 记录空 assistant message
- **我的建议**：**跟进**（低优先）。某些网关静默完成时有用
- **工作量**：**S**

### 25. 多模态视觉模型支持（DeepSeek-V4-Flash-Vision-Exp）
- **上游变更**（v0.1.1-rc.1）：新增视觉模型 + modality gating
- **Lite 现状**：已支持 `image_url` content parts，但无模型目录/modality gating
- **我的建议**：**跟进**（如果用该模型）。图片格式已是 OpenAI 标准，大概率兼容。加 preset + modality flag 即可
- **工作量**：**S**（仅 preset + flag）/ **M**（含 budget gating）

### 26. 父子代理双向 send_message + 持续子代理
- **上游变更**（v0.1.2-rc.1 + v0.1.3-alpha.1）：`report` 工具删除，改为 `send_message` 双向通信；steer 语义（运行中子代理在最近 step boundary 接收消息）；`interrupt` = `cancel(keepInbox:true)`
- **Lite 现状**：子代理前台同步运行，只返回最终输出，无持续会话，无双向通信
- **我的建议**：**跳过**（当前阶段）。需要根本性架构变更（持久化子代理 + Inbox + steer 语义）。等 Lite 有 Agent Team 需求时再做
- **工作量**：**L**

### 27. Assistant-stream 重构（attempt 级持久化）
- **上游变更**：live `agent/assistant-stream` frames + durable `assistant/attempt` 记录嵌入压缩流。支持 live replay、attempt 展示、中断内容保留
- **Lite 现状**：直接 stream `LoopEvent::Delta`，只持久化最终 `AssistantMessage`
- **我的建议**：**跳过**。与 Lite 嵌入式单进程范围一致，除非需要可重放的实时轨迹
- **工作量**：**L**

---

## 四、⏸️ 暂不跟进 / ❌ 不适用

| # | 变更 | 跳过原因 |
|---|------|---------|
| 28 | Session format v0/v1/v2 迁移链 | 过度设计，Lite 无历史格式需兼容 |
| 29 | SessionHandle / 跨进程锁 | Lite 设计为单进程单活跃 |
| 30 | Session 按需读取 API (snapshotEvents) | Lite 会话短，全量加载无性能问题 |
| 31 | session-turn-outline | Lite 无分页历史需求 |
| 32 | Files API 上传/复用/索引 | L 工作量，inline base64 已够用 |
| 33 | Session 日志增量上传 | 隐私风险，会泄露设备 CLI/config dump |
| 34 | 插件包名/版本发送 | Lite 无插件系统 |
| 35 | 插件提供方登录配置 | Lite 无插件 |
| 36 | 第三方语言 i18n | Lite 中文单文件前端 |
| 37 | 实验性 Inspector / Web Preview | 不适用 |
| 38 | ACP / MCP | Lite 没有 |
| 39 | Python SDK / Node.js 24 兼容 | Lite 纯 Rust |
| 40 | 持久 PowerShell/Bash / PTY | Lite 没有 |
| 41 | Bubblewrap 沙箱 | Lite 没有沙箱 |
| 42 | SQLite 后端移除 | Lite 从未用 SQLite |
| 43 | WebFetch / web_search | Lite 没有 web 工具 |
| 44 | Web PTC Mode | Lite 没有 |
| 45 | Agent Preset / Agent Team | Lite 没有团队 |
| 46 | 父子代理双向 send_message + steer | L 工作量，需根本性架构变更（见 🔍 #26） |
| 47 | Assistant-stream 重构 | L，与 Lite 范围一致（见 🔍 #27） |
| 48 | Agent-loop 插件生命周期硬化 | L，Cordis 插件基础设施 Lite 不需要 |
| 49 | 会话流折叠/字号/表格/代码高亮/导航 | UI 特性，Lite 前端简单 |
| 50 | 草稿感知发送/消息排队 | UI 特性 |
| 51 | `/` `@` 菜单改进 | Lite 前端无此功能 |
| 52 | 一次性 token 认证 | Lite 绑定本地 |
| 53 | 定时计划显示 | Lite 没有定时计划 |
| 54 | Skill 选择器模糊搜索 | UI 特性 |
| 55 | 任意类型文件上传 | Lite 是文本+图片 |
| 56 | 模型探测自定义 provider | Lite 是 DeepSeek 专用 |
| 57 | Session format v2 / SessionHandle（v0.1.3-alpha.1） | 破坏性变更，下次同步评估 |

---

## 五、总结

| # | 变更 | 分类 | 工作量 | 建议 |
|---|------|------|--------|------|
| **1** | **流式工具调用空值覆盖修复** | **✅** | **S** | **立即做，最高优先** |
| 2 | `cached_tokens` 别名 | ✅ | S | 立即做 |
| **3** | **derive_messages surface-replace** | **✅** | **M** | **核心价值，立即做** |
| 4 | 原子写入重试 + index 原子写 | ✅ | S | 立即做 |
| 5 | 截断逻辑修复 interruptedTurnClosers | ✅ | S | 立即做 |
| 6 | 提示词分区顺序稳定化 | ✅ | S | 防御性修复 |
| 7 | 文件编辑 null 占位 | ✅ | S | 立即做 |
| 8 | 压缩计入图片占用 | ✅ | S | 立即做 |
| 9 | Compaction 防重入标志 | ✅ | S | 立即做 |
| 10 | SessionSnapshot version 字段 | ✅ | S | 立即做 |
| 11 | 模型探测健壮性 | ✅ | S | 立即做 |
| **12** | **流式请求实际重试（文档声称有但实际没有）** | **✅** | **S** | **立即做，隐藏 bug** |
| **13** | **Turn 取消/中断机制** | **✅** | **M** | **高价值，已有 Aborted 枚举** |
| **14** | **确定性工具排序** | **✅** | **S** | **影响 KV cache 稳定性** |
| 15 | HTTPS + TLS | 🔍 | M | 建议跟进 |
| 16 | 代理支持 | 🔍 | M | 建议（排在 HTTPS 后） |
| 17 | 图像预处理 | 🔍 | M | 建议跟进 |
| 18 | read_image 工具 | 🔍 | M-L | 待议 |
| 19 | 子代理模型选择 | 🔍 | S-M | 待议 |
| 20 | 回答末尾 token 用量 | 🔍 | S | 建议跟进 |
| 21 | 连接状态 + 断线重试 | 🔍 | M | 建议部分跟进 |
| 22 | `thinking` 顶层字段 | 🔍 | S | 待议 |
| 23 | `reasoning_tokens` 字段 | 🔍 | S | 建议跟进 |
| 24 | EMPTY_RESPONSE 守卫 | 🔍 | S | 低优先跟进 |
| 25 | 多模态视觉模型 | 🔍 | S-M | 看是否用该模型 |
| 26 | 父子代理双向 send_message | 🔍 | L | 建议跳过（当前） |
| 27 | Assistant-stream 重构 | 🔍 | L | 建议跳过 |
| 28-57 | ACP/MCP/插件/i18n/UI/等 | ⏸️ | - | 不适用 |

**建议执行顺序**：
1. **第一批（S，全部 ✅，约 4h）**：#1 #2 #4 #5 #6 #7 #8 #9 #10 #11 #12 #14 — 全是 bug fix / 健壮性改进
2. **第二批（M，核心，约 5h）**：#3 surface-replace + #13 turn 取消 — 两个核心功能改进
3. **第三批（S-M，用户审批后，约 3h）**：#20 #23 — 前端展示 + 数据字段
4. **第四批（M，新依赖，约 6h）**：#15 HTTPS + #16 代理 — 需要引入 TLS 依赖
5. **第四批并行（M，约 2h）**：#17 图像预处理 — 需要引入 `image` crate
6. **第五批（M，约 2h）**：#21 连接状态 + SSE 重连
7. **待议**：#18 #19 #22 #24 #25 — 看需求
8. **跳过**：#26 #27 — 架构变动太大
