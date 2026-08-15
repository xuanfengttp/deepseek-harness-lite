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

For small models with limited context windows, compaction is a core feature, not an optional add-on. When derived messages exceed a configurable threshold fraction of the context window, older turns are summarized into a single message using an independent context (the summary request does not include the conversation being summarized).

The threshold is configurable in `config/default.yaml`:

```yaml
compaction:
  threshold: 0.7           # compact when messages exceed 70% of context_window
  keep_recent_turns: 4     # always keep the latest N turns unsummarized
```

Changes take effect on the next chat request (hot-reload — no restart needed). The web UI settings panel provides compaction sliders and a button to open the config file in the system editor.

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
| `ssh_exec` | Execute commands on remote network elements via persistent SSH sessions (connection reuse, interactive queries) |
| `memory_*` | Long-term memory read / write / recall |
| `todo_write` | Task tracking for multi-step operations |
| `subagent` | Delegate a sub-task to a child agent (zero parent context, maxDepth=3) |

Tools are `ToolPlugin` trait implementations registered through `register_builtins()`. Adding a tool means implementing the trait + one registration line — no core code changes. Tools run through a 3-stage pipeline: **check** (permission + validation) → **execute** (with timeout) → **result** (truncation + normalization).

### Layered system prompt

The system prompt is assembled from 5 ordered sections — each short and high-signal, keeping permanent context cost under ~300 tokens:

| Order | Section | Source | Tokens |
|---|---|---|---|
| -100 | Identity | Fixed ("You are an AI agent. Working dir: {{cwd}}.") | ~20 |
| 0 | Persona | Skill body | variable |
| 5 | Custom prompt | User-defined from settings panel | variable |
| 10 | Behavior rules | 3 universal rules (check exit codes, verify facts, be concise) | ~80 |
| 100 | Tool guidance | One behavior rule per allowed tool | ~15/tool |

Each tool has a `guidance` field (how to use it) separate from `description` (what it does) — only `guidance` goes into the system prompt. Runtime variables `{{cwd}}` and `{{model}}` are auto-interpolated. The custom prompt section is optional (leave empty to omit). See [SKILL-GUIDE.md](SKILL-GUIDE.md) §0 for details.

### Custom system prompt

A user-defined system prompt can be injected via the settings panel (General tab → System Prompt). It sits between the persona and behavior rules, supports `{{cwd}}`/`{{model}}` interpolation, and takes effect immediately (hot-reload, no restart). Stored in `config.yaml` under `prompt.custom`.

### SSH remote device operations

The built-in `ssh_exec` tool provides persistent SSH sessions to network elements — connections stay open between calls, enabling interactive device queries (show commands, config retrieval, diagnostics). Devices are pre-configured in the settings panel (Tools tab → SSH toggle → device management) or in `config.yaml`:

```yaml
ssh:
  targets:
    - name: core-router
      host: 192.168.1.1
      port: 22
      user: admin
      password: admin123
```

Skills can use `ssh_exec` with `target` name or inline `host`/`user`/`password`. See [SKILL-GUIDE.md](SKILL-GUIDE.md) §9 for the full SSH usage guide (configuration, call modes, persistent sessions, skill examples).

### Single binary, no runtime dependencies

- Musl static linking — no glibc requirement
- All dependencies are pure-Rust (no C bindings, no cross-toolchain needed)
- Configuration and skills loaded from filesystem; web client embedded at compile time

### Memory footprint

| Metric | Value |
|---|---|
| Runtime RSS | ~6 MB |
| Binary size | ~2.6 MB |
| Target | < 10 MB RSS |

## Architecture

The unified plugin architecture ("one loop + pluggable hooks") replaces the original three hardcoded dispatch branches. All execution modes — `workflow`, `todo`, `plan` — run through a single agent loop, with behavior controlled by `StepHook` strategies. New modes or behaviors are added by implementing a trait, not by editing core loop code. See [DESIGN-UNIFIED.md](DESIGN-UNIFIED.md) for the full design.

```
User input
  → Dispatcher (builds hooks from active skill mode)
     → AgentLoop (single loop, hooks decide per-step behavior)
        ├─ StepHook::pre_step()
        │    ├─ Proceed(injection) → normal LLM step (plan / todo)
        │    ├─ ForceTool(call)    → skip LLM, run tool directly (workflow tool step)
        │    ├─ ForceLlm(prompt)   → independent-context LLM call (workflow llm_judge)
        │    └─ Stop(reason)       → end turn
        ├─ execute tools / stream LLM
        └─ StepHook::post_step() → continue or stop
```

| Mode | pre_step returns | LLM usage | Determinism |
|---|---|---|---|
| `plan` | `Proceed(None)` | Full reasoning each step | LLM-driven |
| `todo` | `Proceed(Some(guidance))` | Per-step with guidance | Medium |
| `workflow` (tool) | `ForceTool(call)` | **0 calls** — bypassed | 100% repeatable |
| `workflow` (llm_judge) | `ForceLlm(prompt)` | Single call, independent context | High |

### Five extension points

| Extension point | Trait | Purpose |
|---|---|---|
| Step hook | `StepHook` | Per-step decision: inject / force tool / force LLM / stop |
| Tool plugin | `ToolPlugin` | Tool definition + execution in one unit |
| Prompt section | `PromptSection` | Dynamic system-prompt sections |
| Command plugin | `CommandPlugin` | Slash command registration |
| Subagent tool | `SubagentTool` | Child agent delegation (inherits dsh core pattern) |

### Module map

| Module | Responsibility | Mirrors dsh |
|---|---|---|
| `types` | Core type definitions + config structs | session + agent + llm types |
| `session` | Append-only event log + message derivation + flash checkpoint + compaction | core/session |
| `llm` | HTTP streaming client (OpenAI-compatible) | llm/llm |
| `prompt` | System prompt assembly with dynamic `PromptSection` | core/system-prompt |
| `hooks` | `StepHook` trait + `StepDecision` / `StepFlow` / context types | agent/pre-step waterfall |
| `strategies` | Three `StepHook` implementations: Plan / Todo / Workflow | (new, replaces dispatch branches) |
| `tools` | `ToolPlugin` trait + `ToolRegistry` + 3-stage execution pipeline | core/tools |
| `commands` | `CommandPlugin` trait + built-in slash commands | interaction/commands |
| `subagent` | `SubagentTool` — child agent delegation (zero parent context, maxDepth=3) | subagent capability |
| `ssh` | Persistent SSH sessions — background connection pool for network elements | (new) |
| `policy` | Allow/deny permission checks | sandbox-policy |
| `skill` | Declarative skill loading (YAML + MD) | skill/skill + skill-filesystem |
| `agent` | Turn/step driver with hook integration + compaction | core/agent-loop |
| `expr` | Condition expression evaluator + variable interpolation | (new) |
| `memory` | Long-term KV store (flash-backed, LRU) | (new) |
| `compaction` | Rolling context summary (independent context, configurable threshold) | (new) |
| `dispatcher` | Builds hooks from skill mode + drives AgentLoop | (new, simplified) |
| `server` | HTTP server + SSE streaming + web client + config hot-reload | (new) |

## Build

```sh
# Prerequisites: Rust 1.75+, cargo-zigbuild + zig for cross-compilation

# Native build
cargo build --release
# → target/release/dsh-lite

# Cross-compile to embedded targets (musl static)
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Supported targets:

| Target | Platform | Binary size |
|---|---|---|
| `x86_64-pc-windows-msvc` | Windows x86_64 | ~2.7 MB |
| `aarch64-unknown-linux-musl` | Linux ARM64 (static) | ~2.7 MB |
| `armv7-unknown-linux-musleabihf` | Linux ARMv7 hard-float (static) | ~2.9 MB |
| `armv7-unknown-linux-musleabi` | Linux ARMv7 soft-float (static) | ~2.9 MB |
| `x86_64-unknown-linux-musl` | Linux x86_64 (static) | ~3.2 MB |

All Linux binaries are statically linked (musl) — no runtime dependencies. See [cross/README.md](cross/README.md) for toolchain setup.

## Release

Push a version tag to trigger automated multi-platform builds and GitHub Release:

```sh
git tag v0.1.0-rc.6
git push origin v0.1.0-rc.6
```

The CI workflow (`.github/workflows/release.yml`) builds all 5 targets in parallel, packages each with `config.yaml` + `skills/` + `README.md`, and creates a GitHub Release with downloadable archives.

For local packaging, use the `packages.ps1` script:

```pwsh
pwsh -File packages.ps1 -Version 0.1.0-rc.6
# → release-packages/dsh-lite-0.1.0-rc.6-{platform}.{zip|tar.gz}
```

See [RELEASE.md](RELEASE.md) for the full release workflow.

## Run

```sh
# Interactive mode (web client)
dsh-lite
# → opens HTTP server at http://127.0.0.1:3081
#   open in browser for chat + markdown + session sidebar + trajectory toggle

# Single-turn mode (pass a prompt)
dsh-lite "check interface status"

# Select a specific skill
dsh-lite --skill interface-diagnostics "eth0 is down"
```

The agent loads config from `config/default.yaml`, scans `skills/` for skill
files, and dispatches requests through the active skill's mode.

Configuration:

```yaml
# config/default.yaml
model:
  base_url: "http://127.0.0.1:8080/v1"
  model: "your-model"
  context_window: 8192

compaction:
  threshold: 0.7
  keep_recent_turns: 4

tools:
  ssh_exec: true

ssh:
  targets:
    - name: core-router
      host: 192.168.1.1
      port: 22
      user: admin
      password: admin123

skill:
  dir: skills
```

The model endpoint is OpenAI-compatible (`/v1/chat/completions` with streaming). All config changes are hot-reloaded on the next chat request — no restart needed. The config file can be opened in system editor via the settings panel.

## Project status

The unified plugin architecture (DESIGN-UNIFIED.md, 7 phases) is **complete**. All extension points are implemented and tested: `StepHook`, `ToolPlugin`, `PromptSection`, `CommandPlugin`, `SubagentTool`. 46 tests pass, 0 compiler warnings.

| Phase | Content | Status |
|---|---|---|
| P0 | Scaffold + cross-compilation | ✅ Done |
| P1 | Core agent loop (plan mode) | ✅ Done |
| P2 | Tri-mode dispatch (workflow/todo/plan) | ✅ Done |
| P3 | Skill system completion | ✅ Done |
| P4 | Memory + compaction + persistence | ✅ Done |
| P5 | Session management (multi-session + offloading) | ✅ Done |
| P6 | Web client + HTTP server | ✅ Done |
| **Unified 1** | **StepHook + AgentLoop refactor + 3 strategies** | **✅ Done** |
| **Unified 2** | **ToolPlugin trait + eliminate registration duplication** | **✅ Done** |
| **Unified 3** | **PromptSection dynamic registration** | **✅ Done** |
| **Unified 4** | **CommandPlugin trait + unified slash commands** | **✅ Done** |
| **Unified 5** | **SubagentTool — child agent delegation** | **✅ Done** |
| **Unified 6** | **Compaction message replacement + think bug fix** | **✅ Done** |
| **Unified 7** | **Skill creator skill file — AI-guided skill generation via plan-mode skill** | **✅ Done** |
| **Post-7** | **Slash command autocomplete popup + compaction ratio config** | **✅ Done** |
| P7 | SSH persistent interactive sessions | ✅ Done |
| **Post-7+** | **Workflow+Subagent skill examples + compaction GUI slider + SSH tool toggle** | **✅ Done** |
| P8 | Size + memory optimization | ✅ Verified (2.63 MB binary) |
| **Post-8** | **Layered system prompt (5 sections) + tool guidance field** | **✅ Done** |
| **Post-8+** | **Custom system prompt (settings panel) + SSH documentation + remote-health-check skill** | **✅ Done** |

### Slash command autocomplete

Typing `/` in the web input box opens a popup listing all registered commands (fetched from `GET /api/commands`, which reads the `CommandPlugin` list). The popup filters matches as you type, supports keyboard navigation (`↑↓` to cycle, `Enter`/`Tab` to select, `Esc` to close), and inserts the command into the input. This mirrors the original dsh `InputTriggerService` slash-menu pattern in a lightweight frontend implementation.

## License

MIT
