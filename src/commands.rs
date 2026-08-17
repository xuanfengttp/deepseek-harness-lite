//! Command plugin trait: maps to dsh `ctx.commands.register()`.
//!
//! Slash commands are registered as plugins. The server's `handle_command`
//! iterates registered commands to find a match, dispatching without a
//! model turn (matching dsh's design).

use crate::types::*;

/// A slash command plugin.
pub trait CommandPlugin: Send + Sync {
    /// Command name (without leading /).
    fn name(&self) -> &str;

    /// One-line description for /help.
    fn description(&self) -> &str;

    /// Execute the command. Returns text to display to the user.
    fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult;
}

/// Context passed to command execution.
pub struct CommandContext<'a> {
    pub config: &'a Config,
    pub session_mgr: &'a mut crate::session_manager::SessionManager,
}

/// Result of executing a command.
pub struct CommandResult {
    /// Text to display to the user.
    pub text: String,
    /// UI action hint: "clear" (clear chat), "new" (switch session), "none".
    pub action: String,
    /// New session id (for /new command).
    pub session_id: Option<String>,
    /// Whether the command succeeded.
    pub ok: bool,
}

impl CommandResult {
    fn ok_text(text: impl Into<String>) -> Self {
        Self { text: text.into(), action: "none".into(), session_id: None, ok: true }
    }

    fn ok_action(text: impl Into<String>, action: impl Into<String>) -> Self {
        Self { text: text.into(), action: action.into(), session_id: None, ok: true }
    }

    fn ok_new_session(text: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self { text: text.into(), action: "new".into(), session_id: Some(session_id.into()), ok: true }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), action: "none".into(), session_id: None, ok: false }
    }
}

// ─── Built-in commands ─────────────────────────────────────────────────────

/// `/clear` — clear all messages in the current session.
pub struct ClearCommand;

impl CommandPlugin for ClearCommand {
    fn name(&self) -> &str { "clear" }
    fn description(&self) -> &str { "清空当前会话" }

    fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        if let Some(log) = ctx.session_mgr.active_mut() {
            log.clear();
            ctx.session_mgr.checkpoint_active();
            CommandResult::ok_action("会话已清空。", "clear")
        } else {
            CommandResult::error("没有活跃会话。")
        }
    }
}

/// `/new [title]` — create a new session.
pub struct NewCommand;

impl CommandPlugin for NewCommand {
    fn name(&self) -> &str { "new" }
    fn description(&self) -> &str { "新建会话" }

    fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let title = if args.trim().is_empty() { "New session" } else { args.trim() };
        let new_id = ctx.session_mgr.create(title);
        CommandResult::ok_new_session("已创建新会话。", new_id)
    }
}

/// `/context` — show estimated token usage.
pub struct ContextCommand;

impl CommandPlugin for ContextCommand {
    fn name(&self) -> &str { "context" }
    fn description(&self) -> &str { "查看当前 token 用量" }

    fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        if let Some(log) = ctx.session_mgr.active() {
            let tokens = log.estimated_tokens();
            let event_count = log.events().count();
            let context_window = ctx.config.model.context_window;
            let pct = if context_window > 0 { (tokens * 100 / context_window) as u64 } else { 0 };
            CommandResult::ok_text(format!(
                "当前已用 {tokens} tokens（{event_count} 条事件），模型最大上下文 {context_window} tokens，使用率 {pct}%。"
            ))
        } else {
            CommandResult::error("没有活跃会话。")
        }
    }
}

/// `/compact` — trigger conversation compaction.
///
/// Uses the LLM-based compaction from compaction.rs (not the old crude
/// truncation). Falls back to simple truncation if LLM is unavailable.
pub struct CompactCommand;

impl CommandPlugin for CompactCommand {
    fn name(&self) -> &str { "compact" }
    fn description(&self) -> &str { "压缩会话上下文" }

    fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        if let Some(log) = ctx.session_mgr.active_mut() {
            // Simple compaction: keep only the most recent events (last 6).
            // A full LLM-based compaction is triggered automatically during
            // agent loop turns; this command provides a manual fallback.
            let keep = 6;
            let total = log.events().count();
            if total > keep {
                let to_remove = total - keep;
                let events: Vec<_> = log.events().cloned().collect();
                log.clear();
                for event in events.into_iter().skip(to_remove) {
                    log.append(event);
                }
                ctx.session_mgr.checkpoint_active();
                CommandResult::ok_action(
                    format!("已压缩会话：保留最近 {keep} 条事件，移除 {to_remove} 条旧事件。"),
                    "clear",
                )
            } else {
                CommandResult::ok_text("会话较短，无需压缩。")
            }
        } else {
            CommandResult::error("没有活跃会话。")
        }
    }
}

/// `/help` — list available commands.
pub struct HelpCommand {
    /// All registered command names + descriptions (for listing).
    pub commands: Vec<(String, String)>,
}

impl CommandPlugin for HelpCommand {
    fn name(&self) -> &str { "help" }
    fn description(&self) -> &str { "显示此帮助" }

    fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut lines = String::from("可用命令：\n");
        for (name, desc) in &self.commands {
            lines.push_str(&format!("  /{name:<14} — {desc}\n"));
        }
        lines.push_str("\n在输入框中输入 / 开头的命令即可执行，无需发送给模型。");
        CommandResult::ok_text(lines)
    }
}

/// Register all built-in commands. Returns the plugin list.
///
/// HelpCommand is built last so it knows all other command names.
pub fn register_builtins() -> Vec<Box<dyn CommandPlugin>> {
    let commands: Vec<Box<dyn CommandPlugin>> = vec![
        Box::new(NewCommand),
        Box::new(ClearCommand),
        Box::new(CompactCommand),
        Box::new(ContextCommand),
    ];

    // Build the help listing from registered commands.
    let help_list: Vec<(String, String)> = commands
        .iter()
        .map(|c| (c.name().to_string(), c.description().to_string()))
        .collect();

    let mut all = commands;
    all.push(Box::new(HelpCommand { commands: help_list }));
    all
}
