# mcp-window-tools

A personal Windows window/screenshot utility set exposed to Claude Code as a **stdio MCP** server.
Standalone crate: it does not depend on the bridge and does not replace the `gemini-computer`
native Computer Use executor.

## Tools

| Tool | Purpose |
|---|---|
| `list_windows` | Enumerate top-level windows, filter by title substring / process PID. Returns each window's hwnd (decimal string), pid, title, visibility, and screen rectangle. |
| `get_window_rect` | Query one window's frame and client-area screen rectangles; pass an exact hwnd or `@active`. |
| `screenshot_window` | Capture the rendered content of a specific window; `client_only` limits it to the client area, `hwnd: "@active"` captures the current foreground window. |
| `screenshot_all` | Full-screen capture per monitor, each returned as a PNG plus a label. |
| `resize_window` | Resize a precisely identified target window (optional move), subject to the target's own minimum size. |
| `window_activate` | Request an exact hwnd to the foreground for manual comparison or layout inspection; Windows focus protection may reject it, and no input is sent. |

Window targeting offers exactly one of: `hwnd` (decimal string, or `0x`/`0X`-prefixed hexadecimal,
accepted since the hex-parse fix), `title` (title substring), or `pid` (the largest visible window
of that process). One call must supply exactly one of these; `resize_window` accepts only the exact
`hwnd` returned by `list_windows`, so it can never retarget a same-named window.

The process is **PerMonitorV2 DPI-aware**; all coordinates are physical pixels.

## Tool parameters

### `list_windows`

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `title` | string | — | Title substring filter (case-insensitive) |
| `pid` | integer | — | Filter by process PID |
| `visible_only` | boolean | `true` | Return only visible windows |
| `limit` | integer | `100` | Maximum entries, cap `500` |

### `get_window_rect`

| Parameter | Type | Notes |
|---|---|---|
| `hwnd` | string | Exact window handle, or `@active` |

Returns the frame rectangle and the client-area rectangle in screen coordinates.

### `screenshot_window`

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | Window handle (decimal, or `0x`-prefixed hex); `@active` allowed |
| `title` | string | — | Window title substring |
| `pid` | integer | — | Window's process PID |
| `client_only` | boolean | `false` | Client area only, excluding title bar and borders |

Exactly one of `hwnd` / `title` / `pid` is required.

### `screenshot_all`

No parameters. One PNG per monitor, plus the label of each display.

### `resize_window`

| Parameter | Type | Notes |
|---|---|---|
| `hwnd` | string | Exact handle returned by `list_windows` |
| `width` | integer (min 40) | Target width |
| `height` | integer (min 40) | Target height |
| `x` | integer | Optional target X |
| `y` | integer | Optional target Y |

This is a side-effectful layout operation. It is constrained by the target window's own minimum size.

### `window_activate`

| Parameter | Type | Notes |
|---|---|---|
| `hwnd` | string | Exact window handle, or `@active` |

Requests foreground placement; it never sends mouse or keyboard input.

## HWND formats

hwnd values are decoded as decimal by default; a `0x`/`0X` prefix selects radix-16 parsing, with a
u64→i64 range check. Invalid or out-of-range values fail with an explicit error naming the value
attempted. `@active` is only valid as an `hwnd` value (e.g. `{"hwnd":"@active"}`); it reads the
current Windows foreground HWND and never does fuzzy title matching.

## Screenshot budgets

- `screenshot_window` keeps the original size, up to 7680 px per side and 12 megapixels total;
  beyond that, shrink the target window with `resize_window` first.
- `screenshot_all` guarantees unaltered output for up to three UHD 4K (3840×2160) displays with a
  total budget of 24,883,200 pixels. Above the budget every monitor is scaled by the same ratio and
  the result reports original and actual sizes.
- `PrintWindow` is not guaranteed for GPU-accelerated, protected, or unresponsive windows; on a
  black screen or failure, use another observation method — the tool never degrades to a full-screen
  capture.

This toolset is for window observation and layout only. It provides no mouse, keyboard, clipboard,
or process control. Desktop content enters the current Claude Code session as MCP tool results, so
invoke it only in trusted conversations.

`window_activate` and `resize_window` are layout operations; every other tool is read-only
observation.

## Building

```bash
cd mcp/window-tools
cargo build --release
# Output: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

Tests (8) cover hwnd parsing (decimal, `0x`/`0X` hex, whitespace, negatives, overflow, invalid
values) and the tool list:

```bash
cargo test --locked
```

## Deployment and registration

```powershell
Copy-Item target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe `
  "C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe"
# In ~/.claude.json under mcpServers add:
# "window-tools": {"type": "stdio", "command": "C:\\Program Files\\ClaudeCodeBridge\\claude-mcp-window-tools.exe", "args": []}
```

Restart the Claude Code session afterwards; new MCP servers load only at session start.

The crate is standalone and independent of the bridge installer: no bridge release packages it,
and MCP registration is per-user in `~/.claude.json`.

## Protocol

Newline-delimited JSON-RPC 2.0 (the same pattern as `claude-computer-host`):
`initialize` → `notifications/initialized` → `tools/list` → `tools/call`; tool failures are
returned as `isError: true` text blocks; protocol version `2025-11-25` (compatible with
`2025-06-18` / `2025-03-26`).
