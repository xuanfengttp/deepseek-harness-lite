//! DeepSeek Harness Lite — lightweight embedded agent for network element devices.
//!
//! Entry point: parse CLI args, load config, register tools, load skills, and
//! run the agent runtime. P2 adds tri-mode dispatch (workflow/todo/plan):
//! the dispatcher routes each request by the active skill's declared mode.

mod types;
mod session;
mod llm;
mod prompt;
mod policy;
mod skill;
mod agent;
mod expr;
mod memory;
mod compaction;
mod dispatcher;
mod tools;

use crate::types::*;
use crate::session::SessionLog;
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use crate::tools::shell;
use crate::tools::file;
use crate::tools::memory as memory_tool;
use crate::policy::Policy;
use crate::agent::LoopEvent;
use crate::dispatcher::DispatchResult;
use std::env;
use std::process::ExitCode;
use tokio::sync::mpsc;

fn main() -> ExitCode {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && (args[1] == "--version" || args[1] == "-V") {
        println!("dsh-lite {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.len() > 1 && (args[1] == "--help" || args[1] == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    // Use tokio current_thread runtime for minimal memory footprint.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();

    match runtime {
        Ok(rt) => rt.block_on(async_main()),
        Err(e) => {
            eprintln!("Failed to start runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main() -> ExitCode {
    log::info!("DeepSeek Harness Lite v{} — P4 memory + compaction + persistence", env!("CARGO_PKG_VERSION"));
    log::info!("Platform: {} / {}", std::env::consts::OS, std::env::consts::ARCH);

    // Load configuration.
    let config = load_config().unwrap_or_else(|e| {
        log::error!("Config load failed: {e}, using defaults");
        default_config()
    });

    log::info!("Model: {} at {}", config.model.model, config.model.base_url);
    log::info!("Server: {}", config.server.listen);

    // Ensure persistence directories exist.
    if !config.session.persist_dir.is_empty() {
        let _ = std::fs::create_dir_all(&config.session.persist_dir);
    }

    // Register built-in tools.
    let policy = Policy::from_config(&config.tools);
    let mut tools = ToolRegistry::new(policy);

    if config.tools.shell {
        tools.register(shell::definition(), shell::make_executor_fn());
    }
    if config.tools.file_read {
        tools.register(file::read_definition(), file::make_read_executor());
    }
    if config.tools.file_write {
        tools.register(file::write_definition(), file::make_write_executor());
    }
    if config.tools.file_search {
        tools.register(file::search_definition(), file::make_search_executor());
    }
    if config.tools.memory {
        let store = std::sync::Arc::new(memory::MemoryStore::open(
            &config.memory.path,
            config.memory.max_entries,
        ));
        log::info!("Memory store: {} entries", store.len());
        tools.register(memory_tool::read_definition(), memory_tool::make_read_executor(store.clone()));
        tools.register(memory_tool::write_definition(), memory_tool::make_write_executor(store.clone()));
        tools.register(memory_tool::recall_definition(), memory_tool::make_recall_executor(store));
    }

    log::info!("Registered {} tool(s)", tools.definitions().len());

    // Load skills.
    let skills = skill::load_dir(&config.skill.dir);
    if skills.is_empty() {
        log::warn!("No skills loaded from {}; using default skill", config.skill.dir);
    }

    // Validate skills against registered tools.
    let known_tool_names: Vec<String> = tools.definitions().iter().map(|t| t.name.clone()).collect();
    for skill in &skills {
        let warnings = skill::validate(skill, &known_tool_names);
        if warnings.is_empty() {
            log::info!("Skill `{}` validated", skill.name);
        }
    }

    // Determine active skill: CLI `--skill <name>` overrides config `skill.active`.
    let cli_skill_name: Option<String> = parse_cli_skill();
    let skill_name = cli_skill_name.as_deref().or(config.skill.active.as_deref());
    let active_skill = skill::select_by_name(&skills, skill_name)
        .cloned()
        .unwrap_or_else(default_skill);
    log::info!("Active skill: {} (mode: {:?}, think: {})", active_skill.name, active_skill.mode, active_skill.think);

    // Create or load the session log.
    let session = if config.session.checkpoint_turn_end {
        let checkpoint_path = format!("{}/session-current.bin", config.session.persist_dir.trim_end_matches('/'));
        match SessionLog::load(&checkpoint_path, 512) {
            Ok(loaded) => {
                log::info!("Session restored from {}: {} events", checkpoint_path, loaded.len());
                loaded
            }
            Err(_) => {
                log::info!("No session checkpoint; starting fresh");
                SessionLog::new(512)
            }
        }
    } else {
        SessionLog::new(512)
    };
    let llm = LlmClient::new(&config.model);
    let mut dispatcher = crate::dispatcher::Dispatcher::new(session, tools, llm, &config.model);

    // If a prompt was passed on the CLI, run one turn immediately.
    let cli_prompt: Option<String> = env::args().nth(1).filter(|a| !a.starts_with('-'));
    if let Some(prompt) = cli_prompt {
        log::info!("Running single turn with prompt: {prompt}");

        let (event_tx, mut event_rx) = mpsc::channel::<LoopEvent>(128);

        // Spawn event printer.
        let printer = tokio::spawn(async move {
            let mut full_text = String::new();
            while let Some(event) = event_rx.recv().await {
                match event {
                    LoopEvent::TurnStart { turn } => log::info!("[turn {turn} started]"),
                    LoopEvent::StepStart { turn, step } => log::info!("[step {turn}.{step} started]"),
                    LoopEvent::Delta { text } => {
                        print!("{text}");
                        full_text.push_str(&text);
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                    LoopEvent::AssistantMessage { content, tool_calls } => {
                        if !content.is_empty() && full_text.is_empty() {
                            println!("{content}");
                        } else if !content.is_empty() {
                            println!();
                        }
                        if !tool_calls.is_empty() {
                            for tc in &tool_calls {
                                log::info!("[tool call: {} ({})]", tc.name, tc.arguments);
                            }
                        }
                    }
                    LoopEvent::ToolCall { call } => {
                        log::info!("[executing tool: {}]", call.name);
                    }
                    LoopEvent::ToolResult { call_id, content, is_error } => {
                        let label = if is_error { "ERROR" } else { "OK" };
                        log::info!("[tool result {call_id}: {label}]");
                        if !content.is_empty() {
                            // Show first 500 chars of result.
                            let preview = if content.len() > 500 { &content[..500] } else { &content };
                            log::info!("  {preview}");
                        }
                    }
                    LoopEvent::StepEnd { turn, step } => log::info!("[step {turn}.{step} ended]"),
                    LoopEvent::TurnEnd { turn, reason } => log::info!("[turn {turn} ended: {reason:?}]"),
                    LoopEvent::Error { message } => log::error!("[error: {message}]"),
                }
            }
        });

        match dispatcher.dispatch(prompt, &active_skill, event_tx).await {
            DispatchResult::Done { mode, reason } => log::info!("Dispatch done: mode={mode:?}, reason={reason:?}"),
            DispatchResult::Failed { mode, message } => log::error!("Dispatch failed: mode={mode:?}, {message}"),
        }

        // Wait for the printer to finish.
        let _ = printer.await;
    } else {
        // No CLI prompt — P1 scaffold mode (interactive server arrives in P6).
        log::info!("No prompt provided. Use: dsh-lite \"your question here\"");
        log::info!("Interactive web client arrives in P6. For now, pass a prompt as CLI argument.");
    }

    ExitCode::SUCCESS
}

/// Load configuration from the default path, then optional override.
fn load_config() -> Result<Config, String> {
    let path = env::var("DSH_LITE_CONFIG").unwrap_or_else(|_| "config/default.toml".to_string());
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {path}: {e}"))?;
    toml::from_str(&content)
        .map_err(|e| format!("parse {path}: {e}"))
}

/// Fallback default config if file loading fails.
fn default_config() -> Config {
    toml::from_str(include_str!("../config/default.toml"))
        .expect("bundled default config must parse")
}

/// Parse `--skill <name>` from CLI args. Returns the skill name if found.
fn parse_cli_skill() -> Option<String> {
    let args: Vec<String> = env::args().collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--skill" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        if let Some(rest) = args[i].strip_prefix("--skill=") {
            return Some(rest.to_string());
        }
        i += 1;
    }
    None
}

/// A minimal default skill when no skill files are found.
fn default_skill() -> Skill {
    Skill {
        name: "default".into(),
        description: "General-purpose assistant".into(),
        when_to_use: None,
        mode: ExecMode::Plan,
        think: false,
        tools_allow: vec![],
        variables: std::collections::HashMap::new(),
        body: "You are a helpful assistant running on a network element device. Answer concisely.".into(),
        steps: vec![],
    }
}

fn print_help() {
    println!(
        "dsh-lite {} — lightweight embedded agent for network element devices\n\n\
         USAGE:\n    dsh-lite [PROMPT] [OPTIONS]\n\n\
         ARGS:\n    <PROMPT>    Run one turn with this prompt\n\n\
         OPTIONS:\n    -V, --version       Print version\n    -h, --help          Print this help\n    --skill <NAME>      Select skill by name\n\n\
         ENV:\n    DSH_LITE_CONFIG    Path to config file (default: config/default.toml)\n    RUST_LOG           Log level (default: info)",
        env!("CARGO_PKG_VERSION")
    );
}
