# WASIp2 example plugins

The examples target the `aim:plugin@0.1.0` WIT world through `aim-plugin-sdk`. The host advertises
the tool specifications from each adjacent `aim-plugin.toml`, then compiles and initializes the
component on its first call. The manifest's capability list is a **request**, not a grant.

- `kv_counter` registers `increment` and stores the named counter through the host `kv` interface.
  It requests `tools.provide` and `kv`.
- `delegate_read` registers `delegate_read`, forwarding a `file_path` argument to the session's
  permissioned `read` tool. It requests `tools.provide` and `tools.call:read`. The host still applies
  the session tool allowlist and ceiling to the delegated call.
- `runaway` is a **test fixture** whose `call-tool` spins forever. Host tests use it to prove that
  fuel or epoch limits terminate a guest call. Do not install it as a normal plugin.

`build/*.wasm` are checked-in WebAssembly **components**, not precompiled wasmtime images. This
keeps routine native checks from rebuilding the guest toolchain. To reproduce them with the pinned
Rust and `wasm32-wasip2` target, run `mise run plugins:examples` from the repository root. That
task invokes `build.sh --verify`; it rebuilds all components and checks their bytes and SHA-256
against `build/SHA256SUMS`. After a guest source change, run `mise exec -- sh
plugins/examples/build.sh --update` and commit the changed component and hash file together.

The components have no WASI filesystem or process imports. Rust `std` requires WASIp2 CLI,
monotonic-clock, and stream imports; the host supplies those with no preopened directories or
process access. All data and side effects needed by these examples pass through `aim:plugin`
imports and remain subject to capability and session checks.
