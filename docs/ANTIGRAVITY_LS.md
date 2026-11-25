# Antigravity LS autonomous backend

`protocol: antigravity-ls` uses an owned official Antigravity language server.
The bridge controls its lifecycle, while Host and LS run with the actual current
interactive user's limited token. LS executes its own native tools and MCP servers;
the bridge does not return them as client `tool_use` calls. No Antigravity IDE,
AntigravityCN GUI or AgyStudio window is required.

The supported installation provides its executable, version and pinned certificate.
Current acceptance uses Antigravity 2.35.1. Existing login data stays in place.

## Start and select

Build with `cargo build --locked --release --target x86_64-pc-windows-msvc`.
Install the bridge, `claude-ls-host.exe`, `claude-ls-launcher.exe`,
`scripts/invoke-ls-session.ps1` and `scripts/install-ls-session-task.ps1` together.
Run the task installer once with installation privileges. The normal package
installer also registers this task.

`ClaudeCodeBridge-LS-Interactive` has an INTERACTIVE group principal, limited run
level, and **no triggers**. It is a demand-only token/session launcher, not a login
or boot daemon. The bridge's controller selects exactly one active WTS session and
uses Task Scheduler RunEx with that session ID. If the restricted service cannot
query WTS, it accepts only the OS-reported active console session with exactly one
matching Explorer desktop session. No username, SID or session ID is hardcoded;
missing or ambiguous sessions fail closed. No passwords are stored and the bridge
service account remains unchanged.

The launcher calls `CreateEnvironmentBlock` with its actual user token and
inheritance disabled. Host/LS receive that complete user environment, including
`USERPROFILE`, `LOCALAPPDATA`, `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`.
**Proxy settings belong to the user's environment, not the LS provider JSON.**
A missing HTTP/HTTPS proxy variable stops startup; the launcher does not substitute
an agent-shell proxy or hardcode a proxy address/port. An obsolete
`ls.launch.proxy_url` is rejected with a migration message.

Selecting an LS provider starts the user Host and waits for its authenticated
health check and per-launch nonce before committing the route. Selecting another
route stops Host and its owned LS tree. Streaming responses, busy tasks and
uncertain tasks block switching; finish or cancel/resolve them first. The bridge
never adopts an externally running Host or kills unrelated LS instances.

At service startup the saved route is restored. If it selects LS, the lifecycle
worker starts the user task when a valid interactive session is available, retrying
failed startup after five seconds. **Before anyone logs in, LS remains unavailable.**
An exited Host or confirmed dead LS is restarted without replaying user requests.
Existing conversations remain dormant and require explicit resume after restart.

Normal shutdown uses authenticated `/shutdown`. The controller holds the exact
running task instance and stops it on bridge stdin EOF or session loss. A file lease
also expires after ten seconds if the controller disappears; launcher then closes
Host stdin. Host's Windows Job reclaims its owned engine tree. Control files live
under `%PROGRAMDATA%/ClaudeCodeBridge/ls-control`, writable by the bridge service
and administrators and readable by interactive users. They contain no credentials.

Copy `examples/providers/antigravity-ls.example.json` to the provider directory and
set its absolute installation, isolated data, token and workspace paths. Those
paths are configuration, not a user identity: the actual task token is selected
from the active session. Keep data/token ownership consistent with that user.
The user needs access to the engine/workspace; the service only needs token read
access and the control directory, not access to the user's entire engine data.

Remove the legacy `ClaudeCodeBridge-LS.lnk` from Startup and stop the old standalone
Host before managed mode. Host errors and sanitized context go to `bridge-host.log`
in the engine data directory; controller diagnostics go to `ls-control/controller.log`.
Old standalone scripts are retained for maintenance, not used by the managed path.

Native MCP configuration lives in the isolated engine's `config/mcp_config.json`.
Configure that file explicitly; no other application's credentials or data root
are copied. The default root is `%LOCALAPPDATA%/ClaudeCodeBridge/ls-engine`.

## Native Claude input filtering and attachments

Ordinary Claude Code requests go directly to `/v1/messages`. The Rust bridge
filters their content before contacting Host. No Python entry script, separate
input token or metadata envelope is required. The old `metadata.ls_input` bypass
is rejected rather than treated as an alternate source of trusted input.

The filter removes top-level and interleaved system/developer instructions,
client tool definitions, and the captured first-user generated context block.
It preserves original user text, whitespace and multiple blocks, including a
literal reminder supplied by the user. It does not perform keyword replacement
inside user text or file contents. IDE selection content is retained as a named
text document without its client-written instruction wrapper. Browser harness
instructions are removed separately from user-selected browser/terminal data.

The current directory is extracted from the captured Environment section and
passed as native LS workspace data. If no such section exists, the configured
workspace is used. Conflicting or nonexistent directories are errors, and a
workspace change cannot silently reuse an existing session.

Attachments are validated before any turn is dispatched:

- Inline PNG/JPEG/WebP/GIF images go to native LS `media.inlineData/mimeType`;
  image-only messages are supported. Remote image URLs are not fetched.
- Inline text documents preserve their title and complete original text, with
  a neutral file boundary. File contents are never reminder-filtered.
  The captured VS Code 2.1.251 file composer uses `atob` for text data; its UTF-8
  byte strings are decoded once inside the Rust filter. Other producer versions
  are not guessed. Non-UTF-8 native file byte strings fail explicitly; prompt
  text is never subjected to this attachment-only decoding.
- Inline base64 `application/pdf` documents use native `SaveMediaAsArtifact`,
  followed by `SendUserCascadeMessage.media` with the returned URI. Local
  artifacts omit the original bytes from the send; remote artifacts retain them,
  matching the official frontend. PDF-only and mixed messages are supported.
  Each PDF is limited to 10 MiB and checked for a PDF header. URL/file-reference
  PDFs and unsupported document formats fail before dispatch. No text extraction
  or page rendering is performed by the bridge.
- A failed attachment save prevents message dispatch and is never retried
  automatically. Confirmed artifacts from failed pre-dispatch requests are
  cleaned up best-effort; an upload with an unknown outcome may leave an artifact.
  Artifacts from accepted or uncertain sends are retained for LS conversation use.
- Images are limited to 100 per message and 10 MiB decoded bytes per image.
  Original image bytes are preserved without automatic resizing or compression.
- Mixed input allows up to 100 image blocks plus 100 non-image blocks, with
  32 MiB total decoded content. Attachment
  bodies participate in session history checks but are not resent on later turns.

PDF acceptance on LS 2.35.1 / `gemini-4-neon-high` verified a random code read
from a generated PDF, numeric addition, SSE, JSON continuation without reupload,
and a PDF-only turn. See [the isolated run results](validation/2026-11-26-ls-native-pdf/live/result.json)
and [native protocol evidence](validation/2026-11-26-ls-native-pdf/evidence.json).
This run used locally built bridge/Host executables; it did not update the
installed Windows service.

For source verification, set `ls.claude_projects_dir` to the local Claude
`projects` directory readable by the bridge service. The bridge matches the
session UUID and waits up to two seconds for asynchronous transcript writes.
Recorded originals must match the HTTP content after known scaffolding removal;
synthetic and compacted turns do not become user tasks. This setting is optional
for structurally supported ordinary requests. Synthetic-marker collisions need
source verification; unknown reminder variants and unsupported imported or
compacted histories fail explicitly rather than being guessed or replayed.

Coverage is grounded in the local Claude Code 2.1.251 captures under
`tests/fixtures/ls-input`. It is not a claim to recognize every future client,
plugin or slash-command transformation. Standard API bearer authentication
continues to apply; the bridge does not accept arbitrary Host RPCs.

## Approval, cancellation and recovery

`permission_mode` is fixed to `turbo`; other values are rejected. The Host also
runs the reference project's built-in automatic approval algorithm. It approves
supported permission, run-command and file-permission interactions once, with
nonblocking delays based on tool class, content size and recent activity
(320–2800 ms). Stale cards are discarded, unknown outcomes are not retried,
and questions/elicitation/unknown interaction types remain pending for a person.
Cancellation and recovery use passive observation, with no automatic approval.

Model Center displays separate LS model and reasoning selectors from the live
catalog. Reasoning variants select actual model IDs, with no prompt emulation.
Selections persist per native profile in `bridge-state.json` (`ls_models`) and require a new session; existing
sessions with a different model return a conflict. Turbo and automatic approval
are shown as fixed settings. An unavailable Host disables the selectors.

The following bridge APIs require the normal bridge bearer token. All are POST
with a JSON body. An optional `profile` selects a particular LS profile without
changing the active profile.

| Path | Body / result |
| --- | --- |
| `/admin/ls/status` | `{}` → owned engine version/PID and model catalog |
| `/admin/ls/models` | `{}` → native model catalog |
| `/admin/ls/model` | `{profile, previous_model, model}` → validate catalog ID and persist selection |
| `/admin/ls/interactions` | `{}` → sessions, pending interactions, execution status and dormant IDs |
| `/admin/ls/respond` | `{"session":"id","interaction":{...}}` → validated native decision |
| `/admin/ls/cancel` | `{"session":"id"}` → stops that owned tree and verifies fresh idle/background state; `confirmed:false` means unconfirmed |
| `/admin/ls/resume` | `{"session":"id"}` → explicitly reattaches a registered dormant Cascade; sends no user message |

Copy `conversationId`, `trajectoryId`, `stepIndex` and `kind` from the pending
interaction into `interaction`. Add `decision`, for example
`{"mode":"permission","allow":true,"scope":"Once"}` or
`{"mode":"runCommand","confirm":false}`. `filePermission` takes `allow`;
`askQuestion` takes typed answers; unsupported kinds stay waiting and can be
cancelled. Completed, duplicated or mismatched decisions are rejected.

Sessions persist under the private engine root. Registration is written as
uncertain **before** dispatch. A Host restart requires explicit resume; uncertain
operations stay blocked from sending again, even if the engine later becomes
idle. Read status and inspect effects, then explicitly start a new session if
needed. Neither restart nor lost HTTP responses replay a task.

The Host's own authenticated `/shutdown` stops its owned LS before exiting.
It never terminates another application's LS. Stopping the Host affects all its
sessions; per-task cancellation uses `/admin/ls/cancel`.

## Capabilities and limits

| Capability | Behavior |
| --- | --- |
| SSE / JSON | Native incremental state; unique terminal event; errors never emit normal completion |
| Native tools / MCP | Executed by LS; no client tool callbacks |
| Thinking | LS-exposed thinking maps to separate Anthropic thinking blocks and thinking_delta events; assistant thinking is accepted on subsequent turns and excluded from text history |
| Tool progress | Reference presentation maps native status, target, command, output and errors to incremental text blocks; these are status text, not Claude tool cards |
| Usage | Sum distinct native model generations for the current turn; `outputTokens` includes thinking once; cache reads are separate |
| Unknown usage | Integer zero placeholders with explicit `x-ls-usage-status`; never represented internally as measured zero |
| Streaming usage | Start values are provisional; final `message_delta` includes usage and `ls_usage_status` |
| `max_tokens` | Not enforced; bridge warning explicitly reports this limitation |
| Sampling, structured output, forced client tool choice | Rejected when directly requested; not emulated through prompts |
| Images / text documents | Native inline images (including image-only input); named text attachments; complete original content retained |
| PDF, unsupported documents, client tool results | Explicit pre-dispatch errors; no silent omission or guessed conversion |
| OpenAI / token counting / vision-proxy source | Unsupported; cannot silently fall through to another provider |
| Limits | Public HTTP 64 MiB; current decoded input 32 MiB; 100 images at up to 10 MiB each plus 100 non-image blocks; internal Host 128 MiB (canonical originals plus native media); 256 sessions; 5–3600 second turn deadline |

Optional `LS_HOST_AUDIT_FILE` records outbound message JSON for controlled local
acceptance. It contains user text, so enable it deliberately and protect the
destination. It never records transport tokens. Source provenance and local
adaptations are recorded in `crates/README.md`.

For an installed package, register the demand-only task once (installation
privileges), then use Model Center to select or leave the LS route:

```powershell
& 'C:/Program Files/ClaudeCodeBridge/scripts/install-ls-session-task.ps1'
```

The package uninstaller stops the bridge before removing this task.
`manage-ls-host.ps1` only retains Stop/Uninstall for old standalone hosts; it no
longer creates login shortcuts or starts LS outside bridge lifecycle management.

The task runs with the user's interactive token and limited privileges. It needs
a logged-in user and the existing official login; it does not launch the IDE.
Default LS Host port is 18791 to coexist with the legacy ACP Host on 18789.
The token and engine data are retained on uninstall; no credentials are copied.
The 2026-11-26 M5 deployment and rollback evidence is in
`docs/validation/2026-11-26-ls-m5-install/`.


Native Claude control compatibility: output_config.effort and thinking (enabled,
disabled, adaptive) are overridden by the GUI-selected LS model ID, without prompt
emulation. Structured output formats remain rejected. System/developer entries
inside messages are excluded just like top-level system/developer fields. Claude
metadata.user_id conversation identifiers are used for stable session binding.
User-block provenance limitations described above still apply.

Timeline output uses sequential content block indices with one open block at a
time. JSON responses contain the same thinking/text timeline as SSE. Thinking
has an empty signature; the bridge does not fabricate Anthropic authentication.
Only thinking exposed by LS is forwarded. Completed LS tools never become
actionable client tool_use requests. Installed native panel evidence, including
the rendered thought disclosure and real write/read result, is recorded in
`docs/validation/2026-11-26-ls-timeline-installed/REPORT.md`.
