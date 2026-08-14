# DeepSeek Harness Lite

[English](README.md) | [中文](README.zh-CN.md)

一个轻量级、可嵌入的 Agent 框架，使用 Rust 从零重写，源自 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 架构。

## 这是什么

DeepSeek Harness Lite（`dsh-lite`）是对 DeepSeek Harness 核心架构的全新 Rust 实现——保留了 turn/step agent loop、append-only session log、基于能力的工具系统、声明式 skill——同时去除了重量级的插件加载器、Web 前端和多进程编排层。

最终产物是一个单一静态二进制文件，运行时内存占用约 6 MB，适用于无法运行完整 Node.js 运行时的资源受限环境。

## 核心特性

### 三模式任务分发

每个请求根据当前激活 skill 声明的执行模式进行路由：

| 模式 | 确定性 | LLM 使用 | 机制 |
|---|---|---|---|
| `workflow` | 最高（SOP） | 极少——仅判断步骤 | 绕过 agent loop，直接执行固定工具步骤 |
| `todo` | 中等（agent 引导） | 每步引导 | agent loop 运行，但受 skill 步骤序列约束 |
| `plan` | 低（探索） | 完整推理 | agent 自由规划、执行工具、根据结果重新规划 |

这避免了对确定性操作运行完整 agent loop 的开销，将 LLM 推理能力保留给真正不确定的任务。

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

对于上下文窗口有限的小模型，压缩是核心特性，而非可选附加。当派生消息超过阈值时，较早的 turn 会被摘要为一条消息，使用**独立上下文**（摘要请求不包含被摘要的对话本身）。

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
| `ssh_exec` | SSH 命令执行，持久连接池（占位） |
| `memory_*` | 长期记忆读取 / 写入 / 回忆 |
| `todo_write` | 多步操作任务跟踪 |

工具通过 3 阶段管线执行：**check**（权限 + 校验）→ **execute**（带超时）→ **result**（截断 + 归一化）。

### 单二进制，无运行时依赖

- musl 静态链接——不依赖 glibc
- 所有依赖均为纯 Rust（无 C 绑定，无需交叉工具链）
- 配置和 skill 从文件系统加载；Web 客户端编译时嵌入

### 内存占用

| 指标 | 数值 |
|---|---|
| 运行时 RSS（P4） | ~6 MB |
| 二进制大小 | ~1.5 MB |
| 目标 | < 10 MB RSS |

## 架构

```
用户输入
  → Dispatcher（按 skill 模式路由）
     ├─ workflow → 固定步骤序列（绕过 agent loop）
     ├─ todo     → agent loop + 步骤引导
     └─ plan     → 完整 agent loop（探索 → 执行 → 重新规划）

Agent loop（plan/todo）：
  turn/start
    → 组装 prompt（persona + 工具 + 变量）
    → step/start → LLM 流式 → assistant 消息
      → tool calls → 3 阶段管线 → tool results
    → step/end
    →（有待处理的工具或新输入 → 下一步）
  turn/end
```

### 模块映射

| 模块 | 职责 | 对应 dsh |
|---|---|---|
| `types` | 核心类型定义 | session + agent + llm types |
| `session` | Append-only 事件日志 + 消息派生 + flash checkpoint | core/session |
| `llm` | HTTP 流式客户端（OpenAI 兼容） | llm/llm |
| `prompt` | 系统 prompt 组装 | core/system-prompt |
| `tools` | 工具注册表 + 3 阶段执行管线 | core/tools |
| `policy` | 允许/拒绝权限检查 | sandbox-policy |
| `skill` | 声明式 skill 加载（YAML + MD） | skill/skill + skill-filesystem |
| `agent` | Turn/step 驱动（plan 模式） | core/agent-loop |
| `expr` | 条件表达式求值 + 变量插值 |（新增）|
| `memory` | 长期 KV store（flash 持久化，LRU） |（新增）|
| `compaction` | 滚动上下文摘要（独立上下文） |（新增）|
| `dispatcher` | 三模式路由（workflow/todo/plan） |（新增）|

## 构建

```sh
# 前置条件：Rust 1.75+，可选 cargo-zigbuild + zig 用于交叉编译

# 本机构建
cargo build --release
# → target/release/dsh-lite

# 交叉编译到嵌入式目标（musl 静态链接）
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
```

支持的目标平台：

| 目标 | 架构 |
|---|---|
| `x86_64-unknown-linux-musl` | x86_64（开发） |
| `aarch64-unknown-linux-musl` | ARM 64 位 |
| `armv7-unknown-linux-musleabihf` | ARMv7 硬浮点 |
| `armv7-unknown-linux-musleabi` | ARMv7 软浮点 |

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

agent 从 `config/default.toml` 加载配置，扫描 `skills/` 目录的 skill 文件，
通过激活 skill 的模式分发请求。

配置：

```toml
# config/default.toml
[model]
base_url = "http://127.0.0.1:8080/v1"
model = "your-model"
context_window = 8192

[skill]
dir = "skills"
```

模型端点为 OpenAI 兼容（`/v1/chat/completions`，支持流式）。

## 项目状态

当前进度：**P6**（Web 客户端 + HTTP 服务器）。完整设计文档和路线图见 [DESIGN-lite.md](DESIGN-lite.md)。

| 阶段 | 状态 |
|---|---|
| P0 — 脚手架 + 交叉编译 | ✅ 完成 |
| P1 — 核心 agent loop（plan 模式） | ✅ 完成 |
| P2 — 三模式分发（workflow/todo/plan） | ✅ 完成 |
| P3 — Skill 系统完善 | ✅ 完成 |
| P4 — 记忆 + 压缩 + 持久化 | ✅ 完成 |
| P5 — 会话管理（多会话 + offloading） | ✅ 完成 |
| P6 — Web 客户端 + HTTP 服务器 | ✅ 完成 |
| P7 — SSH 客户端 | 🔄 下一步 |
| P8 — 体积 + 内存优化 | 计划中 |

## 许可证

MIT
