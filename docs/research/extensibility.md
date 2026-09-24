# R5 — Extensibility runtimes: WASM plugins, portable declarative UI, code mode

Research date: 2026-09-25. Refs are cited as `refs/<repo>/<path>:<line>`, where `refs/` is the shared scratchpad clone. The web sources were checked on this date. Measurements come from `scratchpad/ext-lab/`, a throwaway crate, run on an Apple M3 Ultra with Rust 1.98.1. Anything marked `UNVERIFIED` has not been confirmed.

## TL;DR

1. **Host: wasmtime, components only.** 49.0.1 is current; 36 and 48 are LTS (every 12th release, 24 months). WASI 0.3 was ratified 2026-06-11 and has been on by default since wasmtime 46. Extism (no Component Model), Wasmer (declined WASIp2) and wasmi (no Component Model) are not viable.
2. **Guest toolchains lag the host.** Rust `wasm32-wasip2` is mature (about 16 KB components), but wasip3 is Tier 3 on stable until about Rust 1.100 (Nov 2026). ComponentizeJS is about 8–11 MB, "experimental" and sync-only. componentize-py was about 35 MB in 2024 (`UNVERIFIED` now). Go is early; C# is preview. **So `aim:plugin@0.1` should be a sync WASIp2-style world; the host awaits internally via fiber-based `func_wrap_async`.**
3. **Versioning lever, verified in source:** a guest import `@0.1.0` links to any host `@0.1.y` in both directions. `0.0.x` and pre-release versions never match (`wasmtime-environ-49.0.1/src/component/names.rs:279-316`). Types must still match exactly, so within a track only add interfaces or functions, and keep one host adapter per track (the Zed pattern).
4. **Measured wasmtime costs:**
   - Instantiate + 1 call: 3.1 µs pooled, 12.8 µs on-demand.
   - With a 4 MiB data segment: 4.8 µs from an mmapped `.cwasm`, 387 µs from memory. Off Linux, copy-on-write needs a file-backed image (verified in source).
   - Host call round trip: 89 ns. Epoch or fuel instrumentation: about 2× on a worst-case loop. Binary: +11.4 MB.
5. **Security signal:** 28 wasmtime advisories in 2026, against about 4 a year before (gh API). Two were critical sandbox escapes. `aarch64-apple-darwin` is only Tier 2. Pin LTS, patch fast, and treat grants as the blast-radius limit.
6. **pi (v0.87.1) is the bar:** 40 events, tools with renderers, commands, shortcuts, flags, renderers, providers, a bus, and a rich `ctx.ui`. **Its UI does not survive a wire:** RPC mode drops component factories (`refs/pi/packages/coding-agent/src/modes/rpc/rpc-mode.ts:194-230`). oh-my-pi has the same limit, since its web client shows extension tools as generic JSON.
7. **aim's daemon↔UI split therefore forces declarative UI.** A2UI is the only standard that is data, streams, and already has terminal renderers (community `a2ui-tui` for ratatui, a2ui-ink). MCP Apps and the OpenAI Apps SDK are iframe HTML: web-only, with a text fallback in the terminal.
8. **Recommended UI protocol:** an A2UI-v1.0-shaped envelope with a flat id-keyed component list and JSON-Pointer data, plus an aim terminal catalog and placements (status, widgets, panels, overlays, transcript, tool rows). Built-in UI uses it too, so parity comes by construction. Themes are DTCG 2025.10 tokens with a terminal `$extensions` block.
9. **Codex already ships JS code mode:** an `exec` tool running a V8 isolate with a `tools` global and no fs/net, plus `wait`, in a separate host process. The model catalog carries `tool_mode: direct|code_mode|code_mode_only` and per-model `exec` templates (`refs/codex/codex-rs/protocol/src/openai_models.rs:346-349,703-721`).
10. **Claude's native programmatic tool calling is Python** (GA; tools as async Python functions). **aim should support JS and Python behind one tool-bridge contract.**
11. **Measured runtimes:**
    - rquickjs 0.14 (QuickJS-ng 0.16.2): +1.0 MB; 109 µs cold; 10.7 µs per awaited Rust future; exact interrupt deadline.
    - Monty 1.0.0-beta.3 (pure-Rust Python subset): +8.1 MB; 3.9 µs trivial run; about 1.5 µs per pause/resume; a paused run serialises to 5.3 KB.
    - Neither has ambient I/O.
12. **Code mode recommendation:** a crash-isolated `aim-coderun` worker behind a `CodeRuntime` trait (rquickjs for JS/TS, Monty for Python). Every `tools.*` call re-enters the harness dispatcher, so permissions, hooks, SSH shadowing and audit are shared. Running the engines inside wasm is a phase-2 backend.
13. **Typed surfaces:** generate `.d.ts` and `.pyi` from tool JSON Schemas (Codex caps each tool at 16 KB). Keep only a compact index in the prompt, with `ALL_TOOLS`/`describe`/`search` lazily. Evidence: 150k → 2k tokens (Anthropic), 2,500 endpoints in about 1k tokens (Cloudflare).
14. **Saved programs:** git repos at `~/.aim/programs` and `.agents/programs`, each program with a `program.toml` (params, tools, grants, provenance), synced to a private GitHub remote. Retrieval is FTS5 plus vectors, then one Jev `Client::ask` rerank (jevgrep pattern). A program runs with saved grants ∩ current policy.
15. **MVP order:**
    1. Event/action-fold core using tny ADR 0028's vocabulary (the Verus target).
    2. UI protocol with both renderers.
    3. JS code mode.
    4. wasmtime plugin host + Rust SDK + JS script-plugin runtime.
    5. Monty, programs, Jev.

## Findings

### 1. WASM plugin host

#### 1.1 wasmtime (facts)

**Versions and release policy**
- **Version:** 49.0.1 on crates.io. It needs Rust 1.96 (`cargo info wasmtime`).
- **Default features:** `async, cache, component-model, component-model-async, cranelift, pooling-allocator, wat, gc, threads, stack-switching, …`.
- **`wasmtime-wasi 49.0.1` default features:** `[p1, p2, p3]`. `wasmtime-wasi-http 49.0.1` defaults to `[p2, p3]`.
- **Release cadence:** a new major every month on the 20th. "Each release that is a multiple of 12 is considered an LTS release and is supported for 24 months. Other releases are supported for 2 months." Currently supported: 49, 48 (LTS) and 36 (LTS). https://raw.githubusercontent.com/bytecodealliance/wasmtime/main/docs/stability-release.md

**WASI 0.3**
- "WASI 0.3.0 … shipping on June 11, 2026 following a WASI Subgroup vote."
- 0.3.1 shipped 2026-08-11 and added `map<K,V>`. Later 0.3.x patch releases follow a two-month train.
- Changes from 0.2: `async func`, `stream<T>` and `future<T>` are added and `wasi:io` is removed; `wasi:http` gains `service`/`middleware` worlds; stdio becomes `stream<u8>`.
- Sources: https://raw.githubusercontent.com/bytecodealliance/wasi.dev/main/docs/roadmap.md (lines 14, 20, 41) and https://wasi.dev/releases/wasi-p3
- wasmtime 46.0.0 made both defaults: "Wasmtime now supports WASI 0.3.0 by default and the `component-model-async` wasm feature is now enabled by default" (RELEASES.md release-46.0.0, verified by curl).
- Caveats: the Component Model itself is "not at phase 4" but is on by default. The `Config::wasm_component_model_async` docstring still says "very incomplete", which looks stale. A WASIp3 advisory, GHSA-x84v-gj2h-g759, was published on 2026-08-20.

**Host API (verified in `~/.cargo/registry/.../wasmtime-49.0.1`)**
- `LinkerInstance::func_wrap` is synchronous (`src/runtime/component/linker.rs:504`).
- `func_wrap_async` (`:545`) is async on the host but appears blocking to the guest, implemented with fibers.
- `func_wrap_concurrent` (`:609`) takes `Fn(&Accessor<T>, Params) -> Pin<Box<dyn Future…>>`. The guest may invoke it concurrently if it lowers the import `async`. It requires `Config::concurrency_support`.
- `TypedFunc::post_return` is deprecated as a no-op (`func/typed.rs:360-363`).
- `Config::async_support` was removed in 42.0.0.

**Interruption and limits**
- Epochs "result in faster execution … up to 2-3x" compared with fuel. Use fuel only when "deterministic yielding or trapping is needed".
- Epoch gaps: they do not interrupt code blocked in a host call, bulk memory operations check once, and they are incompatible with Winch.
- `StoreLimitsBuilder` offers `memory_size`, `table_elements`, `instances`, `tables` and `memories`. The last three default to 10,000; memory is unlimited by default.
- WASI now bounds host-side allocations by default (42.0.0) and denies TCP/UDP sockets by default (48.0.0).
- Source: docs.rs/wasmtime/49.0.1 `Config`, `StoreLimitsBuilder` (sub-agent).

**Pooling allocator and caching**
- Pooling reserves "roughly 4G of virtual memory per … linear memory slot", which caps it at about 32k slots, and its limits are fixed up front.
- Deallocation is a single `madvise`, and copy-on-write images make instantiation cheap.
- `Component::serialize` + `unsafe Component::deserialize{,_file}` accept the same wasmtime version only. Deserialisation is "only lightly validated", so it must never be fed untrusted bytes, and a mapped file must not change afterwards (`component.rs:242,281,623`).

**Semver resolution (verified in source)**
- Rustdoc: "if you define `a:b/c@0.2.1` in a Linker but a component imports `a:b/c@0.2.0` then that import will resolve to the `0.2.1` version". It works in reverse too. Several tracks may coexist (`linker.rs:26-58`).
- The rule, from `alternate_lookup_key` (`wasmtime-environ-49.0.1/src/component/names.rs:279-316`):

  | Version | Resolves to |
  |---|---|
  | `1.x.y` | `@1` |
  | `0.M.x` (M ≠ 0) | `@0.M` |
  | `0.0.x` | no alternates |
  | any pre-release | no alternates |

- Types are not lenient: "Items defined in this linker must match the component's imports precisely" (Linker docs).
- Semver-aware since 19.0.0 (#7994).

**Security posture**
- A bug counts as a vulnerability only on Tier 1 platforms and features.
- Tier 1 targets are just `x86_64-{linux-gnu, apple-darwin, windows-msvc}`. **`aarch64-apple-darwin` and `aarch64-linux` are Tier 2** because they lack continuous fuzzing.
- Advisories per year: 2021: 4, 2022: 8, 2023–25: 4 each, **2026 so far: 28** (verified with `gh api repos/bytecodealliance/wasmtime/security-advisories`). These include two critical sandbox escapes on 2026-04-09 (CVE-2026-34987 in Winch, CVE-2026-34971 an aarch64 Cranelift miscompile) and a high-severity WASI filesystem escape on 2026-08-20. The latest batch (2026-09-24) was patched in 49.0.1, 48.0.3 and 36.0.16.
- Sources: https://docs.wasmtime.dev/stability-tiers.html and https://github.com/bytecodealliance/wasmtime/security/advisories .

**Pulley, the portable interpreter**
- Tier 2. Cranelift emits Pulley bytecode, and it "will never be as fast as native Cranelift".
- Use it only where JIT is forbidden (for example iOS) or where no Cranelift backend exists.

**Hot reload**
- No official guide. The documented building blocks: a new `Component` → `InstancePre` → atomic swap for new calls; old `Store`s drain and drop; `epoch_deadline_trap` forces stragglers out; `Component::same` (49.0.0) detects a no-op reload.
- Wizer is now built in as `wasmtime wizer` / the `wasmtime-wizer` crate (since 39.0.0).

**Zed, the closest analogue**
- It keeps WIT snapshots `since_v0.0.1 … since_v0.8.0`, each with its own host `bindgen!` module (`enum Extension { V0_8_0(..), V0_6_0(..), … }`), and reads a `zed:api-version` custom section to pick one.
- It uses wasmtime 48, `epoch_interruption(true)` with a 100 ms ticker, `epoch_deadline_async_yield_and_update(1)`, and incremental compilation.
- Source: https://github.com/zed-industries/zed/blob/HEAD/crates/extension_host/src/wasm_host/wit.rs
- Spin 4.1.0 runs wasmtime 49 with WASIp3 enabled.

#### 1.2 Measurements (ext-lab, `src/bin/wasm_bench.rs`, release build, thin LTO)

| Measurement | Value |
|---|---|
| `Engine::new` on-demand / pooling (1000 slots, 16 MiB max memory) | 0.74 ms / 5.46 ms |
| Compile a trivial component (WAT, Cranelift) / one with a 4 MiB data segment | 6.2 ms / 37.6 ms |
| Serialised `.cwasm` size: trivial / 4 MiB data | 51 KB / 4.26 MB |
| `deserialize` in memory vs `deserialize_file` (mmap), 4 MiB component | 428 µs vs **55 µs** |
| Store + `InstancePre::instantiate` + 1 call, trivial: on-demand / pooling | 12.8 µs / **3.1 µs** |
| Same, 4 MiB data, component from memory: on-demand / pooling | 387 µs / 377 µs |
| Same, 4 MiB data, **mmapped `.cwasm`**: on-demand / pooling | 15.5 µs / **4.8 µs** |
| Typed host→guest call that calls back into the host (s32→s32) | 89 ns |
| 300M-iteration tight loop: none / epoch / fuel | 125 / 249 / 239 ms |
| Epoch trap with a deadline of N ticks | trapped exactly at tick N (see note) |
| Binary delta vs hello world (runtime, cranelift, wat, component-model, pooling, async) | +11.35 MB |

- **Off Linux, copy-on-write only works from a file (verified in source).** `MemoryImageSource::from_data` returns `None` on non-Linux; Linux uses memfd (`wasmtime-49.0.1/src/runtime/vm/sys/unix/vm.rs:119-128`). A file-backed mmap is used on any unix (`src/runtime/vm/cow.rs:105-127`). Heavy guests, such as JS engines or CPython with large data segments, therefore need precompile-to-disk plus `deserialize_file`, or instantiation costs about 0.4 ms per 4 MiB.
- **The loop overhead is a worst case** (an instrumented back-edge on every iteration); real code is lower.
- **Timing caveat:** this sandbox's `thread::sleep(10 ms)` actually took 73 ms, so wall-clock epoch-ticker latencies measured here are meaningless. The tick-count semantics are what was verified.

#### 1.3 Alternatives (facts)

| Runtime | Version | Component Model | Fit |
|---|---|---|---|
| extism | 1.30.0 (2026-06-04; built on wasmtime 43, `wasmtime-wasi` p1 only) | **No**; own ABI + PDK; issue #666 open since 2024 | Only if polyglot PDKs mattered more than WIT. Activity is dependency bumps only. |
| wasmer | 7.4.2 | **No**; WASIX/WAI focus; WASIp2 issue #4439 closed as not planned | No |
| wasmi | 2.0.0 (interpreter, `no_std`, JIT-bomb resistant, fuel) | **No**, and no plans | Embedded or iOS core-module plugins only |
| wasmtime + Pulley | 49 | Yes | The no-JIT fallback inside the same API |

#### 1.4 Guest toolchains producing components today (facts; sizes are hello-world or adder)

| Language | Path | Maturity | Artifact / startup |
|---|---|---|---|
| Rust | `wasm32-wasip2` (Tier 2 since 1.82) + `wit-bindgen 0.62` (async/stream support via `async:` option) | Mature. `cargo-component` "in the process of being deprecated". `wasm32-wasip3` is Tier 3 on stable 1.98 and Tier 2 on nightly, "first available on stable in Rust 1.100.0", and needs wasi-sdk-34 / LLVM 23. | About 16 KB release |
| JS/TS | `jco componentize` = ComponentizeJS 0.23 / StarlingMonkey (SpiderMonkey); jco 1.35 | "Experimental". Imports "can only be synchronous" (may be stale). Wizer pre-init; Weval AOT via `--aot`. | About 8 MB embedding; book example core is 10.6 MiB |
| JS (QuickJS) | `componentize-qjs` (quickjs-ng via rquickjs + wit-dylib + Wizer; `jco componentize --backend qjs`) | Very young (started 2026-02, about 10 stars) | `UNVERIFIED` |
| Javy | QuickJS → **core module, "not a Wasm component"** (verified, javy `docs/docs-using-exports.md:9-10`) | Stable (CLI 9.1.0) | At least 869 KB static; 1–16 KB dynamic |
| Python | componentize-py 0.25.1 (embeds CPython) | Usable | About 35 MB in 2024; current size `UNVERIFIED` |
| Go | `componentize-go` 0.4.3 (upstream Go `-buildmode=c-shared` wasip1, then componentize; async tag for WASI 0.3) | Early. TinyGo component tooling "not currently being maintained". Upstream `GOOS=wasip2` #65333 is backlog; wasip3 port #77141 is not accepted. | `UNVERIFIED` |
| C/C++ | wasi-sdk-34 (`wasm32-wasip2-clang` emits a component; wasip3 target present) | Mature | Small, KB-scale (`UNVERIFIED`) |
| C# | componentize-dotnet 0.8.0-preview (NativeAOT-LLVM experimental) | Preview | `UNVERIFIED` |
| MoonBit | `wit-bindgen moonbit`, then `wasm-tools component embed/new` (documented in component-docs) | Documented | `UNVERIFIED` |
| Zig / Grain | No upstream wit-bindgen generator; core module + `wasm-tools component new` + adapter | `UNVERIFIED` | `UNVERIFIED` |

#### 1.5 pi's extension surface: the bar (facts, pi 0.87.1)

**Loading and trust**
- TS/JS modules with a default factory `(pi: ExtensionAPI) => void | Promise`, loaded **in-process via jiti** with no compile step (`refs/pi/packages/coding-agent/src/core/extensions/loader.ts:2,45`; `docs/extensions.md:15-38`).
- They have full OS authority: "same operating-system permissions" (`docs/extensions.md:5`).
- Project extensions are gated by a `project_trust` event (`:50`).
- Reload replaces the whole runtime (`:50`).

**Events: 40 `pi.on` overloads** (`src/core/extensions/types.ts:1370-1436`)

| Group | Events |
|---|---|
| Trust / resources | `project_trust`, `resources_discover` → extra skill/prompt/theme paths |
| Session | `session_start` (reason startup/reload/new/resume/fork), `session_info_changed`, `session_before_switch`, `session_before_fork`, `session_before_compact` (return a custom compaction), `session_compact`, `session_compact_failed`, `session_shutdown`, `session_before_tree`, `session_tree` |
| Context | `context`, `context_with_system` (rewrite messages sent to the LLM) |
| Provider | `cache_warming_decision`, `before_provider_request` (replace payload), `before_provider_headers`, `after_provider_response`, `provider_stream_event` |
| Agent | `before_agent_start` (edit structured system-prompt sections or replace them), `agent_start`, `agent_end`, `agent_before_settle` (append entries + `continue`), `agent_settled` |
| UI prompts | `ui_prompt_start`, `ui_prompt_end` |
| Turn | `turn_start`, `turn_end` (boundary: append entries, continue) |
| Message | `message_start`, `message_update`, `message_end` (replace) |
| Tool execution | `tool_execution_start`, `tool_execution_update`, `tool_execution_end` |
| Selection | `model_select`, `thinking_level_select` |
| Tool control | `tool_call` (mutate input, block, terminate), `tool_result` (compose content/details/isError) |
| Input | `user_bash` (replace execution), `input` (continue/transform/handled) |

Result types are at `:1222-1300`.

**Registration** (`:1443-1622`)
- `registerTool`: TypeBox params, `execute(id, params, signal, onUpdate, ctx)`, `renderCall` / `renderResult` returning TUI Components, `promptSnippet`, `promptGuidelines`, `executionMode`, `prepareArguments` (`:461-518`).
- `registerCommand` with `getArgumentCompletions` (`:1342-1348`).
- `registerShortcut`, `registerFlag`.
- `registerMessageRenderer`, `registerEntryRenderer`, `registerMarkdownTransformer`.
- `registerProvider` with `streamSimple` and OAuth; `unregisterProvider`.

**Actions:** `sendMessage` (steer/followUp/nextTurn), `sendUserMessage`, `appendEntry`, `setSessionName`, `setLabel`, `exec`, `get`/`setActiveTools`, `getCommands`, `setModel`, `setThinkingLevel`, and `events` (the inter-extension bus).

**`ctx.ui`** (`:143-298`)
- Dialogs: `select`, `confirm`, `input`, `editor` (each with a timeout).
- Status and chrome: `notify`, `setStatus(key)`, `setWorkingMessage`/`Indicator`, `setWidget(key, string[] | factory, placement above/below editor)`, `setFooter`/`setHeader(factory)`, `setTitle`.
- Custom components and editor: `custom(factory, {overlay, overlayOptions})`, `setEditorComponent`, `addAutocompleteProvider`, `onTerminalInput`.
- Themes: `setTheme`, `getAllThemes`.
- Command contexts add `newSession`, `fork`, `navigateTree`, `switchSession`, `reload`, `waitForIdle` (`:365-404`).

**TUI component model and themes**
- Components are imperative: `render(width): string[]` of ANSI lines, plus `handleInput`, `handleMouse`, `invalidate` (`refs/pi/packages/tui/src/tui.ts:111-136`).
- Themes are JSON: 56 color roles (51 required), `vars`, and oklch/hex/256-index colors (`docs/themes.md`; `src/modes/interactive/theme/theme-schema.json`).

**Portability limit: pi's UI does not survive the wire.**
- RPC mode forwards only dialogs, `notify`, `setStatus`, `setWidget(string[])`, `setTitle` and `set_editor_text`.
- `custom()` returns undefined. Footer, header, editor and autocomplete are no-ops. `setTheme` fails.
- Sources: `docs/rpc-extension-ui.md:10-26`; `src/modes/rpc/rpc-mode.ts:194-230` ("Component factories are not supported in RPC mode").

**oh-my-pi additions** (from a sub-agent report; I re-checked `types.ts:462-469`, `registry.ts:74-75`, `extension-ui-controller.ts:148-149` and `session/code-mode.ts:1-5`; other citations were not re-checked)
- **46 event overloads.** New: `session_stop` (block, or continue up to 8×), `before_subagent_spawn` (reroute or block), `tool_approval_requested`/`resolved`, `mcp_notification`, `goal_updated`, `credential_disabled`. Absent under pi's names: `model_select`, `agent_settled`.
- **New registrations:** `registerComposerShape`, thinking renderers, file write/delete fallbacks.
- **New tool fields:** `loadMode` (essential/discoverable), `approval: read|write|exec`, `deferrable`.
- **New context methods:** `ctx.invokeTool`, `ctx.runEphemeralTurn`.
- **Plugins and marketplace:** plugins via `package.json#omp` (extensions, tools, hooks, commands, features, typed `settings` schema) and a Claude-Code-compatible marketplace (`refs/oh-my-pi/packages/coding-agent/src/extensibility/plugins/types.ts:9-95`; `docs/marketplace.md`).
- **No sandbox and no project-trust gate:** `isProjectTrusted()` always returns true (`…/extensions/types.ts:462-469`).
- **Themes:** 67 color tokens plus `symbols` presets (unicode/nerd/ascii), 251 glyph slots, 101 built-in themes (`refs/oh-my-pi/packages/tui/src/theme/schema.ts:9-166`, `symbols.ts:10-288`).
- **Serialisable UI is still dialogs-only.**
  - Collab-web renders extension tools with a generic JSON view (`refs/oh-my-pi/packages/collab-web/src/tool-render/registry.ts:74-75`).
  - `askDialog` is a declarative question spec (`refs/oh-my-pi/packages/tui/src/overlays/ask-dialog.ts:20-60`).
  - `setFooter` / `setHeader` are no-ops even in the TUI.

**tny: the user's own documented preferences** (first-party)
- **ADR 0027:**
  - Extensions load only from global `~/.tny/extensions`. An opened repository cannot activate code.
  - One lazily started out-of-process host, with a version handshake and normalized versioned events.
  - "Extensions are trusted code … not sandboxed" (`refs/tny/docs/adr/0027-python-event-hooks.md:36-57`).
- **ADR 0028** (`refs/tny/docs/adr/0028-extension-parity-contract.md`):
  - *Capability-scoped hook parity*: never simulate unsupported control (`:28-37`).
  - A frozen action vocabulary: observe, transform, block, rewrite, deny, resolve, annotate, replace, continue, stop (`:45-56, 97-115`).
  - Capability keys with `supported / unsupported / unavailable` plus reason codes (`:117-165`).
  - Additive-only compatibility; unknown events become `UnknownEvent`; unknown actions are invalid (`:179-197`).
  - Fold precedence (`:219-231`): stop > deny > last-valid transform > allow-once > annotations > none. A folded deny is sticky.
  - Observation failures fail open; folded denies fail closed (`:233-242`).
  - Payload limits: 128 KiB event, 64 KiB action text (`:264-273`).
  - Extension actions "never create persistent permission grants".
- **ADR 0038:** custom tools use a deliberately narrow, *executable* JSON-Schema dialect ("rejected instead of being advertised without enforcement"), permission classification before invocation, and generation-tagged async completion (`refs/tny/docs/adr/0038-libtny-custom-tools.md`).

### 2. Portable declarative UI

#### 2.1 Candidate standards (facts)

**A2UI** (https://github.com/a2ui-project/a2ui, Apache-2.0)
- **Versions:** v0.9.1 is "Current Production"; v1.0 is "Candidate" (2026-06-08), with final v1.0 targeted for Q4 2026.
- **Messages:**
  - Agent → renderer: `createSurface`, `updateComponents`, `updateDataModel`, `deleteSurface`; v1.0 adds `callRendererFunction` and `agentFunctionResponse`.
  - Renderer → agent: `action{name, surfaceId, sourceComponentId, timestamp, context}`, `error`, and in v1.0 `callAgentFunction`.
- **Data model:** a flat adjacency list keyed by `id` with one `root`. Rendering begins once the root exists, and missing children render as placeholders.
- **Binding:** RFC 6901 JSON Pointer bindings (`{path}`), including list templates.
- **Catalogs:** `catalogId`; the client advertises `supportedCatalogIds`, and inline catalogs are allowed.
- **Basic catalog, 18 components:** Text, Image, Icon, Video, AudioPlayer, Row, Column, List, Card, Tabs, Modal, Divider, Button, TextField, CheckBox, ChoicePicker, Slider, DateTimeInput.
- **Styling:** v1.0 "removes rigid theme properties … to defer visual styling entirely to the target framework's native theme".
- **Transport:** transport-agnostic (JSONL/WebSocket/SSE; AG-UI CUSTOM events; A2A; over MCP as `application/a2ui+json`).
- **Renderers:**
  - Official: React, Lit, Angular, Flutter; SwiftUI and Compose in progress.
  - **Terminal, community only:** `a2ui-ink` (Ink; v0.9/0.9.1; all 18 components; media as placeholders), and **`a2ui-tui` 0.3.1 (ratatui, MIT; `cargo search a2ui` confirms the crate family: `a2ui-base`, `-tui`, `-egui`, `-iced`, `-slint`, `-bevy`)**. The a2ui-tui repo claims v1.0 support, but its message names differ, so conformance is `UNVERIFIED`.

**MCP Apps (SEP-1865)**
- "Final", Extensions Track. It launched on 2026-01-26 as the first official MCP extension, supported by Claude, Goose, VS Code and ChatGPT.
- `ui://` resources; a tool links to its UI through `_meta.ui.resourceUri`.
- `mimeType: "text/html;profile=mcp-app"` is required (verified: `ext-apps/specification/2026-01-26/apps.mdx:240`). The iframe is sandboxed and talks JSON-RPC over `postMessage`.
- Theming uses host CSS variables in `HostContext.styles.variables`.
- The only other planned type is `externalUrl`.
- "Servers SHOULD provide text-only fallback".
- MCP-UI v6 removed `remoteDom` and became an MCP Apps SDK.
- **Not terminal-renderable.**

**OpenAI Apps SDK**
- Widgets were iframe HTML (`text/html+skybridge`, `window.openai`).
- ChatGPT now "implements the open MCP Apps standard". `openai/outputTemplate` is a compatibility alias.
- **Not terminal-renderable.**

**json-render (Vercel Labs)**
- v0.21.0, pre-1.0, Apache-2.0.
- A zod catalog; a flat `{root, elements{id:{type, props, children}}}` spec; JSONL RFC 6902 patch streaming.
- Events are named client actions; there is no standard message back to the agent.
- The first-party `@json-render/ink` terminal renderer has its **own** catalog: Box, Text, Heading, Badge, Spinner, ProgressBar, Sparkline, BarChart, Table, List, KeyValue, StatusLine, Markdown, TextInput, Select, Tabs, …
- No Rust port.

**Others**
- **AG-UI** core 1.0.0 (2026-09-17) is a *transport* ("not a generative UI specification"). It has `STATE_DELTA` / `ACTIVITY_DELTA` as RFC 6902 patches and carries A2UI.
- **Adaptive Cards** 1.6: a nested tree, whole-card replacement, `HostConfig` theming, "MUST ignore unknown element types". No maintained terminal renderer.
- **Textual:** its web mode (textual-serve) streams a terminal to xterm.js rather than rendering HTML widgets.
- **Ratzilla** runs ratatui in the browser, with a terminal look.
- **Theming, DTCG 2025.10:** the first stable spec (not a W3C standard).
  - `$value`/`$type`; aliases `{a.b}`; `$extensions` for vendor data; a Resolver with `theme: light|dark` modifiers.
  - Dimensions are px/rem only, and it has no ANSI concept.
- **Codex themes:** `/theme` covers syntax highlighting only (syntect/two-face `.tmTheme` in `$CODEX_HOME/themes`, live preview, persisted to `[tui] theme`) (`refs/codex/codex-rs/tui/src/theme_picker.rs:1-15`, `config/src/types.rs:889-894`). Issue #21130 says it does not theme the broader TUI.

| Standard | Data, not code | Terminal-renderable | Streaming | Events back | Maturity |
|---|---|---|---|---|---|
| A2UI | yes | yes (community: ratatui, Ink) | upsert by id + pointer data updates | `action` with source id + context | v0.9.1 prod, v1.0 RC |
| MCP Apps / Apps SDK | no (iframe HTML) | no (text fallback) | partial tool input | postMessage JSON-RPC | Final extension |
| json-render | yes | yes (own Ink catalog) | JSON Patch lines | client-side handlers only | 0.21, pre-1.0 |
| Adaptive Cards | yes | no renderer | whole-card replace | Submit/Execute | mature, Microsoft-centric |
| pi components | no (closures) | TUI only | n/a | n/a | — |

### 3. Code mode runtime

#### 3.1 Prior art (facts)

**Codex** (in-repo, stage `UnderDevelopment`, default off; `refs/codex/codex-rs/features/src/lib.rs:118-131, 1051-1083`)
- **Crates and process:** `code-mode`, `-protocol`, `-runtime` and `-host`. The runtime is the `v8` crate with `v8_enable_sandbox`, one fresh isolate per `exec` (`code-mode-runtime/Cargo.toml`, `src/runtime/mod.rs:172-260`). It runs in the standalone `codex-code-mode-host` process over gRPC/stdio (`code_mode_host`: Stable, default on; `disable_in_process_fallback` gives fail-closed behaviour; `feature_configs.rs:57-63`).
- **The `exec` contract** (`code-mode-protocol/src/description.rs:20-43`):
  - Raw JS, not JSON. The optional pragma is `// @exec: {"yield_time_ms", "max_output_tokens"}`.
  - Helpers: `tools.*`, `text()`, `image()`, `store()/load()` (state across cells), `notify()`, `setTimeout`, `ALL_TOOLS`, `yield_control()`, `exit()`.
  - Deferred nested tools are found by filtering `ALL_TOOLS` (`:16-17`).
  - `wait{cell_id, yield_time_ms, max_tokens, terminate}` resumes a running cell (`:45-52`).
- **Schema rendering:** JSON Schema → TypeScript with a per-tool byte budget (`json_schema_types.rs:18-40`). The default is a 16,000-byte minimum (`feature_configs.rs:36-39`). MCP `CallToolResult` types ship as a TS preamble (`description.rs:53-120`).
- **Model catalog:** `ModelInfo.tool_mode ∈ {direct, code_mode, code_mode_only}` and `tool_messages.code_mode{exec, wait, deferred_nested_tools_guidance, mcp_typescript_preamble}` are per-model templates (`protocol/src/openai_models.rs:346-349, 589, 703-721`).
- `js_repl` was removed (discussion #21655).

**oh-my-pi**
- The `eval` tool runs py (subprocess, NDJSON) or js (Bun worker). State persists per language, and `await tool.<name>(args)` goes through an HTTP bridge with a bearer token.
- Bridged calls pass the same `ExtensionToolWrapper`: `tool_call` hooks and approval tiers. **Neither kernel is a sandbox**; the JS kernel inherits the environment.
- "Code Mode" is Codex-only: `code_mode_only` shrinks direct tools to eval/ask/todo/…, and the prompt carries generated `declare const tool:{…}`.
- Sources: `refs/oh-my-pi/packages/coding-agent/src/session/code-mode.ts:1-67`; `docs/tools/eval.md:19-216`; `src/eval/js/tool-bridge.ts:211-307`.

**Cloudflare Code Mode**
- 2025-09-26 post: MCP schema → TypeScript API with doc comments, run in a V8 isolate (Worker Loader) with tools as *bindings*, `globalOutbound: null`. "LLMs have seen a lot of code. They have not seen a lot of 'tool calls'."
- 2026-02-20: the Code Mode MCP server puts 2,500 endpoints behind `search()` + `execute()` in "roughly 1,000 tokens" versus 1.17M ("99.9%").
- `@cloudflare/codemode` 0.5.2: an `Executor.execute(code, fns)` interface, a durable runtime with approve/reject/rollback, and a 60 s timeout. Tools that need approval are excluded from `createCodeTool()`.
- Sources: https://blog.cloudflare.com/code-mode/, https://blog.cloudflare.com/code-mode-mcp/

**Anthropic**
- "Code execution with MCP" (2025-11-04):
  - Tools become files (`servers/<server>/<tool>.ts` + `index.ts`) or come from a `search_tools` tool with a detail level.
  - "150,000 tokens to 2,000 tokens … 98.7%" (verified by curl).
  - Intermediate results stay in the sandbox, and PII is tokenised.
  - Saved functions in `./skills/*.ts` plus a `SKILL.md` become skills.
- Programmatic tool calling is now `status: ga` (`code_execution_20260120`+, `allowed_callers`):
  - Tools are exposed "as async Python functions".
  - Execution pauses at each call with `caller:{type, tool_id}`; you reply with only `tool_result` blocks. Pending calls time out after about 4 minutes.
  - "Do not rely on `allowed_callers` as a security boundary."
  - Claim: +11% accuracy with 24% fewer input tokens on BrowseComp/DeepSearchQA.
- Tool search (regex or BM25 variants, `defer_loading`): "over 85 percent" fewer tokens; accuracy "degrades once you exceed 30–50 available tools".
- Sources: https://www.anthropic.com/engineering/code-execution-with-mcp, https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling

**smolagents / CodeAct**
- smolagents' `LocalPythonExecutor` is an AST interpreter with an import allowlist and operation caps. It "is **not a security sandbox**". Remote executors: E2B, Docker, Modal, Blaxel, Pyodide+Deno.
- CodeAct (ICML 2024): "up to 20% higher success" and "up to 30% fewer actions".
- Adoption split: JS/TS for Cloudflare, Codex, Goose (pctx on deno_core) and executor.sh; Python for Anthropic PTC, Pydantic (Monty), smolagents and CodeAct.
- I found no head-to-head JS-vs-Python code-action benchmark.

#### 3.2 Embeddable runtimes (facts; versions from `cargo info` today)

| Runtime | Version | Model familiarity | Sandbox default | Async host calls | Limits | Size (measured Δ unless noted) | Maturity / notes |
|---|---|---|---|---|---|---|---|
| rquickjs → QuickJS-ng | 0.14.0 → QJS 0.16.2 (`rquickjs-sys-0.14.0/quickjs/quickjs.h:1450-1452`) | JS ES2023+, the same language as Codex/Cloudflare code mode | no I/O ("doesn't aim to provide system … APIs") | yes: `Async(..)` ↔ Promise (verified) | `set_memory_limit`, `set_max_stack_size`, `set_interrupt_handler` | **+1.04 MB** | Mature C engine in-process; memory-safety bugs are possible |
| boa_engine | 0.22.0 | JS, 95.6% test262 | none | `NativeFunction::from_async_fn`, but the default executor blocks | `RuntimeLimits` (loops, recursion, stack); no memory/interrupt API found | not measured | README says "experimental" |
| deno_core + v8 | 0.412.0 / v8 152.2.0 | JS/TS (V8, Codex's choice) | only the ops you register | `#[op2] async` | `heap_limits`, `terminate_execution` | static lib 37–39 MiB gzipped per target | Heavy, prebuilt downloads; snapshots |
| javy | 8.1.0 (CLI 9.1.0) | JS | WASI-granted only | none documented | via the wasmtime host | ≥869 KB wasm | **Core module, not a component** |
| componentize-qjs | n/a | JS | WIT-granted only | p3 streams example | wasmtime | `UNVERIFIED` | Very young |
| **Monty** (pydantic) | **1.0.0-beta.3** (`cargo search monty`) | Python subset matching CPython 3.14 semantics (no inheritance, generators, `match` or third-party packages; 19 partial stdlib modules) | "no ambient authority"; clock and entropy set by an `OsPolicy` | **suspends** at every external call; `resume` / `resume_pending` futures | memory (needs `monty-alloc`), feed/turn time, recursion, suspension count | **+8.09 MB** | Beta. An in-process abort kills the process, so use `monty-pool` workers. Bundles `ty` type checking against stubs. |
| RustPython | 0.5.0 | Python | **host I/O on by default** (`host_env`) | none found | none found | not measured | "not totally production-ready" |
| starlark | 0.14.2 | Python-like, deterministic | none | no (sync only) | heap, ticks, cancel | not measured | Build-config DSL, limited |
| rhai | 1.26.1 | own language | none | **no** ("not possible") | ops, depth, sizes | not measured | Not model-familiar |
| mlua | 0.12.1 | Lua/Luau | Lua 5.x loads `io`/`os`; Luau is minimal | `create_async_function` | memory, interrupt (Luau) | not measured | Less model-familiar |
| TS stripping | oxc_transformer 0.151.0; swc_ts_fast_strip 58.0.0 | — | — | — | — | — | Both strip without type-checking. swc powers Node's type stripping (Amaro). oxc can emit `.d.ts` under isolatedDeclarations. |

#### 3.3 Measurements (ext-lab `qjs_bench.rs`, `monty_bench.rs`, `interrupt_probe.rs`)

**rquickjs**

| Measurement | Result |
|---|---|
| Runtime + `Context::full` / `Context::base` | 109 µs / 55 µs |
| New runtime + 2 host fns + eval of a 12-line code-mode script making 1,000 host calls | 549 µs |
| Sync JS→Rust host call | 529 ns (baseline JS loop iteration: 139 ns) |
| `await callTool(i)` on a genuinely pending Rust future | **10.7 µs per await** |
| Interrupt-handler deadline at 10/50/100/400 ms | fired at 10.1/50.1/100.0/400.0 ms (handler polled about every 23 µs) |
| 16 MiB memory limit vs unbounded allocation | stopped |

One earlier 50 ms run fired at 199 ms. I could not reproduce it, and all 8 later runs were exact.

**Monty**

| Measurement | Result |
|---|---|
| Trivial `1 + 41` new + run | 3.9 µs |
| Parse + compile a 9-line script | 36 µs |
| Same script with 1,001 pause/resume host calls | 1.59 ms (about 1.5 µs per call) |
| Snapshot of a run paused at `read_file()` (postcard) | **5,271 bytes** |
| 50 ms time limit | fired at 50.1 ms |
| 1M-iteration loop | 55 ns per iteration |

**Binary size deltas vs hello world** (aarch64-apple-darwin, opt 3, thin LTO, stripped)

| Runtime | Δ |
|---|---|
| QuickJS | +1.04 MB |
| Monty | +8.09 MB |
| wasmtime + cranelift | +11.35 MB |

#### 3.4 Typed API surfaces and prompt size (facts)
- **Codex:** JSON Schema → TS per tool, with a byte budget; `ALL_TOOLS` metadata; a deferred-tools guidance line.
- **Anthropic:** filesystem-of-wrappers, or `search_tools` with detail levels.
- **Cloudflare:** `search()` over a pre-resolved spec, then `execute()`.
- **Monty:** `type_check_stubs` plus `ty` rejects calls to unimplemented or mistyped APIs *before* running.
- For TS, no Rust-native type checker exists. oxc and swc only strip types.

## Implications for aim (opinion; tradeoffs stated)

### A. The architecture in one picture

```
agent app ──(unix/http/grpc/ws)──► harness daemon (execution layer, SQLite state)
                                   ├─ event core: normalized events + action fold (Verus-verified)
                                   ├─ tool dispatcher  ◄── direct model calls, code mode, plugins, programs
                                   │    └─ permission engine → backend (local | SSH shadow | remote)
                                   ├─ plugin host: wasmtime (LTS), one Engine, pooling + CoW .cwasm cache
                                   │    └─ components linked only to their *granted* aim:plugin interfaces
                                   ├─ code-mode worker process(es): CodeRuntime{rquickjs JS/TS, Monty Py}
                                   │    └─ tools.* ⇒ RPC back to the dispatcher (no ambient fs/net)
                                   ├─ program library: git repos + SQLite FTS5/vectors + Jev rerank
                                   └─ UI hub: surfaces (A2UI-shaped) ⇒ fan-out to TUI (ratatui) / web / export
```

**The key invariant is one tool dispatcher.** Code-mode calls, plugin `tools.call`, program runs and direct model calls all enter the same dispatcher. They carry `source = model | code(cell) | plugin(id) | program(id@sha)`, which buys:

- Permissions, tny-0028 hooks (`pre_tool_use` sees code-mode calls), SSH shadowing, audit and replay are implemented once.
- oh-my-pi reached the same design, where the eval bridge reuses the hook wrapper, but without a sandbox. Codex keeps code mode out-of-process.

### B. Plugin host: decisions

**1. Runtime: wasmtime, components only, one `Engine` per daemon.**
- **Pin the LTS line (48.x, then 60.x).** Automate patch uptake given the 2026 advisory rate; skipping patches is the real risk.
- **Precompile on install** to `~/.aim/cache/wasm/<sha256(component)>-<wasmtime-ver>-<config-hash>.cwasm` and load with `deserialize_file`. Only aim-produced files, never downloaded `.cwasm`, because `deserialize` is unsafe on untrusted input.
- **Use the pooling allocator and `InstancePre`.** Measured: 3–5 µs per instance even with 4 MiB images.

**2. Instance scope and limits.**
- **Scope:** one instance per (plugin, session) by default, matching pi's per-session runtime and isolating sessions. Opt into `scope = "daemon"` for singletons.
- **Durable state** goes in host `kv`, not linear memory, so reloads and crashes lose nothing.
- **Limits:**
  - `StoreLimits` memory at 64 MiB by default.
  - An epoch deadline per callback class: about 20–50 ms for synchronous control hooks (`pre_tool_use`, `user_prompt_submit`), seconds for tools/commands, and cancellation through `epoch_deadline_trap` on abort.
  - Fuel only in deterministic/replay mode (the measured cost is similar to epochs, but fuel gives exact counts).
- **Tradeoff:** a per-callback deadline makes plugins with long synchronous work fail. Push them to `ui.apply` / async tool results rather than blocking hooks.

**3. Plugin API world: sync-only at 0.1, host async internally.**
- Guests see blocking imports; the host implements them with `func_wrap_async` (fibers), so the daemon never blocks.
- Why: `wasm32-wasip3` Rust is not stable until about 1.100, ComponentizeJS imports are sync-only, and TinyGo is unmaintained. Requiring p3 now would shrink the language set, which the brief treats as the point of wasm.
- Plan `aim:plugin@0.2` around `async func` + `stream<ui-patch>` / `stream<provider-event>` once p3 guests are stable. Model-provider plugins (pi `registerProvider`) belong there, since streaming needs `stream<T>`.

**4. Versioning:**
- Start at `0.1.0`, never `0.0.x`, and never pre-release tags; wasmtime will not semver-match them.
- Within the 0.1 track, only **add interfaces and functions** (patch bumps 0.1.1, 0.1.2, …). Old guests keep linking.
- Records and variants are frozen per track. To survive evolution without breaking types:
  - Event payloads are JSON with a schema major, and unknown fields are ignored (tny 0028).
  - The typed `action` variant has an `ext(record{kind, payload})` case. Actions added mid-track use it and become first-class cases at 0.2.
- A breaking change means `0.2.0`, and the host keeps a `0.1` adapter module (the Zed pattern). Adapters are cheap because the host core speaks the normalized JSON vocabulary.
- The manifest declares `api = "0.1"` so incompatibility is rejected before compiling.

**5. Capabilities** (grant model; enforcement kernel is a Verus target)
- **Manifest** (`aim-plugin.toml`): name, version, api, and requested capabilities. Keys reuse tny's vocabulary (`extensions.tool.pre.rewrite`, `extensions.permission.allow_once`, …) plus aim keys:
  - `tools.provide`, `tools.call:<name|glob>`
  - `ui.surface:<placement>`, `ui.dialog`, `ui.theme`, `keys.bind`
  - `session.message.send`, `session.model.set`, `session.effort.set`, `session.tools.set`
  - `kv`, `bus:<topic>`, `net.http:<host-glob>`, `secrets:<name>`, `clock.wall`
- **Storage:** grants live in SQLite as `(plugin, sha256, capability, scope, granted_by, ts)`. A new hash re-prompts only for newly requested capabilities.
- **Enforcement is two-layer:**
  - Link-time: interfaces that were not granted are linked to deny-stubs that return `error::denied`, never faked success. This follows tny "never approximate".
  - Call-time: scope checks, such as HTTP egress allowlists enforced in the host's `wasi:http` implementation, and the tool permission engine.
- **Default trust:** only global `~/.aim/plugins` load. Project `.agents/plugins` needs explicit hash-pinned trust (tny 0027).
- **Agents cannot self-grant.** An agent-authored plugin gets the minimum of what it requests and the session's own ceiling. Extension actions never create persistent permission grants (tny 0028).

**6. Meeting pi's bar**

| pi | aim 0.1 |
|---|---|
| 40 `pi.on` events + result types | `subscriptions` + `on-event` → `list<action>`, folded by tny-0028 precedence. The event vocabulary is tny's frozen set plus pi's extra lifecycles (tree/fork/compaction/provider/cache-warming/ui-prompt), added only where aim owns the boundary. |
| `registerTool` + `renderCall`/`renderResult` | `tool-spec` (narrow executable schema, tny 0038) + `call-tool`; rendering via `render` returning UI messages (data) |
| `registerCommand` + completions, `registerShortcut`, `registerFlag` | `command-spec` + `run-command` + `complete`; declarative `keybinding` records; manifest `[flags]` |
| message/entry renderers, markdown transformer | `render(kind, data, width)` → UI; a `message.render` transform event |
| `sendMessage`/`sendUserMessage`/`appendEntry`, set model/thinking/tools | `session.*` imports + the `entry` action (all capability-gated) |
| `ctx.ui` dialogs/status/widgets/header/footer/overlay | UI surfaces with placements (§C); `ui.ask` dialogs |
| `custom()` raw-input components (doom, snake) | a `cells` grid component plus key-event actions (data, works on web) |
| `setEditorComponent` | out of scope for remote UIs; offer an `editor.key` / `editor.text` transform events instead (tradeoff) |
| `registerProvider` | `aim:provider` world at 0.2 (streams) |
| `pi.events` bus | `bus` interface (topic capability) |
| `pi.exec` (shell) | only `tools.call("bash")`, which is permissioned and SSH-shadowed. No direct spawn. |

**7. WIT sketch (`aim:plugin@0.1.0`)**

```wit
package aim:plugin@0.1.0;

interface types {
  type json = string;                          // UTF-8 JSON; host enforces size limits (tny 0028: 128 KiB event, 64 KiB text)
  enum level { trace, debug, info, warn, error }
  variant error { denied(string), invalid(string), unavailable(string), timeout, failed(string) }
  record event { name: string, schema: u16, seq: u64, session: option<string>, payload: json }
  record tool-result { content: json, is-error: bool, details: option<json> }
  enum permission-decision { allow-once, deny, abstain }
  record ext-action { kind: string, payload: json }
  variant action {
    none, context(string), stop(string), continue-turn,
    prompt-transform(string), prompt-block(string),
    tool-rewrite(json), tool-deny(string), permission(permission-decision),
    tool-annotate(string), tool-result-replace(tool-result),
    entry(json),                               // durable custom entry (pi appendEntry)
    ui(list<json>),                            // UI protocol messages for surfaces this plugin owns
    ext(ext-action),                           // actions added mid-track; promoted at 0.2
  }
  record tool-spec { name: string, description: string, input-schema: json, sensitive: bool }
  record command-spec { name: string, description: string, args-hint: option<string> }
  record keybinding { chord: string, command: string, when: option<string> }
  record registration {
    subscriptions: list<string>, tools: list<tool-spec>, commands: list<command-spec>,
    keybindings: list<keybinding>, themes: list<json>, renders: list<string>,
  }
}
interface host  { use types.{level}; log: func(l: level, msg: string); capability: func(key: string) -> string; now-ms: func() -> u64; }
interface kv    { use types.{error}; get: func(k: string) -> result<option<list<u8>>, error>;
                  set: func(k: string, v: list<u8>) -> result<_, error>; delete: func(k: string) -> result<_, error>;
                  list: func(prefix: string) -> result<list<string>, error>; }
interface tools { use types.{json, tool-result, error}; call: func(name: string, args: json) -> result<tool-result, error>; }
interface session { use types.{json, error};
  send-message: func(kind: string, content: string, trigger-turn: bool) -> result<_, error>;
  set-model: func(id: string) -> result<_, error>; set-effort: func(e: string) -> result<_, error>;
  set-active-tools: func(names: list<string>) -> result<_, error>; query: func(q: json) -> result<json, error>; }
interface ui    { use types.{json, error};
  apply: func(msgs: list<json>) -> result<_, error>;      // out-of-band surface updates (timers, progress)
  notify: func(level: string, text: string);
  ask: func(dialog: json) -> result<option<json>, error>; }  // blocks this plugin only
interface bus   { use types.{json, error}; publish: func(topic: string, msg: json) -> result<_, error>; }
interface plugin {
  use types.{event, action, registration, json, tool-result, error};
  init: func(config: json) -> result<registration, error>;
  on-event: func(ev: event) -> list<action>;
  call-tool: func(name: string, call-id: string, args: json) -> result<tool-result, error>;
  run-command: func(name: string, args: string) -> result<list<action>, error>;
  complete: func(kind: string, prefix: string) -> list<json>;
  on-ui-action: func(action: json) -> list<action>;
  render: func(kind: string, data: json, width: u32) -> option<list<json>>;
  shutdown: func(reason: string);
}
world plugin {
  import host; import kv; import tools; import session; import ui; import bus;
  import wasi:http/outgoing-handler@0.2.0;     // semver-matches the host's 0.2.x; egress allowlist in host
  export plugin;
}
```

**8. Agent-authored plugins at runtime**
- **Two paths, one ABI:**
  - (a) **Compiled components.** Rust is the recommended SDK: an `aim-plugin` crate over wit-bindgen, targeting `wasm32-wasip2`, about 16 KB output; the target is pinned in `rust-toolchain.toml` and tools through mise. Other languages build through jco, componentize-py, componentize-go or wasi-sdk.
  - (b) **Script plugins.** aim ships one prebuilt QuickJS component (`aim:script-runtime`) that exports `aim:plugin` and runs plugin JS/TS source as data (types stripped by oxc on the host). The agent writes `plugin.ts`, and it hot-loads in milliseconds with no toolchain, which is pi-level immediacy inside the wasm sandbox.
- **Harness tools:** `plugin.scaffold | build | check | load | unload`.
  - `check` = `wasm-tools component wit` + a dry-run `instantiate_pre` against the Linker (import types) + a capability diff.
  - `load` into the agent's own session needs no prompt only when grants ⊆ the session ceiling; otherwise the user approves.
- **Tradeoffs:**
  - Script plugins make QuickJS-in-wasm a dependency; `componentize-qjs` is immature, so aim may need to build its own rquickjs-guest with wasi-sdk-34.
  - Compiled JS via StarlingMonkey costs about 8–11 MB per plugin plus seconds of compilation, cached.

**9. Hot reload**
- A watcher triggers compile → precompile → new `InstancePre`.
- Then: send `shutdown(reload)` to the old instances, swap, `init` the new ones, and fire `session_start{reason: reload}`.
- In-flight calls finish on the old store under an epoch deadline.
- `Component::same` skips no-op reloads.

### C. UI protocol: decisions

**1. Adopt A2UI v1.0's shape, not a new invention.**
- Surfaces with ids, a flat component list upserted by `id`, and JSON-Pointer `updateDataModel` for high-rate streaming, so progress and status never resend components.
- `action{name, surfaceId, sourceComponentId, context}` flows back.
- Clients advertise their catalogs. Import and export A2UI messages verbatim, so any A2UI-speaking agent or tool renders in aim, and aim's web UI can reuse official A2UI web renderers for the basic catalog.
- **Tradeoff:** A2UI renamed its messages twice in about a year and v1.0 is an RC. Pin a version inside aim's envelope (`a2ui: "1.0"`) and keep an adapter layer.

**2. aim-specific additions A2UI lacks** (in an `aim` catalog; `catalogId: "aim/terminal@1"`)
- **Placements:** `status.left|right`, `widget.above_editor|below_editor`, `panel.side`, `overlay{anchor, w, h}`, `dialog`, `transcript(entry)`, `tool(call_id)`, `toast`, `title`.
- **Terminal-first components:**
  - Text and content: Text (styled spans), Markdown, Code, Diff.
  - Layout: Row, Column, Box (border, title, padding), Divider, Spacer.
  - Data display: List, Table, Tree, KeyValue.
  - Status: Progress, Spinner, Badge, Sparkline, KeyHint, Log (append-only stream).
  - Media and inputs: Image (kitty/iTerm in the TUI, `<img>` on web, alt fallback), Button, TextField, Select, CheckBox.
  - The escape-hatch `Cells` grid.
- **Layout:** cells map to `ch` on the web. Sizing is `auto | fill(weight) | cells(n) | percent`, which maps 1:1 onto ratatui `Constraint`.
- **Built-in UI goes through the same protocol:** status line, tool rows and dialogs. That gives TUI/web parity *by construction* (the same principle as tny ADR 0017) and makes every built-in replaceable by plugins.

**3. Unknown components render their `fallback` child or text** (Adaptive Cards' "MUST ignore unknown element types" rule).
- MCP Apps HTML surfaces are **web-only**. The TUI shows the text fallback plus an OSC 8 "open in browser" link.

**4. Agent-authored UI.**
- A `ui.show / ui.update / ui.close` tool, and `ui.*` inside code mode, emit the same messages.
- They are schema-validated against the advertised catalog, size-bounded, owned by the session, and persisted in the session log, so replay and HTML export re-render them.
- The catalog's LLM `instructions` (A2UI v1.0) feed the tool description. Keep it compact and lazily expanded.

**5. Themes.**
- **File format:** DTCG 2025.10 token files: `$type: color` plus aliases, and a resolver `theme: light|dark`.
- **Terminal extension:** `$extensions."dev.aim.terminal"` = {ansi256 fallback, ansi16 fallback, attrs (bold/dim/italic), glyph set}.
- **Token vocabulary:** start from pi's 56 roles or oh-my-pi's 67 (semantic: accent, muted, border*, success/warning/error, tool*, md*, syntax*, thinking*, diff*, status-line segments), plus oh-my-pi-style glyph presets (unicode/nerd/ascii).
- **Web:** tokens become CSS custom properties. The same values feed MCP Apps `HostContext.styles.variables`.
- Themes are data, needing no capability beyond `ui.theme` to *apply*. Plugins ship them in `registration.themes`.
- **Tradeoff:** DTCG has no ANSI concept and px/rem-only dimensions. Keep spacing out of tokens, since cells are layout, not theme.

### D. Code mode: decisions

**1. Languages: JS/TS first, Python second, both behind `trait CodeRuntime`.**
- Codex models carry `tool_mode` / `code_mode` exec templates for **JS**. Claude's native PTC exposes tools as async **Python**. aim targets both families, so language selection is per model (catalog hint, then config default).
- Mirror Codex's `exec`/`wait` contract exactly for Codex models: the pragma, `tools.*`, `ALL_TOOLS`, `store`/`load`, `text`/`image`/`notify`/`yield_control`. That way the model-catalog templates can be used verbatim.
- Claude gets `run_code` (Python) with the same semantics.

**2. Engines:**
- **rquickjs (QuickJS-ng)** for JS/TS: +1 MB, 0.1 ms cold, async host calls that give real `Promise.all` concurrency, an exact interrupt deadline, and a memory limit.
- **Monty** for Python: pure Rust; snapshot at every tool call, so a code cell survives a daemon restart and can be replayed or forked; exact time limit; `ty` pre-checks against generated stubs; an `OsPolicy` for deterministic clock and random.
- **Rejected:**
  - V8/deno_core: its static lib is 37–39 MiB gzipped per target. The linked binary delta is `UNVERIFIED` but likely tens of MB. It also needs an external prebuilt download, which conflicts with "lightweight". It is what Codex runs, but orchestration code does not need V8.
  - boa: experimental, with blocking async.
  - RustPython: ambient I/O on by default.
  - rhai and starlark: no async, and low model familiarity.
- **Tradeoffs:**
  - QuickJS is C, so a memory-safety bug is a sandbox escape.
  - Monty is beta, a Python *subset*, and aborts its host process on stack overflow or allocation failure. It needs a supervisor.

**3. Placement: a separate `aim-coderun` worker process, pooled and crash-isolated.**
- It sits under the OS sandbox profile (seatbelt/bwrap per tny ADR 0060) with no network. Tool calls return over the harness RPC.
- **Precedent:** Codex (`code_mode_host`, fail-closed option) and Monty's own advice (`monty-pool`).
- The RPC hop is microseconds against millisecond-scale tools. It also keeps the harness daemon alive when an interpreter aborts, and the execution layer stays separable, as the brief requires.

**4. Should code mode live inside wasm?** Not for the MVP. Keep it as a `CodeRuntime` backend option.
- **Pros:**
  - Memory-safety containment for QuickJS.
  - One limits mechanism (epoch/fuel/StoreLimits) shared with plugins.
  - Fuel-exact determinism.
  - Instantiation in µs with copy-on-write (measured).
- **Cons:**
  - No mature QuickJS component today (javy is core-only; componentize-qjs is young).
  - The async bridge needs either p3 async lowering or a split-phase `start-call`/`wait-any` import pair.
  - Slower interpretation (`UNVERIFIED`), plus build complexity.
- **Trigger to switch:** running untrusted synced programs, or multi-tenant/remote harnesses.
- **The convergence point is the script-plugin runtime (B.8).** Once aim owns a QuickJS component for plugins, the same component can serve as a wasm `CodeRuntime` backend (a different world: only `aim:code/tools` imported). "Share one runtime" then becomes true without paying for it up front.

**5. Typed surface generation.**
- One JSON Schema → TS renderer (port Codex's budgeted renderer; 16 KB per tool) and one JSON Schema → Python `TypedDict`/stub renderer, both from the dispatcher's tool registry (including MCP and plugin tools).
- **Prompt:** a compact index (name, one line, about 8–15 tokens per tool) plus `ALL_TOOLS` plus `describe(name)` / `search(q)` inside the runtime. Full declarations stay out of the prompt, since tool-search evidence says accuracy degrades beyond 30–50 tools.
- Strip TS with oxc (0.151) before QuickJS. Type-check Python with Monty's `ty` before running (cheap wrong-argument detection). TS has no type-check path.

**6. Results discipline.**
- Only `text()`/`return` values reach the model; intermediate data stays in the worker (Anthropic's privacy pattern).
- Cap output tokens per cell.
- Every nested call is logged with `source=code(cell)` for audit and replay.

### E. Saved programs: decisions

**1. Storage.**
- `~/.aim/programs` (user) and `.agents/programs` (project, next to `.agents/skills`); each is a git repo.
- Each program: `<slug>/main.{ts,py}`, `program.toml`, `README.md` with SKILL.md-compatible frontmatter (so a program doubles as a skill, per Anthropic's pattern), and `tests/*.json` (args + assertions over recorded tool results).
- **`program.toml` fields:**
  - Identity and interface: id (ULID), name, description, language, runtime version, `params`/`returns` JSON Schemas.
  - Behaviour: `tools` (statically extracted `tools.*` references via the oxc or ruff AST), `grants` (a capability snapshot).
  - Provenance and versioning: session id and turn, semver, tags.
- Embeddings and usage stats live in SQLite, not git.

**2. Parameterisation.** `export default async function main(args)` or `def main(args)`. Arguments are validated against `params`. Every saved program is also registered as a deferred tool `programs.<slug>` and a `/program <slug>` command.

**3. Versioning and sync.**
- One commit per save; tags per release; `aim programs log|diff|revert`.
- Push to a user-configured **private** GitHub remote. On pull conflicts, keep both versions rather than auto-merging code.
- Pulled programs are **untrusted until their hash is trusted**, the same rule as project plugins.

**4. Retrieval and suggestion.**
- Candidates: SQLite FTS5 (BM25) over name, description, README and tools, unioned with vector top-k.
- Rerank: **one Jev `Client::ask`** whose state is the task summary, with per-candidate questions `Score(RELEVANCE_LEVELS)` + `Noul("Can this program be run as-is for the task?")`. Jev evaluates every question in a request in parallel (`refs/jevgrep/src/search.rs:1-13,26-31`; `refs/jevgrep/crates/typesafe-jev/src/client.rs:182`).
- Surfacing: calibrated probabilities gate TUI suggestions (like skill autocomplete) and `programs.search()` in code mode.

**5. Safety.**
- Effective grants = saved grants ∩ current session policy ∩ trust state. They never escalate.
- Every call goes through the dispatcher and plugin hooks with `source=program:<id>@<sha>`.
- Deterministic replay mode: recorded tool results, plus a virtual clock and random (Monty `OsPolicy`; QuickJS overrides of `Date` and `Math.random`).

### F. What to build first (MVP maximising future flexibility)

1. **Contracts before engines.**
   - A normalized event schema and action fold, reusing tny 0028's vocabulary, precedence, limits and capability states. The fold and grant check are the first **Verus** proofs, since they are the security kernel.
   - The one-dispatcher tool bridge with `source` provenance.
2. **UI protocol v0 plus both renderers,** used by aim's own status line, tool rows and dialogs from day one (dogfooding, so parity is guaranteed). Include A2UI import. Themes as DTCG tokens.
3. **Code mode (JS):**
   - `aim-coderun` worker, rquickjs, and the `exec`/`wait` contract compatible with Codex.
   - `.d.ts` generation and `describe`/`search`.
   - This is the quickest token-efficiency win.
4. **Plugin host:**
   - wasmtime LTS, `aim:plugin@0.1.0` (sync world), grants, deny-stubs, `.cwasm` cache, hot reload.
   - A Rust guest SDK, then the QuickJS script-plugin runtime.
5. **Monty Python runtime, program library, GitHub sync, Jev rerank.**
6. **Later:**
   - `aim:plugin@0.2` on WASI p3 (async, streams): model providers, streaming renderers.
   - A wasm `CodeRuntime` backend.
   - MCP Apps web surfaces.

**Biggest risks**
- wasmtime's advisory rate (mitigation: LTS plus fast patching, and grants that limit blast radius).
- A2UI churn (mitigation: versioned envelope plus adapter).
- Monty beta (mitigation: JS is the primary language; Python is secondary behind a supervisor).
- Script-plugin runtime build effort (mitigation: Rust SDK first).

## Open questions for the user

1. **Plugin trust default.** tny loads only global, trusted, unsandboxed extensions. Should aim keep "global-only unless hash-trusted", even though wasm makes project plugins far safer?
2. **Program sync target.** One private GitHub repo per user (`<user>/aim-programs`), or per-project `.agents/programs` committed into each project's own repo, or both?
3. **Code-mode default per provider.** Should aim honour Codex's `tool_mode: code_mode_only` (where direct tools become only `exec`/`wait`) whenever the catalog says so, or keep code mode opt-in until benchmarks show the win?
4. **Python for code mode.** Is Monty's Python *subset* (no class inheritance or generators, 19 partial stdlib modules; beta) acceptable as the Claude-side language, or should Claude also default to JS?

<!-- REPORT COMPLETE -->
