# mcp-window-tools

> Maintainers: architecture, internals, and Windows API pitfalls are documented in
> [TECHNICAL.md](TECHNICAL.md) (Chinese).

A personal Windows window/screenshot/input utility set exposed to Claude Code as a **stdio MCP**
server. Standalone crate: it does not depend on the bridge and does not replace the
`gemini-computer` native Computer Use executor. Current version **v0.8.0-experimental**,
16 tools.

The process is **PerMonitorV2 DPI aware**; all coordinates are **physical pixels** and,
unless noted, `x`/`y`-style coordinates are relative to a window's **client area**.

## Tool overview

| Tool | Kind | Purpose |
|---|---|---|
| `list_windows` | observe | Enumerate top-level windows; filter by title substring / class / PID. Returns hwnd, pid, title, class, visibility, minimized, elevation, and screen rect. |
| `get_window_rect` | observe | Query one window's frame and client-area screen rects; exact hwnd or `@active`. |
| `screenshot_window` | observe | Capture a window's rendered content (PNG/JPEG, optional downscale, incremental diff). |
| `screenshot_all` | observe | Full-screen capture per monitor; `method`: `auto` (BitBlt) / `wgc` (composed frame) / `ddapi` (DXGI desktop duplication). |
| `resize_window` | side effect | Resize a precisely identified window (optional move), subject to its own minimum size. |
| `window_activate` | side effect | Request an exact hwnd to the foreground; Windows focus protection may reject it; no input is sent. |
| `pixel_scan_line` | observe | Merge a vertical client-area pixel column into color bands for layout assertions. |
| `pixel_find_color` | observe | Find a target color inside a client-area rect; returns match count and bounding box. |
| `pixel_match_multi` | observe | Multi-point feature matching: anchor + offset chain, AVX2 engine, ~1 ms full-4K scan; client-area (hwnd) or monitor (DDAPI mapped-memory direct scan). |
| `send_input` | side effect | Inject mouse/keyboard input into the foreground window; `move` follows a humanized cubic Bézier trajectory. |
| `send_input_background` | side effect | Background-silent mouse: PostMessageW posting; no focus steal, physical cursor untouched. |
| `send_text_background` | side effect | Background text write: EM_REPLACESEL whole-string replace on EDIT controls, WM_CHAR stream otherwise. |
| `wait_for_window` | observe | WinEvent-driven window-appearance wait (zero polling); returns hwnd, rect, title, class, PID. |
| `inspect_element_at` | observe | UIA hit-test at a client-area point. |
| `inspect_focused_element` | observe | UIA properties of the keyboard-focused element. |
| `get_diagnostics` | observe | Self-check: version, request/error stats, GDI/USER handle counts, recent errors. |

**Window targeting** is exactly one of: `hwnd` (decimal string, `0x` hex accepted) /
`title` (substring) / `pid` (largest visible window of the process). Side-effect and
single-query tools require an exact `hwnd` (`@active` = current foreground window).

## Tool parameters

### `list_windows`

| Param | Type | Default | Notes |
|---|---|---|---|
| `title` | string | — | title substring (case-insensitive) |
| `class` | string | — | class-name substring (case-insensitive) |
| `pid` | integer | — | filter by process PID |
| `visible_only` | boolean | `true` | only visible windows |
| `interactive_only` | boolean | `true` | human-interactive heuristic (not minimized, ≥100×100, not a tool window, has title) |
| `limit` | integer | `100` | max rows, capped at 500 |

Each row reports `hwnd` (decimal string), `pid`, `title`, `class`, `visible`, `minimized`,
`elevated` (true = injection from a non-elevated service would be silently dropped by UIPI),
and the screen rect.

### `get_window_rect`

| Param | Type | Notes |
|---|---|---|
| `hwnd` | string | exact hwnd, or `@active` |

### `screenshot_window`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` / `title` / `pid` | — | — | exactly one; `hwnd` may be `@active` |
| `client_only` | boolean | `false` | client area only |
| `method` | string | `auto` | `auto`=PrintWindow, then WGC on near-black/failure, then screen sampling; `wgc`=Windows Graphics Capture composed frame (experimental) |
| `format` | string | `png` | `png` lossless (pixel assertions); `jpeg` lossy, 60-90% smaller |
| `quality` | integer | `85` | jpeg quality 1-100 |
| `max_dim` | integer | `0` | output longest-edge cap (bilinear downscale); 0 = no limit |
| `incremental` | boolean | `false` | incremental diff (experimental; `method=wgc` + png + `max_dim=0` only) |

Response text annotates the actual source (`fallback=wgc (...)`, `fallback=screen (...)`)
and marks reused cached frames. Minimized windows deterministically return
`WINDOW_NOT_VISIBLE` (fast structured error, no timeout hang).

### `screenshot_all`

| Param | Type | Default | Notes |
|---|---|---|---|
| `method` | string | `auto` | `auto`=screen BitBlt (no cursor); `wgc`=WGC composed frame (with cursor); `ddapi`=DXGI desktop duplication (experimental, no cursor, per-monitor BitBlt fallback annotated) |
| `format` | string | `png` | `png` / `jpeg` |
| `quality` | integer | `85` | jpeg quality |
| `max_dim` | integer | `0` | output longest-edge cap |

One image per monitor with dimensions, `method=`, and actual `source=screen/wgc/ddapi/bitblt`;
a failing monitor does not abort the rest.

### `resize_window`

| Param | Type | Notes |
|---|---|---|
| `hwnd` | string | exact hwnd only (no title/pid) |
| `width` / `height` | integer (≥40) | target size (edge ≤7680) |
| `x` / `y` | integer | optional target screen position |

### `window_activate`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | exact hwnd or `@active` |
| `allow_alt_trick` | boolean | `false` | last-resort Alt-key injection into the current foreground window (side effect) |

Activation ladder: direct → AttachThreadInput → (optional) Alt trick; the response reports
which level took effect (`activation_method`). No mouse/keyboard input is sent otherwise.

### `pixel_scan_line`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | exact hwnd or `@active` |
| `x` | integer | — | scan column (client X) |
| `y_from` / `y_to` | integer | — | row range (client Y, `y_to` exclusive) |
| `tolerance` | integer | `2` | per-channel delta for band merging (0-255) |
| `min_band` | integer | `2` | shorter bands ignored |
| `max_bands` | integer | `256` | band cap (1-256) |
| `method` | string | `auto` | `auto`=screen sampling (occlusion annotation); `wgc`=DWM frame (occlusion-accurate, async) |

### `pixel_find_color`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | exact hwnd or `@active` |
| `x` / `y` | integer | — | region origin (client) |
| `width` / `height` | integer (≥1) | — | region size |
| `color` | string | — | `#RRGGBB` (`#` optional) |
| `tolerance` | integer | `0` | per-channel delta (0-255) |
| `method` | string | `auto` | same as `pixel_scan_line` |

### `pixel_match_multi`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | exact hwnd or `@active` (mutually exclusive with `monitor`; client coords) |
| `monitor` | integer (≥1) | — | monitor index (mutually exclusive with `hwnd`; monitor coords; DDAPI direct scan) |
| `anchor` | object | — | `{x, y, color, tolerance}`: anchor color `#RRGGBB` and expected position (result ordering) |
| `offsets` | array | `[]` | `{offset_x, offset_y, color, tolerance}` chain (≤64 items) |
| `search_rect` | object | full area | `{x, y, width, height}` search region |
| `method` | string | `auto` | hwnd-mode source: `auto` screen (occlusion annotation) / `wgc` DWM frame |
| `max_matches` | integer | `32` | result cap (1-4096) |

Semantics: find every pixel matching the anchor color in `search_rect`, verify the offset
chain per candidate (each offset has its own color/tolerance and must stay inside
`search_rect`), sort by Euclidean distance to the expected anchor, return the first
`max_matches`. The response includes `matches`, `candidate_count`, `match_count`
(pre-truncation), `truncated`, `engine` (avx2/scalar), and `capture_elapsed_ms` /
`scan_elapsed_ms`.

Performance: AVX2 full-4K (3840×2160) anchor scan measured at **~1 ms**
(`cargo test --release -- --ignored` reproduces it); the pathological all-pixels-match
input takes ~20 ms, dominated by result materialization. Monitor mode scans the mapped
DDAPI staging memory directly (no whole-image copy); on DDAPI failure (locked session /
static desktop with no cached frame) it falls back to screen sampling, annotated via
`fallback`.

### `send_input`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | exact hwnd or `@active` |
| `activate` | boolean | `false` | bring to front first (same ladder as `window_activate`) |
| `allow_alt_trick` | boolean | `false` | last-resort activation (side effect) |
| `delay_ms` | integer | `50` | inter-action delay (0-5000) |
| `actions` | array | — | ordered actions (1-128 items) |

| kind | fields | effect |
|---|---|---|
| `move` | `x`, `y`, `steps` | move pointer; `steps` (1-200) interpolation; drag while held |
| `click` / `down` / `up` | `x`, `y`, `button` | press+release / press / release (`left`/`right`/`middle`) |
| `scroll` | `x`, `y`, `delta` | wheel notches (120/notch, positive = up) |
| `key_tap` / `key_down` / `key_up` | `vkey` | virtual key: 0-255 or name (`Enter`, `A`, `F5`, `Left`, `Ctrl`, …) |

Since v0.8.0, `move` follows a **humanized trajectory**: cubic Bézier with random
curvature + ease-in/out speed profile + ±2 px Gaussian jitter per step + Gaussian
inter-step delay jitter (clamped to [0.5, 1.2]×mean), landing exactly on target.

Safety: the window must stay foreground for the whole batch; every coordinate must be in
the current client area; the batch is validated before any real input; `down`/`key_down`
must be balanced within the batch; failure paths auto-release held keys; scheduled delay
≤60 s; `UIPI_BLOCKED` if the target is elevated and this service is not.

### `send_input_background`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | parent window exact hwnd or `@active` |
| `client_x` / `client_y` | integer | — | batch base point (client); actions inherit by default |
| `delay_ms` | integer | `50` | inter-action delay (0-5000) |
| `actions` | array | — | ordered actions (1-64 items) |

| kind | effect |
|---|---|
| `click` / `down` / `up` | press+release / press / release; `button` `left`/`right`/`middle`, default left |
| `right_click` | right press+release |
| `move` | post WM_MOUSEMOVE |

Implementation: `RealChildWindowFromPoint` hit-testing to the deepest child → child-local
coordinates → `PostMessageW` of `WM_MOUSEMOVE`/`WM_LBUTTONDOWN`/`WM_LBUTTONUP` etc.
The response reports the hit child hwnd, class, and local coordinates per action.
**No focus steal, physical cursor untouched, foreground unchanged**; `down` must be
balanced by `up` in the same batch.

Honest boundary: messages are only posted; apps using RawInput/DirectInput or with a
broken message pump may not respond — verify with a screenshot or pixel assertion;
`UIPI_BLOCKED` when the target is elevated.

### `send_text_background`

| Param | Type | Default | Notes |
|---|---|---|---|
| `hwnd` | string | — | target input control hwnd (`@active` allowed) |
| `text` | string | — | text to write (≤4096 chars) |
| `clear_first` | boolean | `false` | clear before write (EDIT controls only) |

Standard EDIT/RichEdit controls: `EM_SETSEL` (optionally select-all) + `EM_REPLACESEL`
system-marshaled whole-string replace — instant, no keystroke dropout; other controls:
`WM_CHAR` stream (UTF-16 code units posted one by one, surrogate pairs split, newlines
normalized to CR). 5 s send timeout against hung threads; no focus steal, keyboard state
untouched. Locate the input control with `inspect_element_at` first.

### `wait_for_window`

| Param | Type | Default | Notes |
|---|---|---|---|
| `title_regex` | string | — | window-title regex (Rust regex syntax, case-insensitive by default) |
| `class` | string | — | class-name substring (case-insensitive) |
| `pid` | integer | — | exact PID |
| `timeout_ms` | integer | `5000` | timeout (1-60000) |
| `case_insensitive` | boolean | `true` | title-regex case sensitivity |

At least one filter (AND-combined). A process-wide singleton WinEvent hook listens for
`EVENT_OBJECT_CREATE` / `EVENT_OBJECT_SHOW` (top-level) / `EVENT_SYSTEM_FOREGROUND` and
wakes the waiter directly; an already-existing match returns immediately
(`source=existing`). Timeout is a normal response with `found=false` (not an error) —
the zero-polling replacement for repeated `list_windows` calls.

### `inspect_element_at` / `inspect_focused_element`

| Param | Type | Notes |
|---|---|---|
| `hwnd` | string | exact hwnd or `@active` (`inspect_element_at` only) |
| `x` / `y` | integer | probe point in client coords (`inspect_element_at` only) |

UIA accessibility-tree queries returning control type (name + type code), name, class,
AutomationId, screen bounding rect, and keyboard-focusability. Chromium accessibility
trees activate lazily; targets without a UIA provider return `UIA_UNAVAILABLE`.

### `get_diagnostics`

No parameters. Returns version, uptime, request/error stats (grouped by tool and error
code), GDI/USER handle counts, and the last 20 errors. For per-request tracing, restart
with `WT_LOG_LEVEL=info`.

## HWND format

Decimal by default; `0x`/`0X` prefix parses as hexadecimal. `@active` is only valid as
the `hwnd` value (`{"hwnd":"@active"}`).

## Error contract

Tool errors keep the MCP shape (`content[text]` + `isError: true`) and add machine-readable
`error_code` and `retryable` fields. Codes are stable (additive only):

| Code | Meaning | Retryable |
|---|---|---|
| `INVALID_ARGUMENT` | missing/out-of-range/malformed args, unbalanced batch | no |
| `LIMIT_EXCEEDED` | size/pixel/batch/schedule budget exceeded | no |
| `WGC_UNAVAILABLE` | WGC unavailable (device/DRM/no composition) | no |
| `DDAPI_UNAVAILABLE` | DDAPI unavailable (locked session/no frame, no cache) | no |
| `UIPI_BLOCKED` | elevated target vs non-elevated service | no |
| `INTERNAL_ERROR` | unexpected internal failure | no |
| `WINDOW_NOT_FOUND` | window gone / invalid handle | yes |
| `WINDOW_NOT_VISIBLE` | exists but invisible / minimized | yes |
| `WINDOW_HUNG` | target not responding | yes |
| `NOT_FOREGROUND` | target not foreground | yes |
| `ACTIVATION_DENIED` | focus protection rejected activation | yes |
| `CAPTURE_FAILED` | GDI capture/encode failure | yes |
| `INPUT_FAILED` | execution-time input failure | yes |
| `UIA_UNAVAILABLE` | UIA service/element unavailable | yes |

## Capture budgets

- `screenshot_window` keeps original size: edge ≤7680 px, total ≤12 MP; use
  `resize_window` first if larger.
- `screenshot_all` keeps up to three UHD 4K (3840×2160) monitors at original size; beyond
  that everything scales by one uniform factor (reported).
- Captures and UIA queries run on worker threads with a 15 s hard timeout; hung windows
  can never stall the service.

## Build & test

```bash
cd mcp/window-tools
cargo build --release
# output: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

```bash
cargo test --locked                       # 91 unit tests
cargo test --release -- --ignored         # 4K scan perf smoke (prints measured ms)
pwsh -File tests/run-e2e.ps1              # E2E regression (87 main + 7 hung; interactive desktop)
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

## Deployment

```powershell
pwsh -File mcp/window-tools/deploy.ps1
# build release → smoke test → stop running MCP process → backup → overwrite
# target: C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe
# registration (~/.claude.json mcpServers):
# "window-tools": {"type": "stdio", "command": "C:\\Program Files\\ClaudeCodeBridge\\claude-mcp-window-tools.exe", "args": []}
```

Claude Code reconnects the MCP on the next tool call — the new binary takes effect
immediately (no session restart needed).

## Protocol

Newline-delimited JSON-RPC 2.0: `initialize` → `notifications/initialized` → `tools/list`
→ `tools/call`; protocol version `2025-11-25` (accepts `2025-06-18` / `2025-03-26`).
Every request is isolated with Rust panic unwinding: an unwindable panic returns a
`-32603` error and the service keeps running. stdout is the only protocol channel;
diagnostics go to stderr or the `WT_LOG_FILE` log file.

## Usage boundary

`window_activate`, `resize_window`, `send_input`, `send_input_background`,
`send_text_background` mutate desktop state; everything else is read-only observation.
Desktop content enters the current session as MCP tool results — use only in trusted
sessions.
