# ADR 0016: Host plugins as capability-scoped WebAssembly components

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0005, 0008
- Scope: Plugin ABI, runtime, loading and grants; UI messages are a separate protocol.

## Context

aim needs runtime extensions in languages that can compile to WebAssembly. Current
WASIp3 guest toolchains still lag, whereas WASIp2 components are usable. Research
measured 3.1 µs pooled instantiation for a trivial component and 4.8 µs for a
file-backed 4 MiB image; 2026 also had 28 wasmtime advisories, including sandbox
escapes (research/extensibility.md, TL;DR and §1.2, lines 5–33, 107–126).

## Decision

- Pin a wasmtime LTS line and take security patches promptly. Accept components only,
  with one Engine per daemon, pooling allocator and precompiled `.cwasm` cache keyed by
  source component hash, wasmtime version and engine config. Compile locally before
  loading; do not deserialize a downloaded precompiled image.
- Define WIT package `aim:plugin@0.1.0` as a synchronous guest world. Its host imports
  are `host`, `kv`, `tools`, `session`, `ui`, `bus` and allowlisted `wasi:http`; the host
  awaits internally on fibers. The exported `plugin` interface supports `init`,
  `on-event`, `call-tool`, `run-command`, `complete`, `on-ui-action`, `render`, `shutdown`.
  Durable state belongs in host `kv`, not guest memory.
- Use semver-matched 0.1.x evolution: add functions and interfaces, freeze record and
  variant shapes per track, and carry new actions through an `ext` case. Reserve 0.2
  for WASIp3 async streams and model-provider plugins, keeping a 0.1 host adapter
  (research/extensibility.md, §B.3–4, lines 438–465).
- Store capability grants per plugin hash. Imports without grants link to deny stubs;
  call-time checks enforce scopes such as `tools.call:<glob>`, `ui.surface:<placement>`,
  `bus:<topic>` and `net.http:<host>`. Plugin tools return through the dispatcher;
  plugins receive no ambient fs or process access. An agent-authored plugin cannot
  self-grant above its session ceiling.
- Load global `~/.aim/plugins` by default. Project `.agents/plugins` require explicit
  hash-pinned trust. Ship a Rust SDK for compiled components and a prebuilt QuickJS
  component that runs JS/TS source as script plugins (research/extensibility.md,
  §B.5 and §B.8, lines 466–485, 535–554).

## Consequences

One component ABI supports compiled and hot-loaded script plugins with bounded grants.
wasmtime increases binary size and carries a patch obligation. New hashes require grant
reconciliation, and 0.1's synchronous guest API cannot expose streaming providers.

## Verification

- M7 tests `plugin_ungranted_import_denied`, `plugin_grant_hash_change`,
  `project_plugin_requires_trust` and `plugin_tool_uses_dispatcher` are to be added.
- M7 plugin benchmark repeats instantiate/call and hot-reload timings on supported
  hosts; the research measurements are design evidence, not aim runtime results.
