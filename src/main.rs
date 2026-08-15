//! DeepSeek Harness Lite — lightweight embedded agent for network element devices.
//!
//! Entry point: parse CLI args, load config, register tools, load skills, and
//! run the agent runtime. P2 adds tri-mode dispatch (workflow/todo/plan):
//! the dispatcher routes each request by the active skill's declared mode.

mod types;
mod session;
mod session_manager;
mod llm;
mod prompt;
mod policy;
mod skill;
mod agent;
mod expr;
mod memory;
mod compaction;
mod server;
mod dispatcher;
mod tools;
mod hooks;
mod strategies;
mod commands;
mod subagent;

use crate::types::*;
use crate::session::SessionLog;
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use crate::policy::Policy;
use crate::agent::LoopEvent;
use crate::dispatcher::DispatchResult;
use std::env;
use std::io::Write;
use std::process::ExitCode;
use tokio::sync::mpsc;

fn main() -> ExitCode {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    // Use a custom format with local time so logs are readable.
    env_logger::Builder::from_env(env_logger::Env::default().filter_or("RUST_LOG", "info"))
        .format(|buf, record| {
            let now = chrono::Local::now();
            let level = record.level();
            let module = record.module_path().unwrap_or("");
            writeln!(buf, "[{} {:5} {}] {}",
                now.format("%Y-%m-%d %H:%M:%S"),
                level,
                module,
                record.args())
        })
        .init();

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
    log::info!("DeepSeek Harness Lite v{} — P6 web client + HTTP server", env!("CARGO_PKG_VERSION"));
    log::info!("Platform: {} / {}", std::env::consts::OS, std::env::consts::ARCH);

    // Load configuration — auto-generate a default config file if none exists.
    let mut config = load_or_init_config();

    // Resolve relative paths against the exe directory (not CWD) so the
    // binary works correctly when double-clicked from any location.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Resolve session persist_dir relative to exe dir.
    if !std::path::Path::new(&config.session.persist_dir).is_absolute() {
        config.session.persist_dir = exe_dir.join(&config.session.persist_dir)
            .to_string_lossy().to_string();
    }
    // Resolve memory path relative to exe dir.
    if !std::path::Path::new(&config.memory.path).is_absolute() {
        config.memory.path = exe_dir.join(&config.memory.path)
            .to_string_lossy().to_string();
    }
    // Resolve skill dir relative to exe dir.
    if !std::path::Path::new(&config.skill.dir).is_absolute() {
        config.skill.dir = exe_dir.join(&config.skill.dir)
            .to_string_lossy().to_string();
    }

    log::info!("Model: {} at {}", config.model.model, config.model.base_url);
    log::info!("Server: {}", config.server.listen);

    // Ensure persistence directories exist.
    if !config.session.persist_dir.is_empty() {
        let _ = std::fs::create_dir_all(&config.session.persist_dir);
    }

    // Ensure the skills directory exists with default skills if missing.
    ensure_skills_dir(&config.skill.dir);

    // Register built-in tools (shared registration function, no duplication).
    let policy = Policy::from_config(&config.tools);
    let mut tools = ToolRegistry::new(policy);
    crate::tools::register_builtins(&mut tools, &config);
    crate::tools::register_subagent(&mut tools, &config);

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
    // If neither is set, use the built-in default (general-purpose assistant)
    // instead of auto-selecting the first skill file (which may be platform-specific).
    let cli_skill_name: Option<String> = parse_cli_skill();
    let skill_name = cli_skill_name.as_deref().or(config.skill.active.as_deref());
    let active_skill = if skill_name.is_some() {
        skill::select_by_name(&skills, skill_name)
            .cloned()
            .unwrap_or_else(default_skill)
    } else {
        default_skill()
    };
    log::info!("Active skill: {} (mode: {:?}, think: {})", active_skill.name, active_skill.mode, active_skill.think);

    // Create session manager (multi-session + offloading + double-page cache).
    let mut session_mgr = session_manager::SessionManager::new(
        &config.session.persist_dir,
        512,
    );
    log::info!("Session manager: {} session(s) in index", session_mgr.len());

    // Create a new session or switch to the most recent one.
    let sessions_list = session_mgr.list();
    let session_id = if let Some(most_recent) = sessions_list.first() {
        log::info!("Resuming most recent session: {} ({})", most_recent.id, most_recent.title);
        session_mgr.switch(&most_recent.id).unwrap_or_else(|| session_mgr.create("New session"))
    } else {
        session_mgr.create("New session")
    };

    // Take the active session log out for the dispatcher.
    let session = session_mgr.take_active().unwrap_or_else(|| SessionLog::new(512));
    let llm = LlmClient::new(&config.model);
    let mut dispatcher = crate::dispatcher::Dispatcher::new(session, tools, llm, &config.model)
        .with_compaction(config.compaction.threshold, config.compaction.keep_recent_turns);

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
                    LoopEvent::Usage { prompt_tokens, completion_tokens, cache_hit_tokens, cache_miss_tokens, ttft_ms, decode_ms } => {
                        let cache_pct = if cache_hit_tokens + cache_miss_tokens > 0 {
                            (cache_hit_tokens * 100 / (cache_hit_tokens + cache_miss_tokens)) as u64
                        } else { 0 };
                        log::info!("[tokens: {prompt_tokens} in (cache hit {cache_pct}%), {completion_tokens} out | ttft {ttft_ms}ms, decode {decode_ms}ms]");
                    }
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

        // Return the session to the manager and checkpoint.
        let session = dispatcher.take_session();
        session_mgr.return_session(session);
        session_mgr.checkpoint_active();
        log::info!("Session checkpointed: {} ({} events)", session_id, session_mgr.active().map(|s| s.len()).unwrap_or(0));
    } else {
        // No CLI prompt — start the interactive web server.
        log::info!("Starting interactive web server at http://{}", config.server.listen);
        log::info!("Active sessions: {} (cached: {})", session_mgr.len(), session_mgr.cached_len());

        println!("\n========================================");
        println!("  DeepSeek Harness Lite v{}", env!("CARGO_PKG_VERSION"));
        println!("  Web GUI: http://{}", config.server.listen);
        println!("  Press Ctrl+C to exit");
        println!("========================================\n");

        let state = std::sync::Arc::new(server::ServerState {
            session_mgr: std::sync::Arc::new(tokio::sync::Mutex::new(session_mgr)),
            skills,
            active_skill_name: std::sync::Arc::new(tokio::sync::Mutex::new(active_skill.name.clone())),
            config: config.clone(),
        });

        if let Err(e) = server::run(&config.server.listen, state).await {
            log::error!("Server error: {e}");
            eprintln!("\n========================================");
            eprintln!("  Server failed to start: {e}");
            eprintln!("  Likely cause: port {} is already in use.", config.server.listen);
            eprintln!("  Please close the other process and try again.");
            eprintln!("========================================\n");
            eprintln!("Press Enter to exit...");
            let mut _buf = String::new();
            let _ = std::io::stdin().read_line(&mut _buf);
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

/// Resolve the config file path.
///
/// Priority:
/// 1. `DSH_LITE_CONFIG` env var (highest)
/// 2. `<exe_dir>/.dsh-lite-path` file contents (user-set via settings)
/// 3. `<exe_dir>/config.yaml` (default, next to the binary — auto-generated on first run)
/// 4. `config/default.yaml` (last-resort fallback for dev/legacy)
pub fn resolve_config_path() -> String {
    if let Ok(p) = env::var("DSH_LITE_CONFIG") {
        return p;
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()));

    // Check user-set path marker
    if let Some(dir) = &exe_dir {
        let path_file = dir.join(".dsh-lite-path");
        if let Ok(content) = std::fs::read_to_string(&path_file) {
            let trimmed = content.trim();
            if !trimmed.is_empty() && std::path::Path::new(trimmed).exists() {
                return trimmed.to_string();
            }
        }
    }

    // Default: config.yaml next to the exe (may not exist yet — will be auto-generated)
    if let Some(dir) = exe_dir {
        return dir.join("config.yaml").to_string_lossy().to_string();
    }

    // Last-resort fallback (dev mode without exe resolution)
    "config/default.yaml".to_string()
}

/// Load configuration from the resolved path.
/// Load config from the resolved path. Public so the HTTP server can
/// re-read the config file on each chat request, picking up settings
/// changes without a restart.
pub fn load_config_file() -> Option<Config> {
    load_config().ok()
}

fn load_config() -> Result<Config, String> {
    let path = resolve_config_path();
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {path}: {e}"))?;
    serde_yaml::from_str(&content)
        .map_err(|e| format!("parse {path}: {e}"))
}

/// Load config, or generate a default config file on first run.
///
/// If the resolved config file does not exist, write a fresh default to
/// `<exe_dir>/config.yaml` (or the configured path) and load from it.
/// This ensures the settings panel always has something to show.
fn load_or_init_config() -> Config {
    let path = resolve_config_path();

    // If the file exists and parses, just use it.
    if std::path::Path::new(&path).exists() {
        return load_config().unwrap_or_else(|e| {
            log::error!("Config parse error in {path}: {e}, using defaults");
            default_config()
        });
    }

    // File doesn't exist — generate one from the compiled-in default.
    log::info!("Config file not found at {path}, generating default...");
    let default_content = include_str!("../config/default.yaml");

    // Try to write it. If the path has a parent dir that doesn't exist,
    // create it. If writing fails (e.g. read-only), fall back to defaults.
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::write(&path, default_content) {
        Ok(()) => {
            log::info!("Generated config file: {path}");
            load_config().unwrap_or_else(|e| {
                log::error!("Generated config failed to parse: {e}");
                default_config()
            })
        }
        Err(e) => {
            log::warn!("Cannot write config to {path}: {e}, using in-memory defaults");
            default_config()
        }
    }
}

/// Fallback default config if file loading fails.
fn default_config() -> Config {
    serde_yaml::from_str(include_str!("../config/default.yaml"))
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

/// Ensure the skills directory exists next to the exe. The lite version ships
/// NO built-in skills — the directory starts empty and the user adds their own
/// `.md` skill files. The system auto-discovers and registers them on startup.
fn ensure_skills_dir(dir: &str) {
    let path = std::path::Path::new(dir);
    if path.exists() {
        return; // Already exists (may be empty or user-populated).
    }
    if let Err(e) = std::fs::create_dir_all(path) {
        log::warn!("Failed to create skills directory {}: {e}", path.display());
        return;
    }
    log::info!("Created empty skills directory: {} (add .md skill files here)", path.display());
}

/// The fallback used when no skill is selected. This is NOT a skill — it is a
/// bare general-purpose assistant with no domain-specific instructions. The lite
/// version ships no built-in skills; the user defines their own in the skills/
/// directory and the system auto-discovers them on startup.
fn default_skill() -> Skill {
    Skill {
        name: "default".into(),
        description: "General-purpose assistant".into(),
        when_to_use: None,
        mode: ExecMode::Plan,
        think: false,
        tools_allow: vec![],
        variables: std::collections::HashMap::new(),
        body: "You are a helpful assistant. Answer concisely and accurately.".into(),
        steps: vec![],
    }
}

fn print_help() {
    println!(
        "dsh-lite {} — lightweight embedded agent for network element devices\n\n\
         USAGE:\n    dsh-lite [PROMPT] [OPTIONS]\n\n\
         ARGS:\n    <PROMPT>    Run one turn with this prompt\n\n\
         OPTIONS:\n    -V, --version       Print version\n    -h, --help          Print this help\n    --skill <NAME>      Select skill by name\n\n\
         ENV:\n    DSH_LITE_CONFIG    Path to config file (override; default: exe-dir/config.yaml)\n    RUST_LOG           Log level (default: info)",
        env!("CARGO_PKG_VERSION")
    );
}
