# DeepSeek Harness Lite

[English](README.md) | [中文](README.zh-CN.md)

A lightweight, embeddable agent harness rewritten in Rust, derived from the [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) architecture.

## What is this

DeepSeek Harness Lite (`dsh-lite`) is a from-scratch Rust reimplementation that preserves the core architecture of DeepSeek Harness — turn/step agent loop, append-only session log, capability-based tools, declarative skills — while stripping the heavyweight plugin loader, web frontend, and multi-process orchestration layers.

The result is a single static binary with a ~6 MB runtime memory footprint, designed for resource-constrained environments where a full Node.js runtime is impractical.

## Key features

### Tri-mode task dispatch

Every request is routed by the active skill's declared execution mode:

| Mode | Determinism | LLM usage | Mechanism |
|---|---|---|---|
| `workflow` | Highest (SOP) | Minimal — only for judgment steps | Bypasses the agent loop; runs fixed tool steps directly |
| `todo` | Medium (agent-guided) | Per-step guidance | Agent loop runs with skill-constrained step sequence |
| `plan` | Low (exploration) | Full reasoning | Agent plans, executes tools, re-plans based on results |

This avoids the overhead of the full agent loop for deterministic operations, reserving LLM reasoning for genuinely uncertain tasks.

### Think field

Skills declare `think: true/false` to control the model's reasoning mode per task type. Deterministic workflows run with reasoning off (fast, cheap); exploratory diagnostics run with reasoning on (quality-first).

### Declarative skills (Claude-compatible format)

Skills are YAML frontmatter + Markdown body files — the same format used by Claude Code and the original DeepSeek Harness:

```yaml
---
name: interface-diagnostics
description: Diagnose interface issues
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
    llm_judge: "Identify which interfaces are down"
    input: "{{steps.check_status.result}}"
---
# Interface Diagnostics

You are a diagnostic assistant. Follow this flow:
1. Check interface status
2. Analyze anomalies
3. Suggest fixes
```

One skill active at a time — its persona, tools, and instructions are injected into the prompt. Tool schemas are filtered to the skill's allow-list.

### Session log with message derivation

The append-only event log is the single source of truth for model context. `derive_messages()` projects surface events (user / assistant / tool) into the model history. A bounded ring buffer holds recent events in memory; older events are evicted after checkpoint.

**Model-visible ⟺ logged**: anything reaching a model request is reconstructable from the log.

### Compaction (short-context survival)

For small models with limited context windows, compaction is a core feature, not an optional add-on. When derived messages exceed a threshold, older turns are summarized into a single message using an independent context (the summary request does not include the conversation being summarized).

### Two-layer memory

- **Short-term**: session event log (ring buffer + flash checkpoint)
- **Long-term**: configurable KV store (flash by default, bounded, LRU eviction), accessed via `memory_read` / `memory_write` / `memory_recall` tools

### Built-in tools

| Tool | Description |
|---|---|
| `shell` | Execute shell commands (platform-aware: `sh -c` / `cmd /c`) |
| `file_read` | Read file contents |
| `file_write` | Write to files |
| `file_search` | Glob-pattern file search |
| `ssh_exec` | SSH command execution via persistent connection pool (placeholder) |
| `memory_*` | Long-term memory read / write / recall |
| `todo_write` | Task tracking for multi-step operations |

Tools run through a 3-stage pipeline: **check** (permission + validation) → **execute** (with timeout) → **result** (truncation + normalization).

### Single binary, no runtime dependencies

- Musl static linking — no glibc requirement
- All dependencies are pure-Rust (no C bindings, no cross-toolchain needed)
- Configuration and skills loaded from filesystem; web client embedded at compile time

### Memory footprint

| Metric | Value |
|---|---|
| Runtime RSS (P4) | ~6 MB |
| Binary size | ~1.5 MB |
| Target | < 10 MB RSS |

## Architecture

```
User input
  → Dispatcher (routes by skill mode)
     ├─ workflow → fixed step sequence (bypass agent loop)
     ├─ todo     → agent loop + step guidance
     └─ plan     → full agent loop (explore → execute → re-plan)

Agent loop (plan/todo):
  turn/start
    → assemble prompt (persona + tools + variables)
    → step/start → LLM stream → assistant message
      → tool calls → 3-stage pipeline → tool results
    → step/end
    → (pending tools or new input → next step)
  turn/end
```

### Module map

| Module | Responsibility | Mirrors dsh |
|---|---|---|
| `types` | Core type definitions | session + agent + llm types |
| `session` | Append-only event log + message derivation + flash checkpoint | core/session |
| `llm` | HTTP streaming client (OpenAI-compatible) | llm/llm |
| `prompt` | System prompt assembly | core/system-prompt |
| `tools` | Tool registry + 3-stage execution pipeline | core/tools |
| `policy` | Allow/deny permission checks | sandbox-policy |
| `skill` | Declarative skill loading (YAML + MD) | skill/skill + skill-filesystem |
| `agent` | Turn/step driver (plan mode) | core/agent-loop |
| `expr` | Condition expression evaluator + variable interpolation | (new) |
| `memory` | Long-term KV store (flash-backed, LRU) | (new) |
| `compaction` | Rolling context summary (independent context) | (new) |
| `dispatcher` | Tri-mode routing (workflow/todo/plan) | (new) |

## Build

```sh
# Prerequisites: Rust 1.75+, optionally cargo-zigbuild + zig for cross-compilation

# Native build
cargo build --release
# → target/release/dsh-lite

# Cross-compile to embedded targets (musl static)
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
```

Supported targets:

| Target | Architecture |
|---|---|
| `x86_64-unknown-linux-musl` | x86_64 (development) |
| `aarch64-unknown-linux-musl` | ARM 64-bit |
| `armv7-unknown-linux-musleabihf` | ARMv7 hard-float |
| `armv7-unknown-linux-musleabi` | ARMv7 soft-float |

## Run

```sh
# Single-turn mode (pass a prompt)
dsh-lite "check interface status"

# The agent loads config from config/default.toml,
# scans skills/ for skill files, and dispatches
# the request through the active skill's mode.
```

Configuration:

```toml
# config/default.toml
[model]
base_url = "http://127.0.0.1:8080/v1"
model = "your-model"
context_window = 8192

[skill]
dir = "skills"
```

The model endpoint is OpenAI-compatible (`/v1/chat/completions` with streaming).

## Project status

Currently at **P4** (memory + compaction + persistence). See [DESIGN-lite.md](DESIGN-lite.md) for the full design document and roadmap.

| Phase | Status |
|---|---|
| P0 — Scaffold + cross-compilation | ✅ Done |
| P1 — Core agent loop (plan mode) | ✅ Done |
| P2 — Tri-mode dispatch (workflow/todo/plan) | ✅ Done |
| P3 — Skill system completion | ✅ Done |
| P4 — Memory + compaction + persistence | ✅ Done |
| P5 — Session management (multi-session + offloading) | 🔄 Next |
| P6 — Web client + HTTP server | Planned |
| P7 — SSH client | Planned |
| P8 — Size + memory optimization | Planned |

## License

MIT
