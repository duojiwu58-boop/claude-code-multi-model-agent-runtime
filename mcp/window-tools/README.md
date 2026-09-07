# mcp-window-tools

> Maintainers: architecture, internals, and Windows API pitfalls are documented in
> [TECHNICAL.md](TECHNICAL.md).

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
| `class` | string | — | Window class substring filter (case-insensitive) |
| `pid` | integer | — | Filter by process PID |
| `visible_only` | boolean | `true` | Return only visible windows |
| `limit` | integer | `100` | Maximum entries, cap `500` |

Each row also reports `class` and `minimized` alongside `hwnd`/`pid`/`title`/`visible`/`rect`.

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

### `pixel_scan_line`

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | Exact handle, or `@active` |
| `x` | integer | — | Column to scan (client-area X) |
| `y_from` | integer | — | First row (client-area Y, inclusive) |
| `y_to` | integer | — | Row bound (client-area Y, exclusive) |
| `tolerance` | integer | `2` | Per-channel difference merging adjacent rows (0–255) |
| `min_band` | integer | `2` | Bands shorter than this are dropped |
| `max_bands` | integer | `256` | Band cap (1–256) |

Samples the vertical column `x` from the on-screen window client area and merges consecutive
rows whose colors differ by at most `tolerance` per channel into bands. Each band reports
`y_from`/`y_to` (client-area rows, inclusive), `height`, and the average color `#RRGGBB`.
Useful for layout assertions (panel boundaries, control heights, blank placeholders).
Occluded or minimized areas are sampled as displayed on screen.

### `pixel_find_color`

| Parameter | Type | Notes |
|---|---|---|
| `hwnd` | string | Exact handle, or `@active` |
| `x` / `y` | integer | Region top-left (client-area) |
| `width` / `height` | integer (≥1) | Region size |
| `color` | string | Target color `#RRGGBB` (hash optional) |
| `tolerance` | integer (0–255, default `0`) | Per-channel diff |

Searches the client-area region for matching pixels; returns `match_count` and the minimum
bounding box (`bbox`, null when nothing matches). Complements `pixel_scan_line` for assertions
like "this panel is exactly 74 px tall at y=773".

### `send_input`

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | Exact handle, or `@active` |
| `activate` | boolean | `false` | Request foreground placement first; Windows focus protection may refuse |
| `delay_ms` | integer | `50` | Delay between actions (0–5000) |
| `actions` | array | — | Ordered input actions (below), 1–128 items |

Each action: `{kind, x, y, button, delta, vkey, steps}` — only the fields relevant to the kind
matter. Mouse coordinates are client-area physical pixels; `steps` (1–200) interpolates a
`move` so a held button becomes a drag.

| kind | fields | effect |
|---|---|---|
| `move` | `x`, `y`, `steps` | Move pointer (interpolated when `steps` > 1) |
| `click` | `x`, `y`, `button` | Press + release |
| `down` / `up` | `x`, `y`, `button` | Press or release a button (`left`/`right`/`middle`) |
| `scroll` | `x`, `y`, `delta` | Wheel notch (e.g. 120 per notch, positive = up) |
| `key_tap` / `key_down` / `key_up` | `vkey` | Virtual key: integer 0–255 or a name (`Enter`, `A`, `F5`, `Left`, `Ctrl`, ...) |

**Safety:** the target must remain the foreground window throughout the batch, and every mouse
coordinate must stay inside its current client area. The whole batch is validated before any
input is emitted; `down`/`key_down` must be balanced by `up`/`key_up` in the same batch. Keys
still held when execution fails are released automatically. Scheduled delays are capped at 60
seconds per batch. This is the only tool with significant side effects besides `resize_window`;
invoke it only in trusted conversations.

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

This toolset is for window observation, layout verification, and UI driving. Mouse/keyboard
input (`send_input`) changes real desktop state, so it is gated by a foreground check and
should be invoked only in trusted conversations. There is no clipboard or process control.
Desktop content enters the current Claude Code session as MCP tool results, so invoke it only
in trusted conversations.

`window_activate`, `resize_window`, and `send_input` are state-changing operations;
`list_windows`, `get_window_rect`, `screenshot_window`, `screenshot_all`, `pixel_scan_line`,
and `pixel_find_color` are read-only observation.

## Building

```bash
cd mcp/window-tools
cargo build --release
# Output: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

Tests (14) cover hwnd parsing (decimal, `0x`/`0X` hex, whitespace, negatives, overflow, invalid
values), color parsing, color-band merging, vkey names, input-batch safety constraints, panic
isolation, and the tool list:

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
`2025-06-18` / `2025-03-26`). Every request is wrapped in Rust panic-unwind isolation: an
unwinding panic returns a `-32603` error and the stdio session keeps serving. Native process
failures such as access violations, aborts, and OOM aborts are outside this guarantee.
