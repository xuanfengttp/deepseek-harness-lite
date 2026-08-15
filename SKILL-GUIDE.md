# Skill 编写指南

dsh-lite 的 skill 是 YAML frontmatter + Markdown body 的 `.md` 文件，放在 `skills/` 目录下。
每个 skill 声明一种执行模式，与 agent 的 `StepHook` 策略紧密配合。

---

## 1. 选择模式

| 场景 | 模式 | think | LLM 调用次数 | 确定性 |
|------|------|-------|-------------|--------|
| 流程固定、步骤已知（巡检、备份） | `workflow` | `false` | 0~少量 | 100% |
| 流程已知、部分需 LLM 判断 | `workflow` + `llm_judge` | `false` | judge 步各 1 次 | 高 |
| 步骤大致知道、细节需 LLM 填充 | `todo` | `false` | 每步 1 次 | 中 |
| 问题未知、需自主探索 | `plan` | `true` | 多次 | 取决于 LLM |

---

## 2. Workflow 模式（确定性 SOP）

### 基本结构

```yaml
---
name: my-sop                          # 必填，kebab-case
description: 一句话描述                # 必填
mode: workflow                        # 必填
think: false                          # 推荐 false（快、省）
tools:
  allow: [shell]                      # 精确限定工具
variables:                            # 可变参数，不同设备复用
  target: "192.168.1.1"
  iface: "eth0"
steps:                                # 按顺序执行
  - id: step1                         # 必填，步骤标识
    tool: shell                       # 工具名（扁平结构，不要嵌套 action:）
    args:                             # JSON 参数，支持 {{var}} 插值
      command: "ping -c 5 {{target}}"
    when: "steps.init.result length > 0"  # 可选，条件满足才执行
---
Markdown 正文（此 skill 的人话说明，workflow 模式不发给 LLM）。
```

### 两种步骤类型

**Tool 步**（确定执行，0 次 LLM）：
```yaml
- id: ping
  tool: shell
  args:
    command: "ping -c 10 {{target}}"
```

**LlmJudge 步**（单次 LLM 调用，独立上下文）：
```yaml
- id: analyze
  llm_judge: "你是网络专家。分析数据，输出：状态/原因/建议"
  input: "Ping:\n{{steps.ping.result}}\n\n接口:\n{{steps.iface.result}}"
```

> **重要**：步骤用扁平结构（`tool:` / `llm_judge:` 直接在 step 下），不要用 `action: { tool: }` 嵌套结构，否则解析器无法识别。

### 变量插值

| 语法 | 含义 | 可用位置 |
|------|------|----------|
| `{{var_name}}` | variables 中定义的变量 | args, prompt, input, when |
| `{{steps.xxx.result}}` | 前序步骤的输出结果 | args, prompt, input, when |

### when 条件表达式

```
steps.ping.result contains "timeout"     # 包含检查
steps.ping.result length > 0             # 非空检查
steps.ping.result == "OK"                # 精确匹配
steps.ping.result != "FAIL"              # 不等于
not steps.ping.result contains "error"   # 逻辑非
expr1 and expr2                          # 逻辑与
expr1 or expr2                           # 逻辑或
```

### 完整示例：接口健康检查

```yaml
---
name: interface-health-check
description: 网络接口健康检查（ping + 状态 + 错误计数 + 分析）
mode: workflow
think: false
tools:
  allow: [shell]
variables:
  target: "192.168.1.1"
  iface: "eth0"
steps:
  - id: ping
    tool: shell
    args:
      command: "ping -c 10 {{target}}"

  - id: interface_status
    tool: shell
    args:
      command: "ip link show {{iface}}"

  - id: error_counters
    tool: shell
    args:
      command: "ip -s link show {{iface}}"

  - id: analyze
    llm_judge: |
      你是网络诊断专家。分析以下数据，按此格式输出：
      状态: [正常/警告/故障]
      原因: [一句话说明]
      建议: [具体操作]
    input: |
      Ping 结果:
      {{steps.ping.result}}

      接口状态:
      {{steps.interface_status.result}}

      错误计数:
      {{steps.error_counters.result}}
---
网络接口健康检查标准 SOP。
```

### 准确率优化

1. **llm_judge 的 prompt 给输出模板**：明确格式，减少 LLM 跑偏
2. **tools.allow 精确限定**：只列用到的工具
3. **variables 参数化**：同 SOP 复用不同设备，只改变量
4. **when 做条件分支**：根据前序结果跳过/执行后续步骤

---

## 3. Plan 模式（自主探索）

### 基本结构

```yaml
---
name: fault-diagnosis
description: 网络故障自主诊断
mode: plan
think: true                           # 推荐 true（质量优先）
tools:
  allow: [shell, file_read, memory_read, memory_write]
---
你是高级网络诊断工程师。

## 诊断方法论

1. 症状确认 → 2. 分层排查 → 3. 假设验证 → 4. 范围收窄 → 5. 根因确认

## 输出要求

- 诊断过程：每步命令和发现
- 根因结论：一句话
- 修复建议：具体操作
```

### 编写要点

- **body 是 persona（角色指令）**，不是步骤——plan 模式 LLM 自主决定步骤
- **写方法论而非步骤**：不是"检查网络"，而是"分层排查→假设验证→范围收窄"
- **think: true** 开启推理模式，复杂问题分析质量更高
- **tools.allow 给足工具**：让 LLM 有探索空间
- **明确输出格式**：减少 LLM 跑偏

---

## 4. Todo 模式（引导执行）

### 基本结构

```yaml
---
name: config-audit
description: 设备配置审计
mode: todo
think: false
tools:
  allow: [shell, file_read]
steps:
  - id: check_hostname
    tool: shell
    args: { command: "hostname" }
  - id: check_routing
    tool: shell
    args: { command: "ip route show" }
  - id: summarize
    llm_judge: "汇总审计结果，标注不合规项"
    input: "{{steps.check_hostname.result}}\n{{steps.check_routing.result}}"
---
设备配置审计 SOP。
```

### 编写要点

- steps 同时作为 LLM 引导和执行序列
- 每步 LLM 被告知"当前是步骤 N"，按引导执行
- think: false 因为步骤已知

---

## 5. Workflow + Subagent 协同模式

这是 dsh-lite 的核心设计模式：主 agent（plan 模式，智能规划）委托子 agent
执行特定 skill。主 agent 决定"做什么、给谁做"，子 agent 决定"怎么做"。

### 5.1 协同架构

```
主 agent (plan, think=true)
  │  智能规划：分析任务 → 决定委托策略
  │
  ├─ subagent #1 (skill: "health-check", workflow)
  │    确定性 SOP：固定命令序列，0 次 LLM 理解，100% 可重复
  │    → 返回设备健康报告
  │
  ├─ subagent #2 (skill: "health-check", workflow)
  │    同样的 SOP，不同设备
  │    → 返回设备健康报告
  │
  └─ subagent #3 (无 skill, plan)
       自主探索：LLM 自由排查
       → 返回诊断结论
  │
  └─ 主 agent 汇总所有子 agent 结果 → 生成最终报告
```

### 5.2 两种委托策略

**策略 A：确定性委托**（指定 workflow skill）

适用于已知流程、固定步骤的任务（巡检、备份、配置核查）。
子 agent 用 `WorkflowStrategy`，tool 步绕过 LLM（0 次调用），judge 步单次 LLM。

```json
{"tool": "subagent", "arguments": {
  "description": "health check 192.168.1.1",
  "prompt": "对设备 192.168.1.1 执行标准健康检查，报告 CPU、内存、接口、服务状态。",
  "skill": "health-check"
}}
```

**策略 B：自主探索委托**（不指定 skill）

适用于未知问题、需要 LLM 自主推理的任务（根因诊断、异常排查）。
子 agent 用 `PlanStrategy`，每步都调 LLM，自由探索。

```json
{"tool": "subagent", "arguments": {
  "description": "丢包根因诊断",
  "prompt": "设备 192.168.1.1 的 eth0 接口出现间歇性丢包，2 小时前开始。自主排查：检查接口统计、路由、ARP、CPU 负载、日志。报告根因和修复建议。"
}}
```

### 5.3 主 agent 的 skill 怎么写

主 agent 的 skill 是 `plan` 模式，`tools.allow` 必须包含 `subagent`。
body（Markdown 正文）是主 agent 的 persona，告诉它如何编排委托。

**示例：批量巡检编排 skill**（`skills/batch-inspection.md`）

```yaml
---
name: batch-inspection
description: 批量巡检编排 — 主 agent 规划检查哪些设备，委托子 agent 执行健康检查 SOP
mode: plan
think: true
tools:
  allow: [shell, file_read, memory_read, memory_write, todo_write, subagent]
---
# 批量巡检编排

你是网络运维协调员。

## 工作流程
1. 确定巡检目标设备列表
2. 用 todo_write 创建巡检任务清单（每台设备一个任务）
3. 对每台设备调用 subagent，指定 skill: "health-check"
4. 收集所有子 agent 的结果
5. 汇总生成巡检报告

## 委托方式
对每台设备调用：
{"tool": "subagent", "arguments": {
  "description": "health check <设备IP>",
  "prompt": "对设备 <设备IP> 执行标准健康检查",
  "skill": "health-check"
}}
```

**要点**：
- `mode: plan` — 主 agent 需要智能规划（决定检查哪些设备、如何汇总）
- `think: true` — 编排需要推理
- `tools.allow` 包含 `subagent` — 这是委托的关键
- body 里明确告诉主 agent "调用 subagent 并指定 skill"
- 主 agent 自己不执行诊断命令，只做编排 + 汇总

### 5.4 子 agent 的 skill 怎么写

子 agent 的 skill 可以是 `workflow`（确定性）或 `plan`（自主），取决于任务性质。

**确定性 SOP skill**（子 agent 用 workflow，见第 2 节）

```yaml
---
name: health-check
mode: workflow
think: false
tools:
  allow: [shell, ssh_exec]
steps:
  - id: cpu_mem
    tool: shell
    args: { command: "uname -a" }
  - id: interface
    tool: shell
    args: { command: "ip addr" }
  - id: summarize
    llm_judge: "汇总健康检查结果，标注异常项"
    input: "{{steps.cpu_mem.result}}\n{{steps.interface.result}}"
---
```

**关键**：子 agent 的 skill 不需要包含 `subagent` 工具（子 agent 一般不继续委托）。

### 5.5 协同模式速查

| 场景 | 主 agent skill | 子 agent skill | LLM 调用 |
|------|---------------|---------------|----------|
| 批量巡检 | plan + subagent | workflow（health-check） | 主每步 1 次 + 子 judge 步各 1 次 |
| 故障诊断 | plan + subagent | plan（interface-diagnostics） | 主 + 子都每步 1 次 |
| 配置核查 | plan + subagent | workflow（config-audit） | 主每步 1 次 + 子 judge 步 1 次 |
| 未知异常排查 | plan + subagent | 无 skill（默认 plan） | 主 + 子都每步 1 次 |

### 5.6 示例 skill 文件

项目 `skills/` 目录包含 4 个示例 skill，覆盖所有协同模式：

| 文件 | 模式 | 角色 | 用途 |
|------|------|------|------|
| `health-check.md` | workflow | 子 agent | 确定性健康检查 SOP |
| `interface-diagnostics.md` | plan | 子 agent | 接口故障自主诊断 |
| `batch-inspection.md` | plan | 主 agent | 批量巡检编排（委托 health-check） |
| `fault-triage.md` | plan | 主 agent | 故障分诊（委托诊断 + 执行修复） |

### 5.7 编写检查清单

写主 agent 编排 skill 时检查：
- [ ] `mode: plan`（编排需要智能规划）
- [ ] `think: true`（编排需要推理）
- [ ] `tools.allow` 包含 `subagent`
- [ ] body 明确说明委托策略（指定哪个 skill / 不指定）
- [ ] body 给出 subagent 调用 JSON 示例
- [ ] body 说明如何汇总子 agent 结果

写子 agent 执行 skill 时检查：
- [ ] 根据确定性需求选 `workflow` 或 `plan`
- [ ] `tools.allow` 精确限定（不需要 `subagent`）
- [ ] workflow 的 steps 覆盖完整流程
- [ ] 最后一步用 `llm_judge` 汇总输出（子 agent 只返回最终输出）

---

## 6. 字段速查

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | ✅ | kebab-case 标识 |
| `description` | string | ✅ | 一句话描述 |
| `whenToUse` | string | ❌ | 何时使用此 skill |
| `mode` | `workflow`/`todo`/`plan` | ✅ | 执行模式 |
| `think` | bool | ❌ | LLM 推理模式（默认 plan=true，其他=false） |
| `tools.allow` | string[] | ❌ | 工具白名单（空=全部允许） |
| `variables` | map | ❌ | 可变参数 |
| `steps` | Step[] | workflow/todo 必填 | 执行步骤（扁平结构：`tool:` / `llm_judge:` 直接在 step 下） |
| body | markdown | ❌ | persona（plan）或说明（workflow） |

---

## 7. 上下文压缩配置

当对话消息超过模型上下文窗口的可配置阈值比例时，较早的 turn 会被自动摘要为一条消息，保留最近的对话不变。这通过 `config/default.yaml` 的 `compaction` 段控制：

```yaml
compaction:
  threshold: 0.7           # 消息 token 超过 context_window 的 70% 时触发压缩
  keep_recent_turns: 4     # 始终保留最近 4 个 turn 不被摘要
```

**调优建议**：

| 场景 | threshold | keep_recent_turns | 说明 |
|------|-----------|-------------------|------|
| 小上下文模型（8K） | 0.6 | 3 | 早点压缩，避免溢出 |
| 大上下文模型（32K+） | 0.8 | 6 | 晚点压缩，保留更多上下文 |
| 长 workflow SOP | 0.7 | 4 | 平衡——保留最近结果供 llm_judge 引用 |

修改后在下一个 chat 请求即时生效（热重载），无需重启。Web UI 设置面板的「通用」页可直接调整压缩滑块，或点「打开配置文件」用系统编辑器编辑 YAML。

**与 skill 的关系**：
- `workflow` 模式：压缩对 workflow 步骤无影响（ForceTool 不累积 LLM 历史）
- `todo` 模式：压缩后最近 N 个 turn 的步骤引导仍保留
- `plan` 模式：压缩影响最大——较早的探索历史被摘要，最近的推理链保留

---

## 8. 斜杠命令

在 Web 输入框中输入 `/` 会自动弹出命令列表，支持模糊匹配和键盘导航：

| 命令 | 作用 |
|------|------|
| `/new` | 创建新会话 |
| `/clear` | 清空当前会话消息 |
| `/compact` | 手动触发上下文压缩 |
| `/context` | 查看当前上下文占用 |
| `/help` | 查看帮助 |

命令列表由后端 `CommandPlugin` 动态注册，前端通过 `GET /api/commands` 获取，新增命令只需实现 `CommandPlugin` trait 并注册。
