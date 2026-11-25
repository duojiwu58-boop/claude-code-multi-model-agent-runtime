# Rust source layout

The bridge is a single binary crate, but its implementation is split by responsibility:

| File | Responsibility |
| --- | --- |
| `main.rs` | Crate imports, shared constants, source-slice assembly, and the test-module entry |
| `core.rs` | Shared provider, capability, routing-state, and application-state types |
| `runtime.rs` | Console/service entry points, loopback/authentication boundary, Axum router construction, and shutdown handling |
| `provider.rs` | Provider discovery, profile parsing, endpoint inference, clients, and persisted state |
| `codex.rs` | Local Codex app-server configuration, stdio/loopback WebSocket transport, Claude client tools, transcript replay, streaming, and cancellation |
| `codex_state.rs` | Bounded tool/idle-thread retention, session and helper isolation, strict transcript matching, expiry, retry waiting/replay, and continuity metrics |
| `codex_request.rs` | Effective client tool catalog, inline tool changes, request validation, and capability diagnostics |
| `ls_agent.rs` | LS profile parsing, exact-user-input routing and authenticated Host management |
| `ls_host_protocol.rs` | Closed Host command schema and text normalization |
| `ls_agent_state.rs` | Atomic durable session registration and uncertain-operation state |
| `ls_agent_stream.rs` | Append-only text and native usage projections |
| `bin/claude-ls-host.rs` | User-mode owned LS lifecycle, native streams, approvals, cancellation and recovery |
| `codex_stream.rs` | Native item to Anthropic SSE blocks, public summaries, completed-item reconciliation, and cached response replay |
| `codex_tests.rs` | Offline Codex protocol tests and opt-in authenticated integration verification |
| `openai_inbound.rs` | OpenAI Responses/Chat inputs and outputs over the Codex backend |
| `openai_inbound_stream.rs` | Profile routing, native API passthrough, incremental SSE and error contracts |
| `openai_state.rs` | Bounded atomic local Responses/Conversations resources and generation leases |
| `openai_inbound_tests.rs` | HTTP adapters, state, passthrough and SSE regressions (included in Codex tests) |
| `mcp.rs` | Authenticated MCP protocol handling, Kimi Formula tools, and image generation |
| `vision.rs` | Generic Vision Proxy collection, request construction, caching, and injection |
| `admin.rs` | Health, model listing, authenticated profile switching/reload, redacted proxy administration, and shutdown APIs |
| `routing.rs` | Authenticated Anthropic/Responses handlers, route selection, and native Anthropic forwarding |
| `gemini_interactions.rs` | Gemini Interactions request/response translation and continuation state |
| `gemini_background.rs` | Argon unified budgets, background polling, resumable SSE and request-scoped cancellation |
| `gemini_background_tests.rs` | Local mock-server Argon budget, long-decode, replay and cancellation regressions (included in tests.rs) |
| `openai_chat_forward.rs` | OpenAI Chat upstream forwarding and response handling |
| `openai_responses.rs` | OpenAI Responses request/response translation and streaming |
| `openai_background.rs` | Bounded Responses jobs, cancellation, polling and SSE replay |
| `openai_files.rs` | Local multipart file resources, file references and bounded PDF rendering |
| `openai_chat_state.rs` | Stored Chat resources and independently generated multiple choices |
| `openai_capability_tests.rs` | Resource lifecycle, JSON mode, strict tools and background regression tests |
| `gemini_streaming.rs` | Gemini Interactions streaming state machine |
| `anthropic_streaming.rs` | SSE decoding and OpenAI Chat-to-Anthropic streaming translation |
| `token_count.rs` | Native/fallback token counting and provider error normalization |
| `openai_chat.rs` | Anthropic-to-OpenAI Chat translation, provider reasoning policies, tools, and identity handling |
| `tests.rs` | Binary-crate regression tests |
| `windows_service.rs` | Windows Service Control Manager integration |

The production slices currently use `include!` at crate root. This intentionally keeps the first decomposition behavior-neutral: private item paths, cross-feature access, and the existing root-level tests remain unchanged. Convert slices to namespaced Rust modules only when their shared dependencies can be made explicit without a broad `pub(crate)` visibility expansion.

Because `rustfmt` does not reliably discover arbitrary `include!` targets, format and check every Rust source file from PowerShell with:

```powershell
$sourceFiles = Get-ChildItem -LiteralPath 'src' -Filter '*.rs' -File | Select-Object -ExpandProperty FullName
rustfmt --edition 2021 $sourceFiles
rustfmt --edition 2021 --check $sourceFiles
```

Behavioral verification remains:

```powershell
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --locked --release --target x86_64-pc-windows-msvc
```
