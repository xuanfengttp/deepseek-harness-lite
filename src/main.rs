//! DeepSeek Harness Lite — lightweight embedded agent for network element devices.
//!
//! Entry point: parse CLI args, load config, and start the agent runtime.
//! This P0 scaffold verifies cross-compilation to musl targets and establishes
//! the binary skeleton. Core modules are added in subsequent phases.

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Minimal logging init; full config-driven logging comes in P1.
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

    log::info!("DeepSeek Harness Lite v{} — P0 scaffold", env!("CARGO_PKG_VERSION"));
    log::info!("Platform: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    log::info!("This is a P0 scaffold. Core modules arrive in P1+.");

    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "dsh-lite {} — lightweight embedded agent for network element devices\n\n\
         USAGE:\n    dsh-lite [OPTIONS]\n\n\
         OPTIONS:\n    -V, --version    Print version\n    -h, --help       Print this help\n",
        env!("CARGO_PKG_VERSION")
    );
}
