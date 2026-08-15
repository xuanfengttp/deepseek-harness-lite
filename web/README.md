# DeepSeek Harness Lite — Static Web Client

This directory contains the static web client served by the built-in HTTP server.
The single `index.html` is embedded into the binary at compile time via `include_str!`.

- `index.html` — single-page UI (sidebar + chat + settings), vanilla JavaScript, no framework, no build step

## Features

- Chat dialog with markdown rendering + SSE streaming for assistant responses and tool events
- Session sidebar with quick switching and multi-session support
- Settings panel with 3 tabs:
  - **General** — language, theme, stats toggle, compaction sliders, custom system prompt, config file path
  - **Model** — provider-centric model cards (add/edit/delete providers, API key status, collapsible advanced settings)
  - **Tools** — per-tool toggles, SSH device management (add/edit/delete device targets)
- Slash command autocomplete popup (`/` trigger, keyboard navigation)
- Trajectory toggle for showing/hiding tool execution details
- Context usage ring indicator
