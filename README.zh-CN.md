# DeepSeek Harness Lite

[English](README.md) | [中文](README.zh-CN.md)

一个轻量级、可嵌入的 Agent 框架，使用 Rust 从零重写，源自 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 架构。

## 这是什么

DeepSeek Harness Lite（`dsh-lite`）是对 DeepSeek Harness 核心架构的全新 Rust 实现——保留了 turn/step agent loop、append-only session log、基于能力的工具系统、声明式 skill——同时去除了重量级的插件加载器、Web 前端和多进程编排层。

最终产物是一个单一静态二进制文件，运行时内存占用约 6 MB，适用于无法运行完整 Node.js 运行时的资源受限环境。

## 核心特性

### 统一 Agent Loop + 策略钩子

每个请求都走同一个 Agent Loop。激活 skill 声明的执行模式决定哪个 `StepHook` 策略控制每步行为——没有独立的分发路径，没有绕过的代码路径：

| 模式 | 确定性 | LLM 使用 | 钩子行为 |
|---|---|---|---|
| `workflow` | 最高（SOP） | 极少——仅判断步骤 | `ForceTool` 跳过 LLM 调用，在循环内直接执行工具 |
| `todo` | 中等（agent 引导） | 每步引导 | `Proceed(Some(guidance))`——LLM 在 skill 约束的上下文中推理 |
| `plan` | 低（探索） | 完整推理 | `Proceed(None)`——agent 自由规划、执行工具、根据结果重新规划 |

这避免了确定性操作的 LLM 调用开销，将推理能力保留给真正不确定的任务。新增模式或行为只需实现 `StepHook` trait——无需修改核心循环代码。

### Think 字段

Skill 声明 `think: true/false` 来控制每个任务类型的模型推理模式。确定性 workflow 关闭推理（快速、省）；探索性诊断开启推理（质量优先）。

### 声明式 Skill（Claude 兼容格式）

Skill 是 YAML frontmatter + Markdown body 文件——与 Claude Code 和原版 DeepSeek Harness 相同的格式：

```yaml
---
name: interface-diagnostics
description: 诊断接口问题
mode: plan
think: true
tools:
  allow: [shell, file_read, memory_write]
variables:
  device_model: "AX-200"
steps:
  - id: check_status
    tool: shell
    args: { command: "show interface brief" }
  - id: analyze
    llm_judge: "识别哪些接口处于 down 状态"
    input: "{{steps.check_status.result}}"
---
# 接口诊断

你是一个诊断助手。按以下流程操作：
1. 检查接口状态
2. 分析异常
3. 提供修复建议
```

同一时间只有一个 skill 激活——其 persona、工具和指令被注入到 prompt 中。工具 schema 按 skill 的白名单过滤。

### Session 日志与消息派生

Append-only 事件日志是模型上下文的唯一真相来源。`derive_messages()` 将表层事件（user / assistant / tool）投影为模型历史。有界环形缓冲区在内存中保存最近的事件；更早的事件在 checkpoint 后被淘汰。

**模型可见 ⟺ 已记录**：任何到达模型请求的内容都可从日志重建。

### 压缩（短上下文生存）

对于上下文窗口有限的小模型，压缩是核心特性，而非可选附加。当派生消息超过上下文窗口的可配置阈值比例时，较早的 turn 会被摘要为一条消息，使用**独立上下文**（摘要请求不包含被摘要的对话本身）。

阈值在 `config/default.yaml` 中可配置：

```yaml
compaction:
  threshold: 0.7           # 消息超过 context_window 的 70% 时触发压缩
  keep_recent_turns: 4     # 始终保留最近 N 个 turn 不被摘要
```

修改后在下一个 chat 请求即时生效（热重载——无需重启）。Web UI 设置面板「通用」页提供压缩滑块，或点「打开配置文件」用系统编辑器编辑。

### 两层记忆

- **短期**：session 事件日志（环形缓冲区 + flash checkpoint）
- **长期**：可配置 KV store（默认 flash 持久化，有界，LRU 淘汰），通过 `memory_read` / `memory_write` / `memory_recall` 工具访问

### 内置工具

| 工具 | 描述 |
|---|---|
| `shell` | 执行 shell 命令（平台感知：`sh -c` / `cmd /c`） |
| `file_read` | 读取文件内容 |
| `file_write` | 写入文件 |
| `file_search` | Glob 模式文件搜索 |
| `ssh_exec` | 通过持久 SSH 会话在远程网元设备上执行命令（连接复用，交互式查询） |
| `memory_*` | 长期记忆读取 / 写入 / 回忆 |
| `todo_write` | 多步操作任务跟踪 |
| `subagent` | 委托子任务给子 agent（零父上下文，maxDepth=3） |

工具是 `ToolPlugin` trait 实现，通过 `register_builtins()` 注册。新增工具只需实现 trait + 一行注册代码——无需修改核心代码。工具通过 3 阶段管线执行：**check**（权限 + 校验）→ **execute**（带超时）→ **result**（截断 + 归一化）。

### 分层系统提示词

系统提示词由 5 个有序 section 拼接——每个极短、高信噪比，固定上下文成本控制在 ~300 tokens 以内：

| order | section | 来源 | tokens |
|---|---|---|---|
| -100 | 身份 | 固定（"You are an AI agent. Working dir: {{cwd}}."） | ~20 |
| 0 | 角色 | skill body | 可变 |
| 5 | 自定义提示词 | 设置面板用户输入 | 可变 |
| 10 | 行为规则 | 3 条通用规则（检查退出码、验证事实、简洁回答） | ~80 |
| 100 | 工具引导 | 每个允许工具 1 句行为规则 | ~15/工具 |

每个工具有 `guidance` 字段（怎么用）独立于 `description`（是什么）——只有 `guidance` 进入系统提示词。运行时变量 `{{cwd}}` 和 `{{model}}` 自动插值。自定义提示词 section 可选（留空则不注入）。详见 [SKILL-GUIDE.md](SKILL-GUIDE.md) §0。

### 自定义系统提示词

用户可通过设置面板（通用页 → 系统提示词）注入自定义提示词，位于角色和行为规则之间，支持 `{{cwd}}`/`{{model}}` 插值，即时生效（热重载，无需重启）。存储在 `config.yaml` 的 `prompt.custom` 段。

### SSH 远程设备操作

内置 `ssh_exec` 工具提供持久 SSH 会话连接网元设备——连接在多次调用间保持打开，支持交互式设备查询（show 命令、配置获取、诊断）。设备在设置面板预配置（工具页 → SSH 开关 → 设备管理）或编辑 `config.yaml`：

```yaml
ssh:
  targets:
    - name: core-router
      host: 192.168.1.1
      port: 22
      user: admin
      password: admin123
```

Skill 中可用 `ssh_exec` 配合 `target` 名称或内联 `host`/`user`/`password`。完整 SSH 使用指南（配置、调用方式、持久会话、skill 示例）见 [SKILL-GUIDE.md](SKILL-GUIDE.md) §9。

### 单二进制，无运行时依赖

- musl 静态链接——不依赖 glibc
- 所有依赖均为纯 Rust（无 C 绑定，无需交叉工具链）
- 配置和 skill 从文件系统加载；Web 客户端编译时嵌入

### 内存占用

| 指标 | 数值 |
|---|---|
| 运行时 RSS | ~6 MB |
| 二进制大小 | ~2.6 MB |
| 目标 | < 10 MB RSS |

## 架构

统一插件化架构（"一个循环 + 可插拔钩子"）替代了原有的三个硬编码分发分支。所有执行模式——`workflow`、`todo`、`plan`——都通过同一个 agent loop 运行，行为由 `StepHook` 策略控制。新增模式或行为只需实现 trait，无需修改核心循环代码。完整设计见 [DESIGN-UNIFIED.md](DESIGN-UNIFIED.md)。

```
用户输入
  → Dispatcher（根据激活 skill 模式构建钩子）
     → AgentLoop（唯一循环，钩子决定每步行为）
        ├─ StepHook::pre_step()
        │    ├─ Proceed(injection) → 正常 LLM 步骤（plan / todo）
        │    ├─ ForceTool(call)    → 跳过 LLM，直接执行工具（workflow tool 步）
        │    ├─ ForceLlm(prompt)   → 独立上下文调 LLM（workflow llm_judge）
        │    └─ Stop(reason)       → 结束 turn
        ├─ 执行工具 / 流式 LLM
        └─ StepHook::post_step() → 继续或停止
```

| 模式 | pre_step 返回 | LLM 使用 | 确定性 |
|---|---|---|---|
| `plan` | `Proceed(None)` | 每步完整推理 | LLM 驱动 |
| `todo` | `Proceed(Some(guidance))` | 每步带引导 | 中等 |
| `workflow`（tool） | `ForceTool(call)` | **0 次调用**——钩子直供 | 100% 可重复 |
| `workflow`（llm_judge） | `ForceLlm(prompt)` | 单次调用，独立上下文 | 高 |

### 五个扩展点

| 扩展点 | Trait | 作用 |
|---|---|---|
| 步骤钩子 | `StepHook` | 每步决策：注入 / 强制工具 / 强制 LLM / 停止 |
| 工具插件 | `ToolPlugin` | 工具定义 + 执行一体化 |
| 提示段落 | `PromptSection` | 动态系统提示 section |
| 命令插件 | `CommandPlugin` | 斜杠命令注册 |
| 子 agent 工具 | `SubagentTool` | 子 agent 委托（继承 dsh 核心模式） |

### 模块映射

| 模块 | 职责 | 对应 dsh |
|---|---|---|
| `types` | 核心类型定义 + 配置结构 | session + agent + llm types |
| `session` | Append-only 事件日志 + 消息派生 + flash checkpoint + 压缩 | core/session |
| `llm` | HTTP 流式客户端（OpenAI 兼容） | llm/llm |
| `prompt` | 系统 prompt 组装 + 动态 `PromptSection` | core/system-prompt |
| `hooks` | `StepHook` trait + `StepDecision` / `StepFlow` / 上下文类型 | agent/pre-step 瀑布流 |
| `strategies` | 三个 `StepHook` 实现：Plan / Todo / Workflow |（新增，替代分发分支）|
| `tools` | `ToolPlugin` trait + `ToolRegistry` + 3 阶段执行管线 | core/tools |
| `commands` | `CommandPlugin` trait + 内置斜杠命令 | interaction/commands |
| `subagent` | `SubagentTool` — 子 agent 委托（零父上下文，maxDepth=3） | subagent capability |
| `ssh` | 持久 SSH 会话 — 网元设备后台连接池 |（新增）|
| `policy` | 允许/拒绝权限检查 | sandbox-policy |
| `skill` | 声明式 skill 加载（YAML + MD） | skill/skill + skill-filesystem |
| `agent` | Turn/step 驱动 + 钩子集成 + 压缩 | core/agent-loop |
| `expr` | 条件表达式求值 + 变量插值 |（新增）|
| `memory` | 长期 KV store（flash 持久化，LRU） |（新增）|
| `compaction` | 滚动上下文摘要（独立上下文，阈值可配置） |（新增）|
| `dispatcher` | 根据 skill 模式构建钩子 + 驱动 AgentLoop |（新增，已简化）|
| `server` | HTTP 服务器 + SSE 流式 + Web 客户端 + 配置热重载 |（新增）|

## 构建

```sh
# 前置条件：Rust 1.75+，cargo-zigbuild + zig 用于交叉编译

# 本机构建
cargo build --release
# → target/release/dsh-lite

# 交叉编译到嵌入式目标（musl 静态链接）
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

支持的目标平台：

| 目标 | 平台 | 二进制大小 |
|---|---|---|
| `x86_64-pc-windows-msvc` | Windows x86_64 | ~2.7 MB |
| `aarch64-unknown-linux-musl` | Linux ARM64（静态） | ~2.7 MB |
| `armv7-unknown-linux-musleabihf` | Linux ARMv7 硬浮点（静态） | ~2.9 MB |
| `armv7-unknown-linux-musleabi` | Linux ARMv7 软浮点（静态） | ~2.9 MB |
| `x86_64-unknown-linux-musl` | Linux x86_64（静态） | ~3.2 MB |

所有 Linux 二进制均为 musl 静态链接——无运行时依赖，开箱即用。工具链安装见 [cross/README.md](cross/README.md)。

## 发布

推送版本标签即可触发自动多平台构建和 GitHub Release：

```sh
git tag v0.1.0-rc.6
git push origin v0.1.0-rc.6
```

CI 工作流（`.github/workflows/release.yml`）并行构建全部 5 个目标，每个打包包含 `config.yaml` + `skills/` + `README.md`，并创建 GitHub Release 供下载。

本地打包用 `packages.ps1` 脚本：

```pwsh
pwsh -File packages.ps1 -Version 0.1.0-rc.6
# → release-packages/dsh-lite-0.1.0-rc.6-{平台}.{zip|tar.gz}
```

完整发布流程见 [RELEASE.md](RELEASE.md)。

## 运行

```sh
# 交互模式（Web 客户端）
dsh-lite
# → 在 http://127.0.0.1:3081 启动 HTTP 服务器
#   浏览器打开即可使用：聊天 + Markdown 渲染 + 会话侧边栏 + 轨迹开关

# 单轮模式（传入 prompt）
dsh-lite "检查接口状态"

# 选择特定 skill
dsh-lite --skill interface-diagnostics "eth0 is down"
```

agent 从 `config/default.yaml` 加载配置，扫描 `skills/` 目录的 skill 文件，
通过激活 skill 的模式驱动 AgentLoop 执行请求。

配置：

```yaml
# config/default.yaml
model:
  base_url: "http://127.0.0.1:8080/v1"
  model: "your-model"
  context_window: 8192

compaction:
  threshold: 0.7           # 消息超过 context_window 的 70% 时触发压缩
  keep_recent_turns: 4

tools:
  ssh_exec: true            # 启用 SSH 工具

ssh:
  targets:                  # 预配置设备目标（持久会话）
    - name: core-router
      host: 192.168.1.1
      port: 22
      user: admin
      password: admin123

skill:
  dir: skills
```

模型端点为 OpenAI 兼容（`/v1/chat/completions`，支持流式）。所有配置修改在下一个 chat 请求时热重载——无需重启。可通过设置面板「打开配置文件」按钮用系统编辑器编辑。

## 项目状态

统一插件化架构（DESIGN-UNIFIED.md，7 个阶段）已**全部完成**。所有扩展点已实现并测试：`StepHook`、`ToolPlugin`、`PromptSection`、`CommandPlugin`、`SubagentTool`。46 个测试通过，0 个编译警告。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 脚手架 + 交叉编译 | ✅ 完成 |
| P1 | 核心 agent loop（plan 模式） | ✅ 完成 |
| P2 | 三种执行模式（workflow/todo/plan） | ✅ 完成 |
| P3 | Skill 系统完善 | ✅ 完成 |
| P4 | 记忆 + 压缩 + 持久化 | ✅ 完成 |
| P5 | 会话管理（多会话 + offloading） | ✅ 完成 |
| P6 | Web 客户端 + HTTP 服务器 | ✅ 完成 |
| **统一 1** | **StepHook + AgentLoop 重构 + 三个策略** | **✅ 完成** |
| **统一 2** | **ToolPlugin trait + 消除注册重复** | **✅ 完成** |
| **统一 3** | **PromptSection 动态注册** | **✅ 完成** |
| **统一 4** | **CommandPlugin trait + 统一斜杠命令** | **✅ 完成** |
| **统一 5** | **SubagentTool — 子 agent 委托** | **✅ 完成** |
| **统一 6** | **压缩消息替换 + think bug 修复** | **✅ 完成** |
| **统一 7** | **Skill creator skill 文件 — AI 引导生成 skill（plan 模式 skill）** | **✅ 完成** |
| **7 后增强** | **斜杠命令自动补全弹窗 + 压缩比例可配置** | **✅ 完成** |
| P7 | SSH 持久交互式会话 | ✅ 完成 |
| **7 后增强+** | **Workflow+Subagent skill 示例 + 压缩 GUI 滑块 + SSH 工具开关** | **✅ 完成** |
| P8 | 体积 + 内存优化 | ✅ 已验证（二进制 2.63 MB） |
| **8 后增强** | **分层系统提示词（5 section）+ 工具 guidance 字段** | **✅ 完成** |
| **8 后增强+** | **自定义系统提示词（设置面板）+ SSH 文档 + remote-health-check skill** | **✅ 完成** |

### 斜杠命令自动补全

在 Web 输入框中输入 `/` 会弹出命令列表（从 `GET /api/commands` 获取，读取 `CommandPlugin` 列表）。弹窗随输入实时过滤匹配命令，支持键盘导航（`↑↓` 切换、`Enter`/`Tab` 选中、`Esc` 关闭），选中后插入命令到输入框。这是对原版 dsh `InputTriggerService` slash-menu 模式的轻量级前端实现。

## 许可证

MIT
