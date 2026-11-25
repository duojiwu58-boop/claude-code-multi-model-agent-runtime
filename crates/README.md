# LS core provenance

The `ls-wire` and `ls-agent-core` sources were copied from the user's local
AntigravityCN working tree on 2026-11-26 (repository HEAD
`0daa342975edda9a9dbe973e236a30e7eeafdc4d`). The source working tree, rather
than HEAD alone, is authoritative; `ls-agent-core/SOURCE.sha256` records the
original core file hashes; `ls-wire/SOURCE.sha256` records the wire originals.
No reference-project files were modified.

No top-level license was present in that reference project. This local reuse
is requested by the execution plan; these crates make no independent upstream
license claim. Confirm redistribution rights before publishing them externally.

Local adaptations: standalone manifests using the bridge's dependency versions;
minimal core library exports without GUI/FFI runtime, workbench, or version
resource build script. Owned process security, Job ownership, pinned TLS,
state merging and interaction validation are retained.

Further adaptations: reqwest 0.13 pinned-root API; default review policy and
bridge-specific data root; prohibition on the reference GUI's data root;
verbatim multi-block user items; opt-in outbound audit; relocated captured test
fixtures. The Host consumes native state directly and never uses a GUI queue.

M5 production Host explicitly uses `mode_tool_config("turbo", true)` and active
state consumption to enable the existing reference AutoApproval/Pacer. The
reusable core review default is retained for callers that explicitly choose it.
Cancellation and recovery still consume passively.
