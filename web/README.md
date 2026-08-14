# DeepSeek Harness Lite — Static Web Client

This directory contains the static web client served by the built-in HTTP server.
Files are embedded into the binary at compile time via `include_str!`.

- `index.html` — single-page UI (sidebar + chat + settings)
- `app.js` — vanilla JavaScript, no framework, no build step

Requirements (from design):
- Chat dialog box with markdown rendering
- SSE streaming for assistant responses and tool events
- Session sidebar with quick switching
- Settings panel (trajectory toggle, skill switch, mode selection)
