# DSH 上游 v0.1.0-rc.8 ~ v0.1.1-rc.2 变更分析报告

> 对比范围：`99f6f02fec`（rc.7）→ `b150a551b8`（v0.1.1-rc.2，upstream/master 最新）
> 非合并提交：479 个
> 跨越版本：rc.8 → v0.1.1-rc.1 → v0.1.1-rc.2

---

## 一、官方变更说明

### v0.1.0-rc.8

**新增功能**
- 多模态支持：DeepSeek 适配器支持配置启用原生图片请求，`/goal`、`/plan` 等命令可接收图文输入，`@` 菜单支持引用文件和会话
- Claude Code 与 Codex 子代理可作为 Profile Bundle 按需安装，支持非交互权限模式和多个命名实例
- Windows PTY 终端支持持久 PowerShell 会话

**问题修复**
- 修复图片尺寸过大或历史图片累计载荷过高导致模型请求失败
- 修正取消流式生成后已展示的回复前缀未带入后续提问和分叉会话
- **修复部分自定义 OpenAI 兼容网关因请求格式差异无法调用，以及推理内容回传可能缺失问题**

**体验优化**
- 布局优化（`~` 缩写、窄屏布局、反馈界面）
- 工具调用优化（`web_search` 并发查询、子代理 `reportDelivery` 及时反馈）
- 大历史会话分叉性能改善

**其他**
- SQLite 后端读写与分叉性能改善（数据结构不兼容）
- 品牌使用规范

### v0.1.1-rc.1

**新增**
- DeepSeek 适配器新增多模态视觉理解模型 `DeepSeek-V4-Flash-Vision-Exp`

**修复**
- 修复 `@` 引用前增删改文本时的布局问题
- 修复 Bubblewrap 沙箱 `/proc/<pid>/root` 绕过

**优化**
- Markdown 表格自适应、缓存命中率精度显示、子代理会话标题切换
- **`ask_user_question` 回答内容支持多行输入、自动换行、`Shift+Enter` 换行**

### v0.1.1-rc.2

**优化**
- DeepSeek 适配器支持优先通过 Files API 上传图像，可复用已上传文件
- 图像预处理流程优化：自动缩放并转换格式

---

## 二、逐项分析：跟不跟进

### ✅ 应该跟进

#### 1. `reasoning_content` 回传规则修正（关键！）

**上游变更**：`583894f7ae` — fix(llm-deepseek): pass reasoning content back on every reasoned turn

rc.7 的规则是：reasoning_content 只在 tool-call 轮次回传。
rc.8 修正为：**每个包含 reasoning 的轮次都回传**（不论是否有 tool calls）。

原因：某些 OpenAI 兼容网关会重新编码对话给其他厂商，需要通过 reasoning_content 的哈希来恢复思维链签名。如果 tool-call-free 轮次不回传，网关就丢失了那轮的 thinking 签名。

新规则代码：
```typescript
// CoT passback on every reasoning-carrying turn.
...reasoning.length > 0 ? { reasoning_content: reasoning } : {},
...toolCalls.length > 0 ? { tool_calls: toolCalls } : {},
```

**Lite 现状**：我们在 rc.7 同步时实现了「只在 tool-call 轮次回传」的规则：
```rust
let reasoning = if !tool_calls.is_empty() { thinking.clone() } else { None };
```

**影响**：Lite 在使用自定义 OpenAI 兼容网关时可能丢失 thinking 签名。对于直连 DeepSeek API 的情况，官方 API 在非 tool-call 轮次忽略 reasoning_content，所以不影响功能，但回传也不会有害。

**建议**：改为「只要有 reasoning 就回传」，与上游对齐。改动一行代码。

**工作量**：极小。

---

#### 2. 缓存命中率精度显示

**上游变更**：v0.1.1-rc.1 — 缓存命中率在 99.x% 时的精度显示

**Lite 现状**：`web/index.html` 第 1763 行：
```javascript
const cachePct = Math.round(sessionStats.cacheHitTokens / billedInput * 100);
```
`Math.round` 将 99.7% 取整为 100%，误导用户。

**建议**：改为 `.toFixed(1)`，保留一位小数。

**工作量**：极小。一行 JS 改动。

---

### ⏸️ 暂不跟进（但已深入分析）

#### 5. 多模态图片支持

**上游变更**：rc.8 新增 DeepSeek 适配器原生图片请求 + v0.1.1-rc.1 新增 Vision 模型

**上游实现**（已验证 `git show upstream/master:packages/llm/llm-deepseek/src/serialize.ts`）：

rc.8 起 DeepSeek chat-completions 适配器**原生支持图片**，两条路径：
1. **`file_id`** — Files API 上传（`files-api.ts` 257 行 + `file-store.ts` 331 行），服务端存储，复用已上传文件，7 天有效期
2. **`image_url`** — inline base64 data URL（`data:image/jpeg;base64,...`），Files API 失败时回退

适配器路由逻辑：
```typescript
const hasImages = options.messages.some(message => contentHasImage(message.content))
if (!hasImages) {
  body = serializeRequest(requestOptions, connection.defaults)  // text-only, assertTextOnly()
} else if (representation === 'base64') {
  body = await serializeRequestWithImages(requestOptions, { representation: { kind: 'base64' } })
} else {
  body = await serializeRequestWithImages(requestOptions, { representation: { kind: 'file', ... } })
}
```

Wire 格式 = OpenAI content parts 数组：
```json
{ "role": "user", "content": [
  { "type": "text", "text": "看这张图" },
  { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,..." } }
]}
```
或 file_id 路径：`{ "type": "file", "file_id": "..." }`

配置项：`imagePixelBudget`、`imageMaxBytes`、`imageDetail`（auto/low）、`maxImagesPerRequest`、`maxInlineRequestImageBytes`
附件存储：内容寻址（SHA-256），去重，原子写入（temp→fsync→rename→fsync dir）
图片预处理：自动缩放 + 格式转换，根据模型要求适配

**Lite 现状**：
- 纯文本交互，无图片能力
- `src/llm.rs` 的 `ApiMessage` 只有 `content: String`，不支持 content blocks
- 前端 `index.html` 无图片上传/粘贴 UI
- **但**：Lite 用的是同一套 DeepSeek chat-completions API，wire 格式直接兼容

**如果要做，需要改动**：
1. `src/types.rs` — `Message::User` 的 `content` 从 `String` 改为 `ContentBlock` 枚举（`Text(String)` | `Blocks(Vec<ContentPart>)`），或新增 `images: Vec<ImageBlock>` 字段
2. `src/llm.rs` — `ApiMessage` 序列化为 OpenAI content parts 数组格式（当有图片时）
3. `src/server.rs` — 接收前端上传的 base64 图片，存入 session log
4. `web/index.html` — 图片上传/粘贴 UI + 图片消息渲染
5. 图片预处理（缩放、格式转换）—— 需要 `image` crate（~200KB，增加二进制体积）
6. 可选：Files API 客户端（减少重复上传的 token 消耗）

**建议**：**用户确认需要支持图片**。建议作为 rc.8 的独立迭代（`rc.8.01`）分步实施：
- 第一步：base64 inline 图片（最简路径，不需要 Files API，OpenAI content parts 格式直接兼容）
- 第二步：图片预处理（缩放/格式转换，用 `image` crate）
- 第三步（可选）：Files API 上传 + 复用（减少 token 消耗，但增加 HTTP 客户端复杂度）

**工作量**：中偏大。涉及 types + llm + server + 前端 + 可能新 crate 依赖。

---

#### 7. Files API 图像上传

**上游变更**：v0.1.1-rc.2 — 优先通过 Files API 上传图像，可复用已上传文件

**上游实现**：
- `files-api.ts`（257 行）— DeepSeek Files API 客户端：上传、查询、删除
- `file-store.ts`（331 行）— 本地索引：按端点 + API key + variantId 作用域，记录 file_id + expires_at
- 默认请求 7 天有效期，剩余 <1 小时时重新上传
- 上传失败时自动回退到 base64 inline
- 限制：单文件 128MiB、chat 单图 32MiB、每 key 最多 10,000 文件 / 25GiB

**Lite 现状**：无

**建议**：**用户确认需要**。作为图片支持的第二/三步实施。base64 inline 是基础路径，Files API 是优化路径（减少重复 token 消耗）。如果网络设备运维场景中图片量不大（截图诊断），base64 inline 足够。

**工作量**：中。如果只做 base64，不需要此功能。

---

#### 9. 持久化 Shell 会话

**上游变更**：rc.8 — Windows PTY 持久 PowerShell + `tool-bash-persistent`

**上游实现**：
- `tool-bash-persistent` — 通过 PTY 维持一个持久 bash 进程
- 状态保持：`cd`、环境变量、工作目录跨命令持久
- 交互式工具支持：`top`、`vi` 等
- 输出截断 + 滚动缓冲管理

**Lite 现状**：
- `src/tools/shell.rs` — 每次 `sh -c "command"` 独立进程，无状态保持
- `src/tools/ssh.rs` — **已支持持久会话**，通过 `russh` 维持连接池，复用连接
- 网络设备运维主要走 SSH（已持久化），本地 shell 用于辅助操作

**分析**：
- Lite 的 SSH 工具已经持久化，网络设备运维的主要路径已覆盖
- 本地 shell 单次执行足够：`cd /dir && ls` 可以用 `&&` 连接
- 持久 shell 的价值：交互式工具、长时间运行进程、复杂环境变量
- 网络设备场景下这些需求不常见
- 实现持久 shell 需要引入 PTY 库（`portable-pty`），增加复杂度

**建议**：**暂不跟进**。SSH 已持久化，本地 shell 单次执行满足需求。如果未来有交互式工具需求再考虑。

**工作量**：大（需引入 PTY 库 + 进程管理 + 输出解析）。

---

#### 10. SQLite 后端 vs JSON 持久化

**上游变更**：rc.8 — SQLite 后端读写/分叉性能优化（数据结构不兼容）

**上游实现**：
- `session-persistence-sqlite/` — 可选后端（**JSONL 是默认后端**）
- 1 行 per 事件，schema version 17（打包连续 chunk）
- WAL 模式，`synchronous=FULL`
- 优化后基准（105 sessions, 2.5M events）：SQLite 75MB / 8.58s 写 / 13.10s fork；JSONL 30.65MB

**Lite 现状**：
- `src/session.rs` — 环形缓冲区（512 events 硬编码），checkpoint = 全量重写 ≤150KB 文件
- `src/session_manager.rs` — bincode 索引 + 双页缓存，最多 2 个 session 在内存
- 100+ sessions ≈ 5-20MB flash，≤2 logs in RAM
- 纯 Rust 依赖（musl 交叉编译，零额外工具链）

**分析**：
| SQLite 优势 | 对 Lite 适用？ |
|---|---|
| 可定位后缀读取 | 不适用 — Lite 加载整个有界文件 |
| 无界日志压缩 | 不适用 — 环形缓冲区有界 |
| FTS5 跨会话搜索 | 不适用 — 明确不在范围内 |
| 多会话索引查询 | 不适用 — 内存元数据排序足够 |
| Fork 性能 | 不适用 — Lite 没有 fork |

| SQLite 代价 | 影响 Lite？ |
|---|---|
| `libsqlite3-sys`（C 依赖） | **是 — 破坏纯 Rust 交叉编译** |
| ~1.5MB 二进制增量 | **是**（当前 ~2.67MB） |
| Schema 版本/迁移机制 | **是 — 不需要** |
| 失去 `#[serde(default)]` 前向兼容 | **是** |

**建议**：**不迁移 SQLite**。Lite 的环形缓冲区 + bincode + JSON 模式完全匹配其有界、嵌入式、纯 Rust 的设计目标。SQLite 的 C 依赖会破坏 musl 交叉编译，且优势在有界日志场景下不适用。

**但有一个应修复的 durability gap**：`session.rs` 的 `checkpoint()` 直接 `std::fs::write`，没有原子写入（temp + rename）。一次中断的写入会损坏当前文件。建议改为：写入临时文件 → `fsync` → rename 覆盖。

**工作量**：SQLite 迁移 = 大（且不建议）。原子写入修复 = 极小。

---

#### 11. 沙箱安全

**上游变更**：v0.1.1-rc.1 — 修复 Bubblewrap `/proc/<pid>/root` 绕过

**上游实现**：
- Linux 链路：`bwrap` → `landlock-run`（~300 行 C11，musl 静态，fail-closed）
- macOS：Seatbelt
- Windows：ACL restricted-token
- **无 seccomp**，仅文件效果限制
- 沙箱修复：bwrap 挂载新 `/proc` 但保留宿主 PID 命名空间 → `/proc/<pid>/root` 逃逸。修复 = `--unshare-pid`

**Lite 现状**：
- `src/tools/shell.rs` — `std::process::Command` 直接执行，无沙箱
- Agent 可以在设备上执行任意 shell 命令
- 运行在嵌入式 Linux（musl 静态）和 Windows 上

**分析**：
- 网络设备威胁模型：主要风险不是文件逃逸，而是 **网络重配置、`reboot`、进程 kill、固件刷写**
- Landlock 仅限制文件效果，不覆盖网络/进程操作 — 不匹配主要威胁
- Landlock 需要内核 ≥5.13，嵌入式设备内核常 older — 可能不可用
- bwrap 需要 userns/mount 前提条件，嵌入式设备常缺失
- seccomp 粒度不对，有 brick 风险

**上游最可迁移的教训**：fail-closed + per-call-policy + 敏感操作人工审批的**纪律**，而不是沙箱技术本身。

**建议**：**不加内核沙箱**。改为实现**命令策略层**：
1. 可配置的命令白名单/黑名单（`config/default.yaml` 新增 `shell.policy` 节）
2. 敏感操作审批门（reconfigure-class 操作需用户确认）
3. 基于上游 `tools/pre-execute` + `dsh-user-approval` 的纪律，而非 `ctx.sandbox`

**工作量**：中。配置层 + 命令检查 + 前端审批 UI。

---

#### 14. UI 优化详细对比

上游 rc.8 ~ v0.1.1-rc.2 的 UI 优化逐项对比（已通过 `git show <commit>` 验证每个上游变更）：

| # | 上游优化 | 上游实现 | Lite 现状 | 需要？ | 工作量 |
|---|---------|---------|----------|--------|--------|
| a | `~` 缩写 home 目录 | `abbreviateHomePath()` 在工具调用行/读取卡片/工作区路径中缩短 `$HOME` 为 `~`，POSIX-only | 工具调用参数原样渲染 `truncate(args,60)`，无缩写；前端不知道 home 目录 | ⏸️ 低价值 | 中（需加 `/api/context` 暴露 home + JS helper） |
| b | 窄屏输入框布局 | `.row{flex-wrap:wrap}` + `.trailing{margin-left:auto}`，模型+发送组换行到第二行 | `.input-row` 是 `flex;justify-content:flex-end`，**无 `flex-wrap`**，无 `@media` 断点 | ✅ 是 | 极小（加一行 `flex-wrap:wrap`） |
| c | 反馈界面 | note editor 从溢出行内 span 改为 `position:fixed` popover | Lite 无反馈/笔记功能 | ❌ 不适用 | N/A |
| d | 侧栏搜索焦点响应 | 点击折叠栏搜索按钮展开侧栏并聚焦输入框 | **已有等效实现**：`btn-search-rail` + `expandSidebarAndFocusSearch()` + `setTimeout(focus,200)`。无 outside-click 监听器，不存在上游修复的竞争问题 | ❌ 已有 | 无 |
| e | 工作流面板操作 | 运行/阶段折叠变为用户可切换，`DisclosureMode` 状态机自动展开/折叠 | Lite 无工作流功能 | ❌ 不适用 | N/A |
| f | 模型选择器选中操作 | 新增"全选/取消全选"ghost 按钮用于候选 checkbox 列表 | Lite 的模型配置是 per-provider 单选 `<select>`，结构不同 | ❌ 不适用 | N/A |
| g | 本地文件打开失败重试 | 失败的 `openPath` 弹出 Modal + Retry 按钮 | Lite 聊天中无"打开路径"操作，工具结果是折叠卡片 | ❌ 不适用 | N/A |
| h | Markdown 表格自适应 | <4 列填满列宽并换行；≥4 列保持自然宽 + `md-table-wide` + 容器查询突破 748px；滚动条悬停显示 | `.bubble table{display:block;overflow-x:auto;width:100%}`，所有表格横向滚动，滚动条常显 | ✅ 是 | 小（CSS + renderMd 加列数判断） |
| i | 缓存命中率精度 | `cacheHitPercent` 返回字符串，整数%会到 100 时加小数精度（`99.5%`） | **有完全相同的 bug**：`Math.round(hit/billed*100)` → 99.5% 显示为 100% | ✅ 是 | 极小（一行 JS） |
| j | 子代理会话标题切换 | 面包屑标题变为切换下拉，列出同级子代理会话树 | Lite 无子代理 UI | ❌ 不适用 | N/A |
| k | `ask_user_question` 多行 | `AnswerField` = `<textarea rows=1>` + 隐藏 mirror 控制高度，自动增高到 8 行，Enter 提交 Shift+Enter 换行 | **Lite 完全没有此工具**（已验证：`src/*.rs` 无 `ask_user` 匹配，`index.html` 无相关 UI） | ❌ 不适用 | N/A |

**建议跟进的 UI 项**（3 项，都是小改动）：
- **b. 窄屏布局** — `.input-row` 加 `flex-wrap:wrap`（一行 CSS）
- **h. Markdown 表格** — 按列数加 `wide` class + 悬停显示滚动条（CSS + 小 JS）
- **i. 缓存命中率精度** — 已在 ✅ 项 #2 中列出

**不跟进**：`~` 缩写（POSIX-only + 需服务端 API，低价值）；其余 7 项 Lite 无对应功能

**工作量**：小。纯前端 CSS/JS 改动。

---

#### 2. 取消流式生成保留前缀

**上游变更**：`48d8e2f8f5` + `87f24bb991` — agent-loop cancelled stream prefix finalize

**Lite 现状**：无取消功能（前端无取消按钮）。

**建议**：暂不跟进。等 Lite 加取消功能时再实现。记录此设计。

---

#### 3. `ask_user_question` 多行输入

**上游变更**：`9616790b6c` — feat(web): answer ask_user_question over multiple lines

**Lite 现状**：Lite **没有 `ask_user_question` 工具**。当前工具集：shell, file_read, file_write, file_search, memory_read, memory_write, memory_recall, ssh_exec, subagent。

**建议**：暂不跟进此 UI 改进。如果未来 Lite 加入 `ask_user_question` 工具，再一并实现多行输入。

---

#### 6. DeepSeek-V4-Flash-Vision-Exp 模型

**原因**：视觉模型，取决于是否做图片支持（项 5）。如果项 5 推进，则一并跟进模型配置。

---

#### 8. 子代理 Profile Bundle

**原因**：Lite 的 SubagentTool 是轻量级单进程委托，没有 Profile Bundle 安装机制。不适用。

---

#### 12. `@` 菜单引用文件和会话

**原因**：Lite 前端是单文件 `index.html`，没有 `@` 引用系统。如果做图片支持（项 5），可考虑加文件引用 UI。

---

#### 13. `web_search` 并发查询

**原因**：Lite 没有 web search 工具。不适用。

---

## 三、总结

| # | 项目 | 优先级 | 工作量 | 跟进 |
|---|------|--------|--------|------|
| 1 | reasoning_content 回传规则修正 | **高** | 极小 | ✅ 立即 |
| 2 | 缓存命中率精度显示 | 低 | 极小 | ✅ 顺手 |
| 5 | 多模态图片支持 | **高** | 中偏大 | ✅ 用户确认需要 |
| 7 | Files API 图像上传 | 中 | 中 | ✅ 图片支持第二步 |
| 9 | 持久化 Shell 会话 | 低 | 大 | ⏸️ SSH 已持久化 |
| 10 | SQLite 后端 | — | — | ⏸️ 不迁移（破坏交叉编译） |
| 10b | 原子写入 checkpoint | 中 | 极小 | ✅ 顺手修复 |
| 11 | 沙箱安全 | 中 | 中 | ⏸️ 改为命令策略层 |
| 14a | `~` 缩写 home 目录 | 低 | 中 | ⏸️ 低价值（POSIX-only + 需 API） |
| 14b | 窄屏布局 | 低 | 极小 | ✅ 顺手（`flex-wrap:wrap`） |
| 14h | Markdown 表格自适应 | 低 | 小 | ✅ 顺手 |
| 2 | 取消流式生成保留前缀 | — | — | ⏸️ 等取消功能 |
| 3 | ask_user_question 多行 | — | — | ⏸️ Lite 无此工具 |
| 6 | Vision 模型 | — | — | ⏸️ 随图片支持 |
| 8 | 子代理 Profile Bundle | — | — | ⏸️ 不适用 |
| 12 | `@` 菜单 | — | — | ⏸️ 不适用 |
| 13 | web_search 并发 | — | — | ⏸️ 不适用 |

### 建议执行顺序

**第一批（快速修复，本次同步）**：
1. reasoning_content 回传规则修正（1 行）
2. 缓存命中率精度显示（1 行 JS）
3. 原子写入 checkpoint（durability gap 修复）
4. UI 顺手项：窄屏布局（`flex-wrap:wrap`）+ Markdown 表格自适应

**第二批（图片支持，独立迭代 rc.8.01）**：
5. 多模态图片 base64 inline 支持
6. 图片预处理（缩放/格式转换）
7. Files API 上传（可选）

**待议**：
8. 命令策略层（替代沙箱）— 需要单独讨论设计方案
