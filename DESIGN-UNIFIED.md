# 统一插件化架构设计（兼容合一方案）

> **实现状态：7 个阶段全部完成（✅）。** 44 个测试通过，0 个编译警告。
> Phase 1-7 完成于 commit `38039a2eb7` → `8cfb24231c`；
> 后续增强（斜杠命令弹窗 + 压缩比例可配置）完成于 `8c000b5a20`；
> 设置面板重设计 + YAML 配置 + skill_creator 转为 skill 文件完成于 `5fec787`。
>
> 目标：把原版 dsh 的"Everything is a Plugin"核心理念，适配到 Rust 嵌入式约束，
> 同时保留 workflow 确定性 SOP 执行能力来提升准确率。

## 1. 问题分析

### 1.1 原版创新
原版 dsh 基于 Cordis 框架，**所有功能都是插件**：
- Agent loop 是插件（`AgentLoop extends Service implements AgentFactory`）
- 工具是插件（`ctx.tools.register(defineTool())`）
- plan/goal/compaction/workflow 全是插件，通过 hook `agent/pre-step` 等瀑布流钩子组合行为
- **没有特权核心**——新功能 = 写插件 + 配置行

### 1.2 lite 现状（重构前 → 重构后）

重构前的硬编码状态：
- 三层分发（Workflow/Todo/Plan）是 `dispatcher.rs` 的硬编码 if-else，**不是插件**
- 工具注册是 `main.rs` + `server.rs` 重复两遍的硬编码 if-else
- 系统提示是固定两段（persona + tools），不可扩展
- 斜杠命令是 `handle_command` 的硬编码 match 分支
- Agent loop 有 0 个钩子，扩展必须改源码

**重构后（已实现 ✅）**：
- 三层分发 → `StepHook` 策略（`src/strategies/`），同一 AgentLoop + 不同钩子
- 工具注册 → `ToolPlugin` trait + `register_builtins()`，消除重复
- 系统提示 → `PromptSection` 动态注册
- 斜杠命令 → `CommandPlugin` trait + `handle_command` 遍历插件列表
- Agent loop → `Vec<Box<dyn StepHook>>`，5 个扩展点全部可用
- 子 agent → `SubagentTool`（Phase 5），skill 生成 → `skills/skill-creator.md`（plan 模式 skill 文件，Phase 7 重构后）

### 1.3 用户诉求
- 需要确定性 SOP/workflow 提升执行准确率（部分场景是固定流程，不能让 LLM 自由发挥）
- 希望引入插件化机制，提升架构扩展性
- 嵌入式约束：<10MB RSS, 单二进制, musl 静态链接, 不可能引入 Cordis

### 1.4 兼容合一的核心矛盾
- 原版的 workflow 是 LLM 调用的工具（LLM 决定何时编排）
- 用户的 workflow 是确定性执行路径（绕过 LLM，保证准确率）
- 需要既保留确定性执行，又实现插件化扩展

## 2. 设计方案：一个循环 + 可插拔钩子

### 2.1 核心原则

**One loop, pluggable hooks** — 对应原版的"one agent loop + waterfall hooks"，
用 Rust trait 实现轻量级钩子机制，不引入框架。

### 2.2 五个扩展点（原版 → lite 映射）

| 原版 API | lite 对应 | 作用 | 状态 |
|----------|-----------|------|------|
| `ctx.on('agent/pre-step', ...)` | `trait StepHook` | 步骤前决策：注入/强制/停止 | ✅ Phase 1 |
| `ctx.tools.register(defineTool())` | `trait ToolPlugin` | 工具注册：定义 + 执行一体 | ✅ Phase 2 |
| `ctx.systemPrompt.section(...)` | `Vec<PromptSection>` | 系统提示动态 section | ✅ Phase 3 |
| `ctx.commands.register(...)` | `trait CommandPlugin` | 斜杠命令注册 | ✅ Phase 4 |
| subagent capability | `SubagentTool` | 子 agent 委托（零父上下文） | ✅ Phase 5 |

### 2.3 关键设计：Workflow 作为钩子而非独立路径

**这是"兼容合一"的核心。** 三种模式不再走三条独立代码路径，
而是通过 `StepHook` 钩子控制同一个 agent loop 的行为：

```
┌─────────────────────────────────────────────────────────┐
│                    AgentLoop（唯一循环）                   │
│  begin_turn → assemble prompt                            │
│  loop {                                                  │
│    begin_step                                            │
│    ┌─ StepHook::pre_step() ──────────────────────┐       │
│    │  Proceed(injection)  → LLM 正常运行          │       │
│    │  ForceTool(call)     → 跳过 LLM，直接执行工具 │       │
│    │  ForceLlm(prompt)    → 独立上下文调 LLM       │       │
│    │  Stop(reason)        → 结束 turn             │       │
│    └──────────────────────────────────────────────┘       │
│    execute tools / stream LLM                             │
│    ┌─ StepHook::post_step() ─────────────────────┐       │
│    │  Continue → 下一步                           │       │
│    │  Stop(reason) → 结束 turn                   │       │
│    └──────────────────────────────────────────────┘       │
│    end_step                                              │
│  }                                                       │
│  end_turn                                                │
└─────────────────────────────────────────────────────────┘
```

| 模式 | pre_step 返回 | 行为 | LLM 参与 |
|------|--------------|------|----------|
| **Plan** | `Proceed(None)` | LLM 自由规划执行 | ✅ 每步都调 |
| **Todo** | `Proceed(Some("当前是步骤N: ..."))` | LLM 带步骤引导执行 | ✅ 每步都调 |
| **Workflow (Tool步)** | `ForceTool(call)` | 跳过 LLM，直接执行工具 | ❌ 不调 |
| **Workflow (LlmJudge步)** | `ForceLlm(prompt)` | 独立上下文调 LLM | ✅ 单次调用 |

**确定性保证**：WorkflowStrategy 的 `ForceTool` 完全绕过 LLM，步骤顺序、工具参数
都由 skill YAML 的 `steps` 定义，不受 LLM 自由意志影响。准确率与当前实现一致。

**插件化收益**：三种模式共享同一个循环、同一套事件流、同一个 session log。
新增模式只需实现 `StepHook`，不改 AgentLoop 核心代码。

## 3. 详细设计

### 3.1 StepHook trait（`src/hooks.rs`，新建）

```rust
//! Step hooks: lightweight plugin extension points for the agent loop.
//!
//! Maps to dsh `agent/pre-step` waterfall hook. Hooks are synchronous — they
//! make a decision; the loop does the async work (LLM call, tool execution).

use crate::types::*;

/// Context passed to pre_step.
pub struct PreStepContext<'a> {
    pub turn: u64,
    pub step: u64,
    pub skill: &'a Skill,
}

/// Decision returned by pre_step — controls what the loop does this step.
pub enum StepDecision {
    /// Let the LLM run normally. Optionally inject guidance text
    /// (appended as a user-role message before the LLM call).
    Proceed { injection: Option<String> },
    /// Skip the LLM entirely. Execute this tool call directly.
    /// Used by WorkflowStrategy for deterministic tool steps.
    ForceTool { call: ToolCall },
    /// Call the LLM with a specific prompt, independent context (no history).
    /// Used by WorkflowStrategy for llm_judge steps.
    ForceLlm { system: String, prompt: String },
    /// End the turn now.
    Stop { reason: TurnEndReason },
}

/// Context passed to post_step.
pub struct PostStepContext<'a> {
    pub turn: u64,
    pub step: u64,
    pub skill: &'a Skill,
    /// What happened this step (the assistant content or tool result).
    pub content: String,
    pub is_error: bool,
    /// Whether the LLM requested tool calls (only meaningful for Proceed).
    pub had_tool_calls: bool,
}

/// Flow control returned by post_step — whether to continue or stop.
pub enum StepFlow {
    /// Run another step.
    Continue,
    /// End the turn with this reason.
    Stop { reason: TurnEndReason },
}

/// A step hook plugs into the agent loop's step boundaries.
///
/// Implementations are stateful (they track progress internally).
/// The loop calls pre_step before each step and post_step after.
pub trait StepHook: Send + Sync {
    /// Called before each step. Returns a decision that controls execution.
    fn pre_step(&mut self, ctx: &PreStepContext) -> StepDecision;

    /// Called after each step completes. Returns whether to continue.
    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow;
}
```

### 3.2 三种策略实现（`src/strategies/`，新建）

#### PlanStrategy — 完整探索（默认，无干预）

```rust
//! Plan strategy: full agent loop, LLM drives freely.

use crate::hooks::*;

pub struct PlanStrategy;

impl StepHook for PlanStrategy {
    fn pre_step(&mut self, _ctx: &PreStepContext) -> StepDecision {
        StepDecision::Proceed { injection: None }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        // Original behavior: if no tool calls, the turn is complete.
        if ctx.had_tool_calls {
            StepFlow::Continue
        } else {
            StepFlow::Stop { reason: TurnEndReason::Completed }
        }
    }
}
```

#### TodoStrategy — LLM 引导 + 步骤约束

```rust
//! Todo strategy: LLM runs with step-by-step guidance.

use crate::hooks::*;
use crate::types::*;
use std::collections::HashMap;

pub struct TodoStrategy {
    steps: Vec<SkillStep>,
    current: usize,
    step_results: HashMap<String, String>,
    variables: HashMap<String, String>,
}

impl TodoStrategy {
    pub fn new(skill: &Skill) -> Self {
        Self {
            steps: skill.steps.clone(),
            current: 0,
            step_results: HashMap::new(),
            variables: skill.variables.clone(),
        }
    }
}

impl StepHook for TodoStrategy {
    fn pre_step(&mut self, _ctx: &PreStepContext) -> StepDecision {
        if self.current >= self.steps.len() {
            return StepDecision::Stop { reason: TurnEndReason::Completed };
        }
        let step = &self.steps[self.current];
        let guidance = format!(
            "You are on step {n} of {total}: `{id}`.\nComplete this step before proceeding.",
            n = self.current + 1,
            total = self.steps.len(),
            id = step.id,
        );
        StepDecision::Proceed { injection: Some(guidance) }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        let step = &self.steps[self.current];
        self.step_results.insert(step.id.clone(), ctx.content.clone());
        self.current += 1;
        if self.current >= self.steps.len() {
            StepFlow::Stop { reason: TurnEndReason::Completed }
        } else {
            StepFlow::Continue
        }
    }
}
```

#### WorkflowStrategy — 确定性 SOP（绕过 LLM）

```rust
//! Workflow strategy: deterministic SOP, bypasses LLM for tool steps.

use crate::hooks::*;
use crate::types::*;
use crate::expr;
use std::collections::HashMap;

pub struct WorkflowStrategy {
    steps: Vec<SkillStep>,
    current: usize,
    step_results: HashMap<String, String>,
    variables: HashMap<String, String>,
    /// Set when pre_step returns ForceTool/ForceLlm, so post_step knows
    /// which step to record the result for.
    active_step: usize,
}

impl WorkflowStrategy {
    pub fn new(skill: &Skill) -> Self {
        Self {
            steps: skill.steps.clone(),
            current: 0,
            step_results: HashMap::new(),
            variables: skill.variables.clone(),
            active_step: 0,
        }
    }
}

impl StepHook for WorkflowStrategy {
    fn pre_step(&mut self, ctx: &PreStepContext) -> StepDecision {
        // Skip steps whose `when` condition is not met.
        loop {
            if self.current >= self.steps.len() {
                return StepDecision::Stop { reason: TurnEndReason::Completed };
            }
            let step = &self.steps[self.current];
            if let Some(when) = &step.when {
                if !expr::evaluate(when, &self.step_results) {
                    log::info!("Workflow step `{}` skipped (condition: {})", step.id, when);
                    self.current += 1;
                    continue;
                }
            }
            break;
        }

        self.active_step = self.current;
        let step = &self.steps[self.current];

        match &step.action {
            StepAction::Tool { tool, args } => {
                let interpolated = expr::interpolate_json(args, &self.step_results, &self.variables);
                let call = ToolCall {
                    id: format!("wf_{}", step.id),
                    name: tool.clone(),
                    arguments: interpolated,
                };
                log::info!("Workflow step `{}`: force tool `{}`", step.id, tool);
                StepDecision::ForceTool { call }
            }
            StepAction::LlmJudge { prompt, input } => {
                let interpolated_prompt = expr::interpolate_str(prompt, &self.step_results, &self.variables);
                let interpolated_input = expr::interpolate_str(input, &self.step_results, &self.variables);
                let full_prompt = format!("{interpolated_prompt}\n\n---\n\n{interpolated_input}");
                log::info!("Workflow step `{}`: force llm_judge", step.id);
                StepDecision::ForceLlm { system: String::new(), prompt: full_prompt }
            }
        }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        let step = &self.steps[self.active_step];
        self.step_results.insert(step.id.clone(), ctx.content.clone());
        self.current = self.active_step + 1;
        if self.current >= self.steps.len() {
            StepFlow::Stop { reason: if ctx.is_error { TurnEndReason::Error } else { TurnEndReason::Completed } }
        } else {
            StepFlow::Continue
        }
    }
}
```

### 3.3 AgentLoop 重构（`src/agent.rs`，修改）

AgentLoop 持有 `Vec<Box<dyn StepHook>>`，在 `run_turn` 中调用钩子：

```rust
pub struct AgentLoop {
    session: SessionLog,
    tools: ToolRegistry,
    llm: LlmClient,
    model: String,
    max_tokens: usize,
    temperature: f32,
    context_window: usize,
    compaction_threshold: f32,
    keep_recent_turns: usize,
    /// Step hooks — control execution flow (strategy, compaction, etc.)
    hooks: Vec<Box<dyn StepHook>>,
}
```

`run_turn` 核心循环改为：

```rust
loop {
    let step = self.session.begin_step();
    let _ = event_tx.send(LoopEvent::StepStart { turn, step }).await;

    // Call all hooks' pre_step. Last non-Proceed decision wins.
    // (In practice, the strategy hook makes the decision; compaction
    //  and other hooks can be added later.)
    let mut decision = StepDecision::Proceed { injection: None };
    let pre_ctx = PreStepContext { turn, step, skill };
    for hook in &mut self.hooks {
        decision = hook.pre_step(&pre_ctx);
        if !matches!(decision, StepDecision::Proceed { injection: None }) {
            break; // A hook made a decisive choice
        }
    }

    let result = match decision {
        StepDecision::Proceed { injection } => {
            // Normal LLM step (current agent.rs logic, optionally with injection).
            if let Some(text) = injection {
                self.session.append(SessionEvent::UserMessage { content: text });
            }
            // [compaction check] — will become a hook in Phase 5
            // derive_messages → build request → stream → collect
            // → append AssistantMessage → execute tool calls
            // Returns (content, is_error, had_tool_calls)
            self.run_llm_step(skill, &assembled, &event_tx, turn, step).await
        }
        StepDecision::ForceTool { call } => {
            // Skip LLM, execute tool directly.
            self.session.append(SessionEvent::ToolCall { call: call.clone() });
            let _ = event_tx.send(LoopEvent::ToolCall { call: call.clone() }).await;
            let result = self.tools.execute_checked(&call, &skill.tools_allow).await;
            self.session.append(SessionEvent::ToolResult {
                call_id: call.id.clone(), content: result.content.clone(), is_error: result.is_error,
            });
            let _ = event_tx.send(LoopEvent::ToolResult {
                call_id: call.id.clone(), content: result.content.clone(), is_error: result.is_error,
            }).await;
            (result.content, result.is_error, false) // no tool calls from LLM
        }
        StepDecision::ForceLlm { system, prompt } => {
            // Single LLM call with independent context (no history).
            let messages = vec![Message::User { content: prompt }];
            let request = LlmRequest { model: self.model.clone(), system, messages, tools: vec![], ... };
            // stream → collect → emit Delta + AssistantMessage
            // Returns (content, false, false)
            self.run_forced_llm(&request, &event_tx).await
        }
        StepDecision::Stop { reason } => {
            self.session.end_step();
            let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;
            self.session.end_turn(reason.clone());
            let _ = event_tx.send(LoopEvent::TurnEnd { turn, reason: reason.clone() }).await;
            return Ok(reason);
        }
    };

    self.session.end_step();
    let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;

    // Call all hooks' post_step.
    let post_ctx = PostStepContext { turn, step, skill, content: result.0, is_error: result.1, had_tool_calls: result.2 };
    let mut flow = StepFlow::Continue;
    for hook in &mut self.hooks {
        flow = hook.post_step(&post_ctx);
        if matches!(flow, StepFlow::Stop { .. }) { break; }
    }

    match flow {
        StepFlow::Continue => {} // loop again
        StepFlow::Stop { reason } => {
            self.session.end_turn(reason.clone());
            let _ = event_tx.send(LoopEvent::TurnEnd { turn, reason: reason.clone() }).await;
            return Ok(reason);
        }
    }
}
```

### 3.4 Dispatcher 简化（`src/dispatcher.rs`，修改）

Dispatcher 不再有三种 run_workflow/run_todo/run_plan 分支，
统一为"构建钩子 → 借给 AgentLoop → 运行"：

```rust
pub async fn dispatch(
    &mut self,
    user_message: String,
    skill: &Skill,
    event_tx: mpsc::Sender<LoopEvent>,
) -> DispatchResult {
    // Build hooks from the skill's mode.
    let hooks: Vec<Box<dyn StepHook>> = match skill.mode {
        ExecMode::Workflow => vec![Box::new(WorkflowStrategy::new(skill))],
        ExecMode::Todo => vec![Box::new(TodoStrategy::new(skill))],
        ExecMode::Plan => vec![Box::new(PlanStrategy)],
    };

    // Lend parts to AgentLoop, attach hooks, run.
    let session = std::mem::replace(&mut self.session, SessionLog::new(0));
    let tools = std::mem::replace(&mut self.tools, ToolRegistry::placeholder());
    let llm = self.llm.clone();

    let mut agent_loop = AgentLoop::new(session, tools, llm, &model_config)
        .with_hooks(hooks)
        .with_compaction(self.compaction_threshold, self.keep_recent_turns);

    let result = agent_loop.run_turn(user_message, skill, event_tx).await;

    // Take parts back.
    let (session, tools, _llm) = agent_loop.into_parts();
    self.session = session;
    self.tools = tools;

    match result {
        Ok(reason) => DispatchResult::Done { mode: skill.mode, reason },
        Err(e) => DispatchResult::Failed { mode: skill.mode, message: e },
    }
}
```

**关键变化**：
- `run_workflow()` / `run_todo()` / `run_plan()` 三个方法全部删除
- `match skill.mode` 从"选择执行路径"变为"构建钩子列表"
- 所有执行都走同一个 `AgentLoop::run_turn()`
- ExecMode 枚举保留（用于选择策略），但不再驱动分支执行

### 3.5 ToolPlugin trait（`src/plugins.rs`，新建）

```rust
//! Tool plugin trait: maps to dsh `ctx.tools.register(defineTool())`.

use crate::types::*;
use serde_json::Value;

/// A tool plugin provides its definition and execution logic together.
pub trait ToolPlugin: Send + Sync {
    /// The tool's schema (name, description, parameters, timeout).
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with JSON arguments. Returns the result.
    fn execute(&self, args: Value) -> ToolResult;
}
```

ToolRegistry 改为持有 `HashMap<String, Box<dyn ToolPlugin>>`：

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolPlugin>>,
    policy: Policy,
}
```

注册改为：
```rust
pub fn register(&mut self, plugin: Box<dyn ToolPlugin>) {
    let def = plugin.definition();
    log::debug!("Registered tool: {}", def.name);
    self.tools.insert(def.name.clone(), plugin);
}
```

执行改为直接调用 trait 方法（保持 spawn_blocking + timeout 管线）。

**共享注册函数**（消除 main.rs + server.rs 重复）：

```rust
/// Register all built-in tools based on config. Called once at startup.
pub fn register_builtins(registry: &mut ToolRegistry, config: &Config) {
    if config.tools.shell {
        registry.register(Box::new(shell::ShellTool));
    }
    if config.tools.file_read {
        registry.register(Box::new(file::FileReadTool));
    }
    // ...
}
```

每个工具从 `definition() + make_executor_fn()` 改为实现 `ToolPlugin` trait：
- `tools/shell.rs`: `pub struct ShellTool; impl ToolPlugin for ShellTool { ... }`
- `tools/file.rs`: `FileReadTool`, `FileWriteTool`, `FileSearchTool`
- `tools/memory.rs`: `MemoryReadTool`, `MemoryWriteTool`, `MemoryRecallTool`

### 3.6 PromptSection 动态注册（`src/prompt.rs`，修改）

```rust
/// A system prompt section with ordering.
pub struct PromptSection {
    pub name: String,
    pub order: i32,
    pub text: String,
}

/// Assemble system prompt from dynamic sections + tool schemas.
pub fn assemble(
    sections: Vec<PromptSection>,
    tools: &[ToolDefinition],
    allow: &[String],
    variables: &HashMap<String, String>,
) -> AssembledPrompt {
    // Sort sections by order, join, interpolate variables.
    // Filter tools by allow-list.
}
```

Section 来源：
- Skill 的 body → `PromptSection { name: "persona", order: 0, text: skill.body }`
- 每个 ToolPlugin 可贡献一个 guidance section（`order: 100 + index`）
- 未来扩展：compaction 状态 section、plan mode section 等

### 3.7 CommandPlugin trait（`src/commands.rs`，新建）

```rust
//! Command plugin trait: maps to dsh `ctx.commands.register()`.

/// A slash command plugin.
pub trait CommandPlugin: Send + Sync {
    /// Command name (without leading /).
    fn name(&self) -> &str;
    /// One-line description for /help.
    fn description(&self) -> &str;
    /// Execute the command. Returns text to display to the user.
    fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult;
}

pub struct CommandContext<'a> {
    pub session_mgr: &'a mut SessionManager,
    pub config: &'a Config,
}

pub struct CommandResult {
    pub text: String,
    pub action: String, // "clear", "new", "none"
    pub session_id: Option<String>,
}
```

内置命令改为插件：
- `ClearCommand` — `/clear`
- `NewCommand` — `/new`
- `ContextCommand` — `/context`
- `CompactCommand` — `/compact`（统一用 compaction.rs，不再粗暴截断）
- `HelpCommand` — `/help`（自动列出所有注册命令）

`handle_command` 改为遍历 `Vec<Box<dyn CommandPlugin>>` 查找匹配的命令。

### 3.8 SubagentTool — 子 agent 委托（`src/subagent.rs`，新建）

这是继承原版"主 agent 智能选择确定性 vs 自主性"模式的核心。
对应原版的 `tool-subagent` + `subagent-spawn-in-process` + `applyChildComposition`。

#### 设计原理

原版的精妙之处：**主 agent 不直接跑 workflow，而是判断问题类型后委托子 agent**。
子 agent 可以用确定性 SOP（workflow skill）也可以自主探索（plan）。
lite 继承这个模式，但更直接——子 agent 直接用 `StepHook` 策略，不需要 LLM 理解 skill 文本。

```
主 Agent (PlanStrategy, LLM 自主决策)
  │
  │  用户: "诊断 192.168.1.1 丢包"
  │  LLM 判断问题类型:
  │    已知问题 → 委托子 agent 跑 workflow skill（确定性，省 LLM）
  │    未知问题 → 委托子 agent 自主诊断（PlanStrategy，灵活）
  │
  ├─ subagent(prompt="检查健康", skill="health-check")
  │    → new AgentLoop(独立 SessionLog, 共享 ToolRegistry + LlmClient)
  │    → WorkflowStrategy: ForceTool/ForceLlm（确定性 SOP，0 次 LLM 理解）
  │    → 返回最终分析结论
  │
  └─ subagent(prompt="诊断丢包根因")
       → new AgentLoop(独立 SessionLog, 共享 ToolRegistry + LlmClient)
       → PlanStrategy: LLM 自主 ping→查接口→查光模块→查对端→结论
       → 返回最终诊断结论
```

#### 与原版的对比

| 维度 | 原版 | lite |
|------|------|------|
| 子 agent 创建 | `ctx.agents.create()`（Cordis scope + preset join） | `AgentLoop::new()`（直接构造） |
| 子 agent 模式 | 继承父 preset，LLM 自主 | **可选**：指定 skill → 确定性 SOP；不指定 → 自主 |
| skill 加载 | LLM 调 `skill` 工具读 `<skill_content>` XML | **直接用 Strategy**，0 次 LLM 理解 |
| 确定性 SOP | LLM 理解 skill 文本后执行（不完全确定） | **ForceTool 100% 确定**（绕过 LLM） |
| 成本 | 子 agent 每次 skill 都要 LLM 调用 | workflow skill 的 tool 步 **0 次 LLM** |
| 隔离 | 独立 session + scope | 独立 SessionLog（临时） |
| 递归控制 | durable depth in SessionHeader | depth 计数器 |
| 并行 | background job + continuable | 只做 foreground（同步等待） |
| 结果返回 | `finalAssistantOutput`（只取最终输出） | 同——子 session 最后一条 AssistantMessage |

#### 关键设计决策

- **子 agent 零父上下文**（对应原版 spawn 的 `inheritsParentContext = false`）：只收到一条 prompt，不看父对话历史
- **共享 ToolRegistry + LlmClient**（对应原版 `applyChildComposition` 继承父 preset）：省内存，不重复创建工具实例
- **skill 参数是 lite 的增强**：原版子 agent 要 LLM 调 skill 工具加载文本再理解；lite 直接用 Strategy，tool 步 0 次 LLM
- **递归深度 MAX_DEPTH=3**（对应原版 `maxDepth`）：子 agent 可以再派子 agent，但最多 3 层
- **只做 foreground**（简化）：单线程串行，同步等待子 agent 完成。后续可加 background
- **事件不冒泡**（对应原版 `isConcurrencySafe`）：子 agent 的中间步骤对父不可见，只返回最终输出

#### 使用示例

skill YAML 定义确定性 SOP（子 agent 用）：
```yaml
# skills/health-check.md
---
name: health-check
description: 设备健康检查标准流程
mode: workflow
think: false
tools:
  allow: [shell]
variables:
  target: "192.168.1.1"
steps:
  - id: ping
    tool: shell
    args: { command: "ping -c 5 {{target}}" }
  - id: cpu
    tool: shell
    args: { command: "top -bn1 | head -5" }
  - id: analyze
    llm_judge: "分析设备健康状态，输出正常/警告/故障 + 原因"
    input: "ping:\n{{steps.ping.result}}\n\ncpu:\n{{steps.cpu.result}}"
---
设备健康检查 SOP。
```

主 agent（Plan 模式）的 LLM 调用：
```json
// LLM 判断这是已知问题，用确定性 SOP
{"tool": "subagent", "arguments": {
  "description": "设备健康检查",
  "prompt": "检查 192.168.1.1 的健康状态",
  "skill": "health-check"
}}
// → 子 agent 用 WorkflowStrategy，4 次 shell + 1 次 LLM = 5 次操作

// LLM 判断这是疑难问题，需要自主诊断
{"tool": "subagent", "arguments": {
  "description": "丢包根因诊断",
  "prompt": "诊断 192.168.1.1 eth0 接口丢包的根因，自主排查"
}}
// → 子 agent 用 PlanStrategy，LLM 自主多步推理
```

#### 内存影响

- 子 agent 的 SessionLog 是临时的：turn 结束后只保留最终输出字符串，SessionLog 丢弃
- 最大并发：单线程串行，同一时刻只有 1 个子 agent 在跑
- 额外内存：1 个 SessionLog（~100KB-1MB）+ 1 个 AgentLoop 结构体（~1KB）
- 不影响 <10MB RSS 目标

## 4. 实现阶段（全部完成 ✅）

### Phase 1: StepHook + AgentLoop 重构（核心） ✅ commit `38039a2eb7`
1. ✅ 新建 `src/hooks.rs` — `StepHook` trait + `StepDecision` / `StepFlow` / context 类型
2. ✅ 新建 `src/strategies/mod.rs` + `plan.rs` + `todo.rs` + `workflow.rs`
3. ✅ 修改 `src/agent.rs` — 持有 `Vec<Box<dyn StepHook>>`，`run_turn` 调用钩子
   - ✅ 抽取 `run_llm_step()` 和 `run_forced_llm()` 辅助方法
4. ✅ 修改 `src/dispatcher.rs` — 删除三分支，统一构建钩子 + 调 AgentLoop
5. ✅ 验证：`cargo test` 全过，workflow 确定性执行行为不变

### Phase 2: ToolPlugin trait ✅ commit `87aa9726f3`
1. ✅ 新建 `src/plugins.rs` — `ToolPlugin` trait（最终落在 `src/tools/mod.rs`）
2. ✅ 修改 `src/tools/mod.rs` — `ToolRegistry` 持有 `Arc<dyn ToolPlugin>`
3. ✅ 改造 `tools/shell.rs` / `file.rs` / `memory.rs` — 实现 `ToolPlugin`
4. ✅ 新建 `register_builtins()` 共享函数，消除 main.rs + server.rs 重复
5. ✅ 修改 `src/main.rs` + `src/server.rs` — 调用 `register_builtins()`
6. ✅ 验证：`cargo test` 全过

### Phase 3: PromptSection 动态注册 ✅ commit `c59e29713a`
1. ✅ 修改 `src/prompt.rs` — `assemble()` 接收 `Vec<PromptSection>`（`build_sections()` + `assemble_sections()`）
2. ✅ 每个 `ToolPlugin` 可贡献 guidance section
3. ✅ 修改 `src/agent.rs` — 从 tools 构建 sections 传给 `assemble()`
4. ✅ 验证：系统提示输出不变

### Phase 4: CommandPlugin trait ✅ commit `37dc908036`
1. ✅ 新建 `src/commands.rs` — `CommandPlugin` trait + `CommandContext` / `CommandResult`
2. ✅ 改造 5 个内置命令为插件实现（Clear / New / Context / Compact / Help）
3. ✅ 修改 `src/server.rs` — `handle_command` 遍历插件列表
4. ✅ 统一 `/compact` 命令用 `compaction.rs`（消除两套实现）
5. ✅ 验证：斜杠命令行为不变

### Phase 5: SubagentTool — 子 agent 委托（继承原版核心模式） ✅ commit `a87cfa073c`
1. ✅ 新建 `src/subagent.rs` — `SubagentTool`（实现 `ToolPlugin`）+ 递归深度控制（MAX_DEPTH=3）
2. ✅ 子 agent = 同进程 `new AgentLoop(独立 SessionLog, 共享 ToolRegistry+LlmClient)`
3. ✅ 子 agent 支持 `skill` 参数：指定则用对应 Strategy（确定性/自主），不指定默认 PlanStrategy
4. ✅ `read_result` 只取子 agent 最终输出（`extract_final_output`），中间步骤不返回父
5. ✅ 注册 `subagent` 工具到 ToolRegistry，Plan 模式的主 agent 可调用
6. ✅ 验证：子 agent 独立 session、确定性 SOP 正常、结果返回父 agent

### Phase 6: Compaction 钩子化 + bug 修复 ✅ commit `3c98e0f257`
1. ✅ Compaction 检查保留在 `agent.rs` 的 `run_llm_step()` 中（阈值 + keep_recent 参数化）
2. ✅ 接通 compaction 消息替换（`session.apply_compaction()` + 重新派生消息）
3. ✅ 修复 `think=false` 仍发 `reasoning_effort:"none"` 的 bug（改为省略字段，即 `None`）
4. ✅ 验证：compaction 正常工作

### Phase 7: Skill Creator — AI 辅助编写 skill ✅ commit `db0703e3ee` → 重构为 skill 文件 `5fec787`
- ✅ ~~`src/skill_creator.rs` — `SkillCreatorTool`（实现 `ToolPlugin`）~~ → 已重构为 `skills/skill-creator.md`（plan 模式 skill 文件）
- ✅ 主 agent 通过 `file_write` 工具直接写 `.md` 文件到 `skills/` 目录，不再依赖编译进二进制的代码工具
- ✅ 内置模板：workflow SOP 模板、plan 诊断模板、todo 引导模板、subagent 编排模板（4 个模板在 skill body 中描述）
- ✅ kebab-case 校验 + 防覆盖保护（由 skill body 中的检查清单指导 agent 执行）
- ✅ 结合编写指南的规范，保证生成的 skill 与 StepHook 架构紧密配合
- ✅ 依赖 Phase 1（StepHook）+ Phase 2（ToolPlugin）完成

### 后续增强（Phase 7 之后）✅ commit `8c000b5a20` + `8cfb24231c`

**1. SubagentTool async 死锁修复**（`8cfb24231c`）
- 问题：`Handle::current().block_on()` 在 `current_thread` runtime 中死锁——`tokio::spawn`（LLM 连接驱动）需要主 runtime 调度器，而调度器被 `spawn_blocking` 阻塞
- 修复：在 blocking 线程上创建独立 `current_thread` runtime（`tokio::runtime::Builder::new_current_thread().enable_all().build()`），不再依赖主 runtime
- LlmClient 是无状态的（只有 base_url + api_key），每次 `stream()` 创建新 TcpStream，安全跨 runtime

**2. 编译告警清零**（`8cfb24231c`）
- 移除 6 个未使用的 import
- 4 个未使用变量加 `_` 前缀
- ~20 个预留 API 加 `#[allow(dead_code)]`
- 结果：29 个告警 → 0 个告警

**3. 斜杠命令自动补全弹窗**（`8c000b5a20`）
- 后端新增 `GET /api/commands` 端点，从 `CommandPlugin` 实例动态返回命令列表（name + description）
- 前端 `web/index.html`：输入 `/` 时弹出命令列表，实时过滤匹配，键盘导航（`↑↓` 切换、`Enter`/`Tab` 选中、`Esc` 关闭）
- 参考原版 dsh 的 `InputTriggerService` slash-menu 模式，用纯前端 JS 轻量实现

**4. 压缩比例可配置**（验证，`8c000b5a20`）
- `config/default.yaml` 中 `compaction: { threshold: 0.7, keep_recent_turns: 4 }` 已存在
- `handle_chat` 每次请求从磁盘重新读 config（`server.rs:595`），修改后即时生效（热重载）
- 配置流：`config.compaction.threshold` → `Dispatcher::with_compaction()` → `AgentLoop::with_compaction()` → `run_llm_step()` 的 `needs_compaction()` 检查
- Web UI 设置面板「通用」页提供压缩滑块，或点「打开配置文件」用系统编辑器编辑 YAML

### 设置面板重设计 + YAML 配置 ✅ commit `1f61e94` → `5fec787`

**1. 配置格式 TOML → YAML**
- `Cargo.toml`：`toml = "0.8"` → `serde_yaml = "0.9"`
- `config/default.toml` → `config/default.yaml`
- 所有 `toml::from_str`/`toml::to_string` 改为 `serde_yaml::` 等价

**2. 设置面板 3 页签重构**
- 通用：语言（pill 选择器）+ 外观（cube）+ 统计栏 + 压缩滑块 + 配置文件路径
- 模型：provider-centric 折叠卡片（参考原版 dsh ModelsSection），收起显示密钥状态圆点，编辑展开 API Key + 可折叠自定义设置
- 工具：每个工具一行开关，SSH 开关展开设备管理 UI（添加/编辑/删除设备目标）

**3. 全局「打开配置文件」按钮**
- 弹窗 header 右上角，调用 `POST /api/config/open`，后端用系统编辑器打开 YAML 文件
- 界面不再承载原始配置编辑功能

**4. skill_creator 从代码工具改为 skill 文件**
- 删除 `src/skill_creator.rs`（465 行），移除所有注册和 policy 引用
- 新增 `skills/skill-creator.md`（plan 模式 skill），agent 通过 `file_write` 生成 skill 文件

## 5. Skill 编写指南

### 5.1 三种模式的选择

| 场景 | 模式 | think | 原因 |
|------|------|-------|------|
| 流程已知、步骤固定（巡检、备份、配置） | `workflow` | `false` | 确定性执行，0 次 LLM，100% 可重复 |
| 流程已知、内容需 LLM 判断（解析日志、分析输出） | `workflow` + `llm_judge` 步 | `false` | tool 步绕过 LLM，judge 步单次 LLM |
| 流程大致知道、细节需 LLM 填充（按步骤排查） | `todo` | `false` | LLM 带步骤引导执行 |
| 问题未知、需自主探索（疑难诊断、根因分析） | `plan` | `true` | LLM 自主规划+执行+反思 |

### 5.2 Workflow 模式编写规范

**核心原则**：每一步要么是 `Tool`（确定执行，0 LLM），要么是 `LlmJudge`（单次 LLM 判断）。

```yaml
---
name: interface-health-check
description: 网络接口健康检查标准 SOP（ping + 接口状态 + 错误计数 + 分析）
mode: workflow
think: false
tools:
  allow: [shell]
variables:
  target: "192.168.1.1"
  iface: "eth0"
steps:
  # Tool 步：确定执行，不调 LLM（扁平结构，不要嵌套 action:）
  - id: ping
    tool: shell
    args:
      command: "ping -c 10 {{target}}"
    # when 条件：可选，满足才执行
    when: "steps.init.result length > 0"

  - id: interface_status
    tool: shell
    args:
      command: "ip link show {{iface}}"

  - id: error_counters
    tool: shell
    args:
      command: "ip -s link show {{iface}}"

  # LlmJudge 步：单次 LLM 调用，独立上下文（llm_judge 直接在 step 下）
  - id: analyze
    llm_judge: |
      你是网络诊断专家。分析以下接口健康数据，输出：
      1. 状态判定：正常 / 警告 / 故障
      2. 关键指标摘要
      3. 如有异常，说明可能原因和建议操作
    input: |
      Ping 结果:
      {{steps.ping.result}}

      接口状态:
      {{steps.interface_status.result}}

      错误计数:
      {{steps.error_counters.result}}
---
网络接口健康检查 SOP。按步骤执行，最后由 LLM 综合分析。
```

**编写要点**：
- `tools.allow` 精确限定：只列该 SOP 用到的工具，减少 LLM 误调用的可能
- `variables` 定义可变参数：不同设备复用同一 SOP，只改变量
- Tool 步的 `args` 支持 `{{var}}` 和 `{{steps.xxx.result}}` 插值
- `when` 条件支持 `contains`、`length >`、`==`、`!=`、`and`、`or`、`not`
- `llm_judge` 的 prompt 要明确输出格式，提高分析准确率
- `think: false` 因为 workflow 不需要 LLM 推理（judge 步除外）
- **步骤用扁平结构**（`tool:` / `llm_judge:` 直接在 step 下），不要用 `action: { tool: }` 嵌套结构

### 5.3 Plan 模式编写规范

**核心原则**：skill body 是 persona（角色指令），LLM 自主规划执行。

```yaml
---
name: fault-diagnosis
description: 网络故障自主诊断（未知问题根因分析）
mode: plan
think: true
tools:
  allow: [shell, file_read, memory_read, memory_write]
---
你是高级网络诊断工程师，负责网元设备的故障根因分析。

## 诊断方法论

按以下思路排查，根据中间结果动态调整方向：

1. **症状确认**：复现问题，收集基本现象
2. **分层排查**：从物理层→数据链路层→网络层→应用层
3. **假设验证**：每个假设都要用命令验证，不猜测
4. **范围收窄**：排除不可能的因素，聚焦可疑点
5. **根因确认**：找到能解释所有症状的单一根因

## 输出要求

- 诊断过程：列出每步命令和发现
- 根因结论：一句话说明根因
- 修复建议：具体可操作的修复步骤
- 如无法确定根因：说明已排除的可能性，建议下一步排查方向

## 可用工具

- `shell`：执行诊断命令
- `file_read`：读取配置文件、日志文件
- `memory_read`/`memory_write`：记录和回忆历史诊断经验
```

**编写要点**：
- `think: true` 让 LLM 开启推理模式，提高复杂问题分析质量
- `tools.allow` 给足工具，让 LLM 有探索空间
- persona body 要写**方法论**而非步骤（plan 模式 LLM 自主决定步骤）
- 明确输出格式，减少 LLM 跑偏

### 5.4 Todo 模式编写规范

**核心原则**：步骤列表引导 LLM 按序执行，但每步内容由 LLM 填充。

```yaml
---
name: config-audit
description: 设备配置审计（按检查项逐项核查）
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
  - id: check_firewall
    tool: shell
    args: { command: "iptables -L -n" }
  - id: check_services
    tool: shell
    args: { command: "systemctl list-units --state=running" }
  - id: summarize
    llm_judge: "汇总配置审计结果，标注不合规项"
    input: "{{steps.check_hostname.result}}\n{{steps.check_routing.result}}\n{{steps.check_firewall.result}}\n{{steps.check_services.result}}"
---
设备配置审计 SOP。逐项检查后汇总。
```

**编写要点**：
- todo 模式的 steps 同时作为 LLM 引导和执行序列
- 每步 LLM 被告知"当前是步骤 N"，按引导执行
- `think: false` 因为步骤已知，不需要 LLM 推理规划

### 5.5 与 SubagentTool 配合使用

skill 可以被主 agent 通过 `subagent` 工具委托给子 agent 执行：

```json
// 主 agent（plan 模式）调用 subagent 工具
{"tool": "subagent", "arguments": {
  "description": "接口健康检查",
  "prompt": "检查 192.168.1.1 的 eth0 接口健康状态",
  "skill": "interface-health-check"
}}
```

**最佳实践**：
- 确定性 SOP → workflow skill + subagent（0 LLM 理解，直接 ForceTool）
- 疑难诊断 → 不指定 skill，subagent 用 plan 自主探索
- 批量巡检 → 主 agent 循环调用 subagent + workflow skill，每台设备一次
- skill 的 `variables` 在 subagent 调用时通过 prompt 隐式传递（子 agent 的 prompt 包含设备信息）

### 5.6 准确率优化技巧

1. **workflow 的 llm_judge prompt 要给输出模板**：
   ```yaml
   prompt: |
     分析以下数据，按此格式输出：
     状态: [正常/警告/故障]
     原因: [一句话说明]
     建议: [具体操作]
   ```

2. **plan 的 persona 要给方法论**：不是"检查网络"，而是"分层排查→假设验证→范围收窄"

3. **tools.allow 要精确**：workflow 只给用到的工具；plan 给足探索空间

4. **variables 参数化**：同一 SOP 复用到不同设备，只改变量不改步骤

5. **when 条件做分支**：根据前序步骤结果跳过或执行后续步骤

6. **think 字段匹配模式**：workflow/todo 用 false（快省），plan 用 true（质量）

## 6. 对比验证

### 5.1 行为一致性验证

重构后必须保证的行为不变量：

| 场景 | 重构前 | 重构后 | 验证方式 |
|------|--------|--------|----------|
| Plan 模式对话 | LLM 自由执行 | PlanStrategy → Proceed(None) → 同 | 手动测试 + 现有测试 |
| Workflow tool 步 | 绕过 LLM，直接执行 | WorkflowStrategy → ForceTool → 同 | `run_workflow` 逻辑等价 |
| Workflow llm_judge | 独立上下文调 LLM | WorkflowStrategy → ForceLlm → 同 | 保持独立上下文 |
| Todo 模式 | 拼步骤列表进 message | TodoStrategy → Proceed(Some) → 同 | 步骤引导注入 |
| 事件流 | TurnStart/StepStart/Delta/... | 同（AgentLoop 统一发） | 事件序列不变 |
| Session log | User/Assistant/Tool 事件 | 同（AgentLoop 统一记） | derive_messages 不变 |
| 工具调用内联 | 按 call_id 匹配 | 同（事件不变） | Web 客户端不变 |

### 5.2 插件化收益验证

| 扩展场景 | 重构前 | 重构后 |
|----------|--------|--------|
| 新增执行模式 | 改 dispatcher match + 加 run_xxx | 实现 StepHook + 加构建分支 |
| 新增工具 | 加 tools/xxx.rs + 改 main.rs + server.rs | 实现 ToolPlugin + 注册一行 |
| 新增提示 section | 改 prompt.rs 固定逻辑 | push PromptSection |
| 新增斜杠命令 | 改 handle_command match | 实现 CommandPlugin + 注册一行 |
| 组合钩子 | 不可能 | push 多个 hook 到 Vec |
| 委托子任务 | 不可能 | 调 subagent 工具，子 agent 可确定性可自主 |
| 主 agent 智能选择 | 不可能 | LLM 自主判断何时用 SOP、何时自主探索 |

### 5.3 体积影响

- 新增 trait 定义 + vtable：~2-5KB
- 删除 dispatcher 三分支重复逻辑：~-2KB
- 净增：极小，不影响 <10MB RSS 目标
- 无运行时框架开销（trait 是零成本抽象的 Rust 编译期机制）

## 7. 与原版的差异（诚实的妥协）

| 原版 | lite | 原因 |
|------|------|------|
| Cordis 框架（运行时插件加载、HMR、scope） | Rust trait（编译期插件） | 嵌入式约束，不可能引入 JS 框架 |
| 声明合并扩展 SessionEventMap | 固定 Rust enum + `Extension` variant | Rust enum 不可运行时扩展 |
| 40+ 能力缝（Definition/Provider/Consumer） | 5 个 trait 扩展点 | 够用，不过度设计 |
| Preset cordis.yml 组合 | Skill YAML 选择 mode | 简化，嵌入式够用 |
| 瀑布流（waterfall，多监听器链式） | 顺序钩子（first decisive wins） | 简化，单策略场景够用 |
| 工具执行 4 阶段管线 | 保留 3 阶段（check/execute/result） | 不变 |
| 子 agent LLM 加载 skill 文本 | 子 agent 直接用 Strategy | 更确定、更省 LLM（0 次理解） |
| 子 agent fork 继承父历史 | 只做 spawn（零父上下文） | 简化，嵌入式子任务应独立 |
| 子 agent background/continuable | 只做 foreground | 单线程，后续可加 |
| InputTriggerService（slash 菜单，Cordis 插件） | 纯前端 JS 弹窗 + `GET /api/commands` | 轻量实现，无框架依赖 |
| compaction 阈值硬编码或配置 | `config.compaction.threshold` 可配置 + 热重载 | 已对齐 |

**lite 继承了原版的"骨"（插件化扩展机制），在嵌入式约束下用 Rust trait 替代 Cordis；
继承了原版的"魂"（主 agent 智能委托子 agent，子 agent 可确定性可自主），用 StepHook 策略替代 LLM 理解 skill 文本；
同时保留了 workflow 确定性执行的"肉"（ForceTool 绕过 LLM，100% 可重复）。**

---

## 8. 实现记录

| 阶段 | commit | 内容 |
|------|--------|------|
| Phase 1 | `38039a2eb7` | StepHook + AgentLoop 重构 + 三个策略 |
| Phase 2 | `87aa9726f3` | ToolPlugin trait + 消除注册重复 |
| Phase 3 | `c59e29713a` | PromptSection 动态注册 |
| Phase 4 | `37dc908036` | CommandPlugin trait + 统一斜杠命令 |
| Phase 5 | `a87cfa073c` | SubagentTool — 子 agent 委托 |
| Phase 6 | `3c98e0f257` | 压缩消息替换 + think=false bug 修复 |
| Phase 7 | `db0703e3ee` | SkillCreatorTool — AI 辅助生成 skill |
| 增强 | `8cfb24231c` | SubagentTool async 死锁修复 + 告警清零 |
| 增强 | `8c000b5a20` | 斜杠命令弹窗 + 压缩比例可配置 |
| 重构 | `1f61e94` | 设置面板 3 页签 + TOML→YAML 配置 + 打开配置文件 |
| 重构 | `5fec787` | skill_creator 从代码工具改为 skill 文件 + SKILL-GUIDE 格式修正 |

全部 7 个阶段 + 后续增强 + 重构已完成。44 个测试通过，0 个编译警告。
