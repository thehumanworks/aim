# R4 — Verus formal verification: mise install, working proofs, workspace structure

Scope: install Verus through mise (pinned), prove it works on a realistic aim invariant, measure
the dev loop, and recommend how aim should structure verified code. Everything below marked
"ran" was executed on 2026-09-25 on an Apple M3 Ultra (28 cores, macOS 27.0, rustup + mise
2026.9.12). The machine was shared with other agents running builds, so timings carry noise.

`LAB` = `/private/tmp/claude-501/-Users-tomas-projects-aim/138888ab-ed67-4eee-88a5-9b62a4dea285/scratchpad/verus-lab`
(left in place for inspection). `LAB/verus-src` is a shallow clone of `verus-lang/verus` at the
pinned tag. Citation shorthands, all inside that clone:

| Shorthand | Full path |
|---|---|
| `guide/src/X` | `verus-src/source/docs/guide/src/X` |
| `cargo-verus/src/X` | `verus-src/source/cargo-verus/src/X` |
| `source/X` | `verus-src/source/X` |
| `examples/X` | `verus-src/examples/X` |
| `vstd/X` | `verus-src/source/vstd/X` (same content as the crates.io vstd) |

## TL;DR

- **The install works through mise's `github:` backend.** `version_prefix = "release/"` removes the
  prefix from the tag, so the pin is `0.2026.09.20.aef82ed`. `mise lock` writes sha256 checksums
  that match GitHub's asset digests. The download has no macOS quarantine flag. The zip is 449 MB
  and unpacks to 1.4 GB, so use `filter_bins` to put only `verus` and `cargo-verus` on PATH.
- **Each Verus release pins exactly one rustc.** 0.2026.09.20 needs 1.98.1, which is also the
  user's stable toolchain today, but only by coincidence. The `verus` driver always runs
  `rustup run 1.98.1-aarch64-apple-darwin rust_verify`. mise's `rust` backend is rustup under the
  hood, so the two are compatible. The default rustup profile is enough at runtime; `rustc-dev`
  and `llvm-tools` are not needed.
- **Under `cargo verus`, every crate in the build is compiled by Verus's own rustc_driver** (1.98.1),
  verified or not, whatever cargo started the build. A run started by cargo 1.97.1 also succeeded.
  The dependency closure of the verified crates must therefore compile on Verus's rustc. The rest
  of the workspace does not.
- **How vstd is consumed:** `cargo-verus` ships inside the release zip. A verified crate adds
  `vstd = "=0.0.0-2026-09-20-0158"` from crates.io (an exact pin taken from Verus's toolchain
  manifest) and `[package.metadata.verus] verify = true`. The first `cargo verus` build verifies
  vstd itself (2059 items, 21–36 s); later builds use the cache. The plain `verus file.rs` CLI uses
  the prebuilt `vstd.vir` instead.
- **The toy kernel verifies:** 31 items in 0.57 s. It covers:
  - the blackboard job lifecycle (one transition function plus 5 theorems, including "any history
    accepts at most 4·max_retries+3 events");
  - a multi-job `Board`;
  - compaction-plan invariants: tool call and result never split, pinned items always kept,
    token sums never overflow.

  All 7 bugs I planted deliberately were rejected with precise errors.
- **A plain `cargo build`/`cargo test`/`clippy` on stable works.** Ghost code is erased; I checked
  the macro expansion and the compiled symbols. No `RUSTC_BOOTSTRAP` or feature flags are needed.
  One trap: importing a spec-only item with `use` breaks the plain build (E0432). The fix is
  `#[cfg(verus_only)]`.
- **clippy pedantic+nursery has no friction from the macro itself.** The first pass gave 18
  warnings, all on my own code and none on macro-generated code. Every suggested fix was accepted
  by Verus: slices, `if let`, `u64::from`, `const fn` (including `&mut self`), `#[must_use]`,
  `+=`, `Default`. The restriction lints `indexing_slicing` and `arithmetic_side_effects` duplicate
  what the proofs already guarantee.
- **The public API of a verified crate must not have preconditions.** A `requires` clause is only
  an assumption about the caller. Unverified callers can break it, and Verus does not stop them;
  I verified that. The pattern that works is private fields + type invariant + `View` +
  `apply() -> Result`. Verus's own checker for this, `-V check-api-safety`, reports false alarms
  on vstd's own specs, so it is unusable today.
- **`--no-cheating` rejects `assume`, `admit`, `external_body` and `assume_specification`.** This
  is the CI gate for proofs written by agents. Verus's own guide recommends the same loop: a
  coding agent, the verifier, and a check for cheats.
- **Features, measured on this release** (the docs table dates from 2026-05-13 and is partly out
  of date):
  - Verified: traits with specs, generics, closures (except ones that capture `&mut`), `for`
    loops, `map`/`collect`, Vec/slices/HashMap/String/Option/Result/`?`, `dyn Trait`, lifetimes,
    `Drop` (with annotations), and `async fn` + `.await`.
  - Fails: floats (vstd ships no float axioms, by design).
  - Unsupported: std `Mutex`, `println!`, fn pointers, destructuring assignment.
- **Dev-loop timings:**

  | Run | Time |
  |---|---|
  | Cold, empty target dir | 32 s |
  | No-op rerun | 0.16 s |
  | After an edit | 0.9 s |
  | `verus` CLI directly | 0.5 s |

  One trap: changing Verus flags does not mark the build as stale, so run `cargo clean -p <crate>`
  before re-running with new flags.
- **Prebuilt releases exist only for macOS arm64 and x86_64, Linux x86_64 and Windows x86_64.**
  There is no Linux arm64 build, so CI verification must run on x86_64 Linux or macOS arm64.
- **rustfmt does not format code inside `verus!{}`.** `verusfmt` 0.7.3 does, and mise can install
  it from GitHub; I did not install it.
- **Recommendation:** make one verified leaf crate, `aim-kernel`, whose only dependency is vstd:
  pure state machines and arithmetic, no async, I/O, serde or floats. Every other crate calls it
  through a total API. Lock about 9 decisions this way (ranked below). Do not verify I/O, async
  runtimes, providers, the UI or floating-point scoring.

## Findings

### F1. Installing Verus with mise (ran)

Verus releases: weekly stable tags `release/0.YYYY.MM.DD.<sha>` (cron every Monday,
`verus-src/.github/workflows/release.yml:5`) plus `release/rolling/<ver>` pre-releases.
Assets of `release/0.2026.09.20.aef82ed` (`gh release view`): `…-arm64-macos.zip` (449,371,666 B),
`…-x86-linux.zip`, `…-x86-macos.zip`, `…-x86-win.zip`. No linux-arm64 asset. INSTALL.md lists
first-tier prebuilt targets: macOS 14 arm, macOS 15 x86_64, Windows 2022, Ubuntu 24.04 x86_64
(`verus-src/INSTALL.md:6-9`).

```text
$ mise ls-remote github:verus-lang/verus | tail -2          # default options
release/0.2026.09.20.aef82ed
release/rolling/0.2026.09.24.b9416fa
$ mise ls-remote github:verus-lang/verus | tail -2          # with version_prefix = "release/"
0.2026.09.20.aef82ed
rolling/0.2026.09.24.b9416fa
$ mise install --force github:verus-lang/verus@0.2026.09.20.aef82ed
mise ✓ github:verus-lang/verus@0.2026.09.20.aef82ed  27.8s  verus-0.2026.09.20.aef82ed-arm64-macos.zip
$ mise exec -- verus --version
Verus
  Version: 0.2026.09.20.aef82ed
  Profile: release
  Platform: macos_aarch64
  Toolchain: 1.98.1-aarch64-apple-darwin (overridden by environment variable RUSTUP_TOOLCHAIN)
```

The `Toolchain:` line is text fixed when Verus's CI built the release. The zip's `version.json`
contains the identical string, and the line is printed unchanged with `RUSTUP_TOOLCHAIN` unset or
set to 1.97.1. It does not describe the local machine.

Proven config (`LAB/aim-toy/mise.toml`; the table form parses fine):

```toml
[tools]
rust = "1.98.1"   # MUST equal channel in verus-src/rust-toolchain.toml at the pinned tag

[tools."github:verus-lang/verus"]
version = "0.2026.09.20.aef82ed"
version_prefix = "release/"
filter_bins = ["verus", "cargo-verus"]
```

Equivalent CLI (ran): `mise use --tool-option version_prefix=release/ --tool-option 'filter_bins=["verus","cargo-verus"]' github:verus-lang/verus@0.2026.09.20.aef82ed`.
It writes the inline form:
`"github:verus-lang/verus" = { version = "0.2026.09.20.aef82ed", version_prefix = "release/", filter_bins = ["verus", "cargo-verus"] }`.

- **Asset selection needs no help.** Automatic matching picked `arm64-macos` here and `x86-linux`
  for `linux-x64` (seen in `mise lock`). No `asset_pattern` is needed. Rolling pin, not installed:
  `version = "rolling/0.2026.09.24.b9416fa"`.
- **Always pin an exact version.** The user's global config sets `prereleases = true`, so `latest`
  could resolve to a rolling pre-release.
- **Why the install is so big.** The zip is a copy of the whole cargo `target-verus/release` dir
  (`deps/`, `build/`, `.fingerprint/`, …). CI runs `cp -R ./target-verus/release …` then
  `zip -r` (`verus-src/.github/workflows/build.yml:98-103`), so the install is 1.4 GB (`du -sh`).
- **Why `filter_bins` matters.** Without it, the install root goes on PATH: `mise which z3`
  resolved to the install dir, and `air`, `rust_verify`, `line_count` and others were exposed.
  With it, only the symlinks in `.mise-bins/{verus,cargo-verus}` are exposed. `cargo-verus` still
  finds the driver through the symlink: the `-v` output shows
  `RUSTC_WRAPPER=…/.mise-bins/verus`, and `verus` follows the symlink to find its root.
- **Adding `filter_bins` to an already-installed version needs a reinstall.** mise reported:
  `mise ERROR No executable found for configured tool: verus … Reinstall it with: mise install --force github:verus-lang/verus@0.2026.09.20.aef82ed`.
- **No quarantine.** `xattr -l verus z3 rust_verify` shows only `com.apple.provenance:` and no
  `com.apple.quarantine`, and nothing prompted. INSTALL.md mentions a
  `macos_allow_gatekeeper.sh` script (`verus-src/INSTALL.md:52`), but this zip does not contain it
  and it is not needed when mise does the download.
- **Lockfile with checksums.** `mise lock --platform macos-arm64,linux-x64` wrote `mise.lock`
  entries with the URL, `url_api` and a checksum:
  `sha256:3f89fd25…d69377bc` (arm64-macos) and `sha256:7b870fa1…f0447b33` (x86-linux). Both are
  byte-identical to `gh api repos/verus-lang/verus/releases/assets/<id> --jq .digest`. The
  `lockfile` setting is not set on this machine.

### F2. Rust toolchain coupling (ran)

- **Verus pins its toolchain.** `verus-src/rust-toolchain.toml` at `release/0.2026.09.20.aef82ed`
  (identical on the rolling tag):
  `channel = "1.98.1"`, `components = ["rustc","rust-std","cargo","rustfmt","rustc-dev","llvm-tools"]`.
- **How the driver picks the toolchain** (`verus-src/source/verus/src/main.rs`):
  - `VERUS_USE_RUSTUP` (default on) at `:73`;
  - it checks that `rustup toolchain list` contains `TOOLCHAIN` and otherwise prints
    `rustup install …` (`:133-157`);
  - it then runs `rustup run <TOOLCHAIN> -- <root>/rust_verify` (`:195-199`).

  An explicit `rustup run` beats `RUSTUP_TOOLCHAIN`, so the workspace's own pin cannot redirect
  Verus.
- **The binary links against the toolchain's compiler library.** `otool -L rust_verify` shows
  `@rpath/librustc_driver-2446825d52b9075b.dylib` and `@rpath/libLLVM.dylib` with no LC_RPATH.
  The same file names exist in `~/.rustup/toolchains/1.98.1-aarch64-apple-darwin/lib/`.
  - Installed components are only cargo, clippy, rust-docs, rust-src, rust-std, rustc and rustfmt.
    There is no `rustc-dev` and no `llvm-tools`, and verification still works. **Those two
    components are only needed to build Verus from source; the default profile is enough to run
    it.**
- **mise's `rust` is rustup.** `~/.local/share/mise/installs/rust/1.98.1 -> ~/.cargo/bin`, and mise
  exports `RUSTUP_TOOLCHAIN=1.98.1`. Inside `mise exec` this even overrides a
  `RUSTUP_TOOLCHAIN` set by the caller. According to mise's docs, mise installs rustup if it is
  missing and accepts `profile`, `components` and `targets` options
  (https://mise.jdx.dev/lang/rust.html). It is therefore not an alternative to rustup but a front
  end for it, and it is compatible with Verus's `rustup run`.
- **Mismatch experiment.** `PATH=<verus>:$PATH RUSTUP_TOOLCHAIN=1.97.1 cargo verus verify` (cargo
  1.97.1) succeeded: `2059 verified` for vstd and `1 verified` for the crate.
  - Why: for crates that are not being verified, `rust_verify` calls
    `run_rustc_compiler_directly` (`verus-src/source/rust_verify/src/main.rs:97-100`). It compiles
    them with its *own* linked 1.98.1 rustc_driver, not the `rustc` cargo passed in.
  - `cargo-verus` itself runs whichever cargo invoked it. The `-v` output shows
    `…/toolchains/1.98.1-aarch64-apple-darwin/bin/cargo "check" …`, and the source has a TODO:
    `// TODO: use the "+ ... toolchain" argument?` (`cargo-verus/src/subcommands.rs:560`).
  - **Consequence:** everything built by one `cargo verus …` run is compiled by Verus's rustc with
    `RUSTC_BOOTSTRAP=1`.
- **Running without rustup.** `VERUS_USE_RUSTUP=0` alone fails:
  `dyld: Library not loaded: @rpath/librustc_driver-2446825d52b9075b.dylib … no LC_RPATH's found`.
  Adding `DYLD_LIBRARY_PATH=~/.rustup/toolchains/1.98.1-aarch64-apple-darwin/lib` makes it work
  (`15 verified`); `DYLD_FALLBACK_LIBRARY_PATH` does not.
  - UNVERIFIED: macOS System Integrity Protection strips `DYLD_*` variables when a process is
    launched through `/bin/sh`, so this route may break inside shell-based task runners. Prefer
    rustup.

### F3. cargo-verus, vstd and the crates.io crates (ran + read)

- **cargo-verus.** `cargo-verus` is in the release zip (`guide/src/cargo_verus.md:8`). It is a thin
  wrapper that runs `cargo check` (for `verify`, `focus`, `check`) or `cargo build` (for `build`)
  with these environment variables (`cargo-verus/src/subcommands.rs:455-461`):
  - `RUSTC_WRAPPER=<dir>/verus`
  - `RUSTC_BOOTSTRAP=1`
  - `__CARGO_DEFAULT_LIB_METADATA=verus`
  - `CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true`
  - per-package `__VERUS_DRIVER_VERIFY_<id>` / `__VERUS_DRIVER_ARGS_FOR_<id>`

  Crates opt in with `[package.metadata.verus] verify = true`. Other metadata keys are `no_vstd`,
  `is_vstd`, `is_core`, `is_builtin` and `is_builtin_macros` (`source/docs/CARGO-VERUS.md`).
- **crates.io.** `cargo search` / `cargo info` show:
  - `vstd 0.0.0-2026-09-20-0158`, with default feature `std` and optional `alloc`, `allocator`,
    `allow_panic`, `nonzero_internals`, `strict_provenance_atomic_ptr`. Its own manifest has
    `[package.metadata.verus] verify = true, is-vstd = true`.
  - vstd's dependencies are exact `=` pins: `verus_builtin =0.0.0-2026-09-16-0054`,
    `verus_builtin_macros =0.0.0-2026-09-20-0158`,
    `verus_state_machines_macros =0.0.0-2026-09-06-0133`.
  - `verus` and `cargo-verus` are 0.0.0 placeholders; `verusfmt 0.7.3` also exists.
- **Which vstd goes with which Verus.**
  - `source/cargo-verus/toolchain-manifests/0.2026.09.20.aef82ed.toml` (added to `main` by a bot
    after the release) gives `vstd = "0.0.0-2026-09-20-0158"`, `z3 = "4.16.0"`, `cvc5 = "1.1.2"`.
  - `cargo verus toolchain list` knows releases 0.2026.08.23 through 0.2026.09.20.
  - `cargo verus new --lib hello` generated `vstd = "=0.0.0-2026-09-20-0158"`, `edition = "2021"`,
    `verify = true`, a lint block allowing `cfg(verus_only)`, and a `.git` directory.
  - `cargo verus verify --check-toolchain -v` prints the Verus version and the vstd instances in
    use.
- **vstd builds on stable.** It gates its unstable features behind
  `#![cfg_attr(verus_keep_ghost, feature(...))]` (`vstd.rs:12-28` in the crates.io source), so it
  compiles on stable whenever Verus is not running.
- **First build cost.** The first `cargo verus` build in a target dir compiles and verifies vstd:
  `verification results:: 2059 verified, 0 errors`, 21–36 s (one 60 s run under heavy load). The
  result is cached in that target dir.
- **The plain CLI needs no Cargo.** `verus file.rs --crate-type=lib` uses the prebuilt
  `vstd.vir`/`libvstd.rlib` in the install dir, with no crates.io access (used for all the
  negative tests).
- **Kernel dependency closure (`cargo tree`): 16 crates.** Only `vstd` and `verus_builtin` are
  runtime dependencies. The rest are proc-macro dependencies (`syn`, `quote`, `proc-macro2`,
  `verus_syn`, `verus_prettyplease`, `synstructure`, `convert_case`, `indexmap`, `hashbrown`, …).

### F4. The toy workspace `LAB/aim-toy` (ran)

```text
aim-toy/
  mise.toml  mise.lock  Cargo.toml   # workspace: edition 2024, resolver 3, vstd pinned in workspace.dependencies
  crates/aim-kernel/   verify = true   src/{job.rs, board.rs, compaction.rs}  tests/lifecycle.rs
  crates/aim-probes/   verify = true   feature probes (F5)
  crates/aim-harness/  (no verus metadata) depends on aim-kernel via path, uses only its public API
LAB/negative/*.rs      seeded bugs, checked with the plain `verus` CLI
LAB/probes-fail/*.rs   feature probes that fail (f*), pass (ok_*), and false-spec controls (ctl_*)
```

Edition 2024 works for verified crates. The decision is written once as a spec function; the
code below is from `aim-kernel/src/job.rs:80`:

```rust
/// THE LOCKED DECISION: the complete transition function. `None` = rejected, job unchanged.
pub open spec fn next(pre: JobView, ev: Event) -> Option<JobView> {
    if is_terminal(pre.state) { None } else { match ev {
        Event::Claim { worker }    => if pre.state is Open { Some(with_state(pre, JobState::Claimed { worker })) } else { None },
        Event::Start { worker }    => if pre.state == (JobState::Claimed { worker }) { Some(with_state(pre, JobState::Running { worker })) } else { None },
        Event::Complete { worker } => if pre.state == (JobState::Running { worker }) { Some(with_state(pre, JobState::Done { worker })) } else { None },
        Event::Fail { worker }     => if pre.state == (JobState::Running { worker }) { Some(retry_or_fail(pre)) } else { None },
        Event::Expire              => if holder(pre.state) is Some { Some(retry_or_fail(pre)) } else { None },
        Event::Cancel              => Some(with_state(pre, JobState::Cancelled)),
    }}
}
// The executable code is proven to follow the spec exactly (job.rs:321):
pub const fn apply(&mut self, ev: Event) -> (r: Result<(), LifecycleError>)
    ensures match next(old(self)@, ev) {
        Some(post) => r is Ok && final(self)@ == post,
        None       => r is Err && final(self)@ == old(self)@,
    },
```

What is proven (31 items, all automatic or by short induction):

| Property (aim decision) | Where | How |
|---|---|---|
| Terminal states absorb every event | `lemma_terminal_absorbing` job.rs:138 | automatic |
| Only the holder may start, complete or fail a job; `Done` records the worker that finished it | `lemma_only_holder_progresses` :147 | automatic |
| The holder changes only through `Claim` of an `Open` job (a job is never held by two workers) | `lemma_holder_changes_only_by_claim` :160 | automatic |
| Each accepted event keeps `retries ≤ max_retries` and strictly reduces a progress measure | `lemma_step_bounded` :172 | automatic |
| Any event history is accepted at most 4·max_retries+3 times (retries bounded ⇒ the lifecycle terminates) | `theorem_bounded_lifecycle` :203 | induction over `Seq<Event>` |
| The retry counter never overflows `u32` | `#[verifier::type_invariant]` on private fields, :248 | `use_type_invariant` |
| On a multi-job board, one event changes only its own job, exactly as `next` says | `Board::apply` board.rs:61 (`old@.update(id, post)`) | extensional `=~=` |
| A compaction plan keeps pinned items and never splits a (ToolCall c, ToolResult c) pair | `plan_ok` compaction.rs:39; `repair_plan` :100 only adds kept items | `for` loop invariant plus a closed-form spec |
| Token totals never overflow: `Ok(t)` ⇒ t = Σ kept; `Err(Overflow)` ⇔ Σ > u64::MAX | `kept_tokens` :130 | `checked_add` plus a monotonicity lemma |

The mutable-reference syntax changed in recent releases. A postcondition must say `final(self)`
for the new value or `old(self)` for the value on entry; otherwise the error is "to dereference a
mutable reference parameter in a postcondition, disambiguate by wrapping it in either `old` or
`final`" (`source/docs/migration-mut-ref.md:77`). Type invariants additionally require
`use_type_invariant(&*self)` before a field assignment ("value may fail to meet its declared type
invariant after assignment"). Invariants are only allowed on types with no fields public outside
the crate (`guide/src/reference-type-invariants.md:34`).

**Seeded bugs (ran, `verus LAB/negative/<f> --crate-type=lib`).** Every one was rejected:

| File | Bug | Verus output (first lines) |
|---|---|---|
| n1_complete_without_claimant_check | the implementation lets any worker complete | `error: precondition not satisfied` (private `complete_unchecked`) and `postcondition not satisfied` on `apply` |
| n2_policy_allows_steal | the *policy* `next` allows re-claiming a Claimed job | `postcondition not satisfied` in `lemma_holder_changes_only_by_claim` (`pre.state is Open`) and in `lemma_step_bounded` (measure) |
| n3_unbounded_retry | retry without checking the budget | `value may fail to meet its declared type invariant` and `possible arithmetic underflow/overflow` |
| n4 / n4b | the plan drops the call of a kept result; in n4b the closed-form spec was changed to match, so implementation and spec agree with each other | loop invariant failure (n4); `plan_ok` postcondition failure (n4b), i.e. the decision itself caught it |
| n5_unchecked_token_sum | `acc + tokens` without `checked_add` | `possible arithmetic underflow/overflow` |
| n6_pub_requires_api | makes the requires-fn `pub` | **verifies (15 verified, 0 errors)**: nothing stops unverified callers from breaking a precondition |

**(a) Verification timings** (kernel = 31 items; `/usr/bin/time -p`):

| Command | Wall time |
|---|---|
| `cargo verus verify -p aim-kernel`, empty target dir (includes vstd 2059 items) | 31.96 s (95 s CPU) |
| same, no-op rerun | 0.16 s |
| same, after editing kernel content | 0.90 s |
| `cargo verus focus -p aim-kernel`, first time (separate `target/verus-partial` dir, deps rebuilt) | 19.49 s |
| `cargo verus focus`, warm, after an edit | 0.75 s |
| `cargo verus verify --workspace` (kernel + probes verified, harness passed through), warm | 0.85 s |
| `verus crates/aim-kernel/src/lib.rs --crate-type=lib --time` (prebuilt vstd) | 0.67 s; `total-time 520 ms`, `smt-run 104 ms` |
| `cargo verus build -p aim-harness` (codegen artifacts, so vstd is verified again) | 25.66 s |

**(b) Plain stable build (ran, cargo 1.98.1).**
- `cargo build --workspace`: 7.69 s cold, no warnings. `cargo test`: 3 passed.
  `cargo run -p aim-harness`: prints `job: Failed after 1 retries` and
  `plan [true, true, true, false] keeps Ok(4940) tokens`.
- **The build needs** the crates.io `vstd` (it supplies the `verus!` macro and prelude, and pulls
  `verus_builtin*`). It needs no features, no `RUSTC_BOOTSTRAP` and no nightly.
- **Ghost code is erased:**
  - `cargo rustc -- -Zunpretty=expanded` shows only exec code; `proof { … }` becomes `{}`, and no
    spec or proof function remains.
  - `nm` on the rlib lists only exec symbols (`Job::apply`, `repair_plan`, `Job::retry_or_fail`, …).
  - `JobView` (with `nat` fields) and `impl View for Job { type V = JobView; }` survive as empty
    shells.
- **Trap** (erasure.md): `use crate::job::{…, next, wf};` breaks the plain build with
  `error[E0432]: unresolved imports crate::job::next, crate::job::wf … no next in job`.
  - Fix: put spec-only imports under `#[cfg(verus_only)]`, and declare the cfg in
    `[workspace.lints.rust] unexpected_cfgs = { level = "warn", check-cfg = ['cfg(verus_only)'] }`.
  - Guarding anything other than imports with `verus_only` is unsound
    (`guide/src/erasure.md:49-51`).
- **Coexistence in one target dir.** `cargo verus verify` and plain `cargo build` can share
  `target/` without rebuilding each other (S1–S4: 36 s cold, then 0.40 s, 0.38 s, 0.23 s), because
  `__CARGO_DEFAULT_LIB_METADATA=verus` gives Verus's artifacts different hashes. A separate
  `--target-dir target/verus` is optional; I used one for clarity.

**(c) Clippy friction** (`cargo clippy -- -W clippy::pedantic -W clippy::nursery`, clippy 0.1.98).
The first pass on the kernel gave 18 warnings:
- 5 `missing_const_for_fn`
- 4 `ptr_arg` (`&Vec` → `&[_]`)
- 3 `must_use_candidate`
- 3 `missing_errors_doc`
- 1 each: `single_match_else`, `assign_op_pattern`, `cast_lossless`

Adding `Board` later produced `new_without_default` and one more `const fn` suggestion.
- **Every span pointed at my own code; none at macro-generated code.**
- **All were fixed with Verus still passing (31 verified).** The fixes: `&[T]` slices, `if let`,
  `u64::from`, `#[must_use]`, `const fn` (including `const fn apply(&mut self…)` with `match` and
  nested calls), `+=`, and `impl Default`. A second run is clean under `-D warnings`. Before/after
  copies are in `LAB/negative/*_before_clippy.rs.txt`.
- **Restriction lints report what is already proven.** With `-W clippy::indexing_slicing
  -W clippy::arithmetic_side_effects`, clippy flags 10 indexings and 8 arithmetic operations that
  Verus has already proven in bounds and free of overflow.
- **rustfmt leaves `verus!` bodies alone:** `cargo fmt --check` only reported diffs outside
  `verus!`.

**(d) Unverified crate depending on the verified one.** `aim-harness/Cargo.toml` contains only
`aim-kernel.workspace = true` (a path dependency), and the harness calls only `Job::apply`,
`Board::apply`, `repair_plan` and `kept_tokens`. `cargo verus verify --workspace` shows
`Checking aim-harness`, i.e. it passes through without being verified. A verified crate that
depends on another verified crate imports its `.vir` automatically
(`--VIA-CARGO import-dep-if-present=…`).

**Gates and traps (ran):**
- **`--no-cheating` works** (`cargo verus verify -p aim-probes -- --no-cheating`): three
  `error: external_body/assume_specification not allowed with --no-cheating` in the probes; the
  kernel passes (`31 verified`).
- **`-V check-api-safety` is unusable today.** The docs call it `-V check-safe-api`
  (`calling-verified-from-unverified.md:12`), but the CLI spells it `check-api-safety`. It aborts
  on vstd's own specs: `Safe API violation: 'requires' clause is nontrivial for function
  core::option::impl&%0::unwrap` (and `vstd::std_specs::iter::…::next` when `for` loops are used),
  before it ever reaches the planted `pub` requires-fn.
- **Changing Verus flags does not trigger re-verification.** After a successful verify, running
  again with `-- --no-cheating` or `-- -V check-api-safety` printed
  `Finished … in 0.01s` and verified nothing. `touch` does not help either, because freshness is
  judged by file content (`CARGO_UNSTABLE_CHECKSUM_FRESHNESS`). The flags only took effect after
  `cargo clean -p <crate> --target-dir …`.

### F5. Rust feature support (docs table vs. measured on 0.2026.09.20)

Source table: `guide/src/features.md` ("Last Updated: 2026-05-13", line 7). The "tested" column
was run in `aim-probes` (all 16 items verify) or `LAB/probes-fail`. For each surprising pass, a
deliberately false spec was also checked and failed, which confirms the pass is real.

| Feature | Docs | Tested here |
|---|---|---|
| Traits with spec fns + requires/ensures; generic `B: Budget` using them | Supported | ✔ verified |
| Closures with their own `ensures`; higher-order `f.requires((a,))` | Supported | ✔ |
| Closures capturing `&mut` | "no mutable captures" | ✘ `Verus does not currently support closures capturing a mutable reference` |
| `for i in 0..n` with invariant; `iter.index()`/`iter.seq()` ghost state | Partial | ✔ (kernel) |
| Iterator adaptors `v.iter().map(…).collect()` | Partial | ✔ verified; false-spec control fails |
| `Vec`, slices, `Option`/`Result`, `?`, enums with data (`is`, `matches`, `->Variant_field`) | Supported | ✔ |
| `HashMap<u64,u32>` (`m@` as `Map`) | Supported | ✔. Non-primitive keys need `assume(obeys_key_model::<K>())`, an explicit assumption the proofs rely on without proof (`vstd/std_specs/hash.rs:17`) |
| `String`/`&str` (`s@ == name@` via `to_owned`) | Supported | ✔ |
| Integer overflow / underflow | Supported | ✔ checked on every operation (n3, n5) |
| Floats | Partial | ✘ `(a + b)` fails `precondition not satisfied`. vstd "deliberately omits axioms about floating point" (`examples/float.rs:11`); you must write your own `broadcast axiom`s. An `external_body` wrapper works. |
| `async fn` with ensures; `.await` between verified async fns | "Not supported" | ✔ verified; false-spec control fails. `source/rust_verify_test/tests/async_functions.rs` has 11 tests, so the table is stale. Treat as new; tokio types have no specs. |
| `dyn Trait` calls to a trait method with `ensures` | Partial | ✔ verified; false-spec control fails |
| Lifetimes (`fn longest<'a>`) | Supported | ✔ |
| `impl Drop` | "Not supported" | ✔ only with `opens_invariants none` + `no_unwind` on `drop` (errors name both) |
| `const fn` (including `&mut self`) | Partial | ✔ |
| `derive(Clone, Copy, PartialEq, Eq, Debug)` inside `verus!` | (`Debug` not reasoned about) | ✔ compiles and verifies |
| `std::sync::Mutex` | Not supported | ✘ `std::sync::poison::mutex::Mutex is not supported` (use vstd locks, atomics or state machines) |
| `println!` / I/O | Not supported | ✘ `std::io::stdio::_print is not supported`; wrap in `external_body` |
| Function-pointer types | Not supported | ✘ `…does not yet support … function pointer types` |
| Destructuring assignment | Not supported | ✘ `…does not yet support … destructuring assignment` |
| `#[verifier::external_body]`, `assume_specification[u64::is_power_of_two]` | — | ✔ both (and both rejected by `--no-cheating`) |
| `unsafe`, raw pointers, `serde` derives inside `verus!` | Supported / Partial / "serde::Serialize: Not supported" | UNVERIFIED (not probed) |
| Code outside `verus!{}` (e.g. `pub async fn`, anything) | — | Ignored by Verus (neither verified nor constrained) |

### F6. CI and dev loop (ran + read)

- **mise tasks (ran).** All four tasks in `LAB/aim-toy/mise.toml` pass:
  - `mise run verify`: `cargo verus verify --workspace --target-dir target/verus`
  - `mise run verify:kernel`: with `-- --time`, which prints `total-time`, `smt-run`, …
  - `mise run verify:focus`
  - `mise run check`: build + test + `clippy -D warnings`
- **Cache in CI:**
  - the mise install dir `~/.local/share/mise/installs/github-verus-lang-verus/<ver>` (1.4 GB;
    `mise.lock` is the natural cache key);
  - `~/.rustup/toolchains/1.98.1-*`;
  - `target/verus` (123 MB in the lab).
- **Typical times:** 32 s cold (mostly vstd); under 1 s incremental on this ~300-line kernel. The
  SMT solver needed 104–115 ms. The default resource limit is `--rlimit 10`.
- **Keeping verification fast:**
  - Use `--verify-module`/`--verify-function` for local iteration (flags from `verus --help`).
  - `cargo verus focus` only pays off once several verified crates depend on each other.
  - Prefer a spec written as a closed formula plus loop invariants (as in `repaired` in
    compaction.rs) over quantifier-heavy spec functions, and avoid non-linear arithmetic.
- **Formatting and IDE.** `verusfmt` is published on GitHub with prebuilt binaries
  (v0.7.3, 2026-09-10; `mise install -n github:verus-lang/verusfmt@0.7.3` → "would install").
  `verus-analyzer` releases exist (latest 2026-09-15); UNVERIFIED with this setup.
- **Verus's own advice for LLM-written proofs** (`guide/src/llmforverusproof.md`):
  - use a coding agent that can run the verifier and read its full errors;
  - give it `vstd` and `rust_verify_test` as reference material;
  - add a "cheat checker" that flags new `assume`, `admit`, `external_body` or `axiom`, and changed
    specs or exec code (`:111-125`).

  `--no-cheating` enforces the first three mechanically.
- **Future concurrency tool.** `verus_state_machines_macros` (tokenized state machines,
  `source/docs/state_machines/`) is the tool for proving a *concurrent* blackboard. It was not
  needed for the single-writer toy.

## Implications for aim (recommendations; opinion)

**1. Workspace structure.**

```text
aim/
  mise.toml   mise.lock          # rust = <Verus's toolchain>, github:verus-lang/verus pinned, verify/check tasks
  Cargo.toml                     # [workspace.dependencies] vstd = "=<manifest vstd>"; [workspace.lints.rust] cfg(verus_only)
  crates/aim-kernel/             # the ONLY verified crate at first; deps: vstd only; leaf of the graph
    src/{job.rs board.rs compaction.rs budget.rs policy.rs negotiate.rs retry.rs effort.rs eventlog.rs}
  crates/aim-proto/              # wire and serde types; lossless From/TryFrom to kernel types (tested, not verified)
  crates/aim-harness*/ aim-agent*/ aim-tui/ …   # ordinary crates; call the kernel only through its total API
```

- **What goes in the kernel.** Pure, deterministic, synchronous state machines and integer
  arithmetic. The harness and agent layers own all I/O, async, time and randomness, and feed
  results in as plain values: the kernel receives `now_ms: u64` or a random `u64` as arguments.
  That keeps `external_body` at zero and lets `--no-cheating` apply to the whole kernel.
- **Public API rules for the kernel:**
  - no `pub fn` with `requires`;
  - private fields plus `#[verifier::type_invariant]`;
  - `impl View` for the abstract state, with `pub open spec fn` decisions over it;
  - errors returned, never assumed.

  `-V check-api-safety` is broken, so enforce the first rule with a small CI grep/AST check. Keep
  vstd and ghost types out of what other crates need; they only see exec items after erasure.
- **Name the decision in the code.** Each locked decision is a single `pub open spec fn` whose
  doc comment says `THE LOCKED DECISION` and cites the aim ADR, plus theorems about it. That spec
  function is the live documentation the brief asks for. The seeded-bug runs show that the
  theorems catch a wrong *policy* (n2), not only a wrong implementation.
- **Split only when needed.** Move to `aim-kernel-*` crates when kernel verification exceeds about
  30 s or when dependency direction requires it. Verified-to-verified imports work (`.vir`
  export).
- **Tradeoff.** Wire types are defined twice (`aim-proto` and the kernel) because serde derives
  and async do not belong inside `verus!`. That costs about 1 `From` impl per type in exchange for
  a kernel that builds in under a second.

**2. Toolchain strategy.**
- **Pin both, bump both together.** In the project `mise.toml`, set
  `rust = <channel from verus-src/rust-toolchain.toml at the pinned tag>`, pin the Verus release
  by exact version, and bump the two in one commit. A small `mise run verus:bump <tag>` task can
  read the tag's `rust-toolchain.toml` through `gh api`. Commit `mise.lock` (sha256 per platform).
- **Today the whole workspace can stay on 1.98.1**, which matches the user's global pin.
- **If the rest of aim needs a newer rustc than Verus supports,** that is fine as long as:
  - verification runs `-p aim-kernel` (never `--workspace` across crates that need the newer
    rustc);
  - the kernel's dependency closure stays on vstd, with `rust-version` set to Verus's rustc.

  The cost is two toolchains installed. Nothing leaks, because `rustup run` isolates Verus.
- **Release channel.** Use weekly stable releases, not rolling; bump monthly or when a needed
  feature lands (the toy used `final()`, which is recent). Watch for `vstd` exact pins and syntax
  changes on each bump.
- **CI (x86_64 Linux, since no linux-arm64 Verus exists):**
  - `mise install --locked`;
  - `mise run verify:kernel -- --no-cheating`, run after `cargo clean -p aim-kernel` or in a fresh
    target dir, so the flag is not skipped by the cache;
  - the ordinary `check` job on any platform.

  Cache the mise install dir, the rustup toolchain and `target/verus`.
- **Skip `rust-toolchain.toml`** unless an editor needs it. mise's `RUSTUP_TOOLCHAIN` overrides it
  in activated shells.

**3. Aim decisions worth locking with proofs** (ranked by value/cost; items 1–2 are already done in the lab):

| # | Decision | Property to prove | Effort |
|---|---|---|---|
| 1 | Blackboard job lifecycle ("fiverr for agents") | The `next` table; terminal states absorb; exclusive claim; only the holder progresses; ≤ 4·max_retries+3 accepted events; `Board` changes only the addressed job | done (<1 s) |
| 2 | Compaction plan invariants (tny ADR 0069 "provider sees a sound transcript", `refs/tny/docs/adr/0069-native-loop-stream-error-recovery.md:110-120`) | Pinned items kept; call and result never split; repair only adds; overflow-free token totals. Next step: the kept subsequence contains no orphan results (a sequence-filter lemma). | done / medium |
| 3 | Session event log | `append` ⇒ `final@ == old@.push(e)`; sequence numbers strictly increase; `fork(at)` ⇒ `child@ == parent@.take(at)` and the parent is unchanged; replay is a pure fold | low |
| 4 | Permission policy evaluation | Deny overrides allow; default deny, failing closed on unknown input (tny ADR 0059); monotone: adding a deny rule never grants anything and adding an allow rule never revokes a deny | medium (quantifiers over the rule sequence) |
| 5 | Token budget arithmetic | All counters are checked `u64`; reserve/commit never exceeds the budget; the compaction trigger is monotone in usage | low |
| 6 | Retry/backoff | `delay(n) ≤ cap`; non-decreasing in n; attempts ≤ max; jitter within `[0, delay]` given a random `u64` passed in | low |
| 7 | Effort controller | Level always within the canonical set (tny ADR 0009: `none…xhigh`); at most one step per decision; hysteresis (no flip-flop within a window of k signals) as a trace theorem. **Use integer or fixed-point scores**, because floats fail. | medium |
| 8 | Protocol version negotiation | `negotiate(ours, theirs)` returns the highest version both sides support; symmetric; `None` exactly when the two ranges do not overlap | low |
| 9 | Bounded mailboxes and queues (tny ADR 0146) | Capacity never exceeded; first-in first-out order; no loss on a rejected push | low |

**4. What NOT to attempt in Verus.**
- **Anything that does I/O or async:** tokio, SSH shadowing, HTTP/gRPC/WebSocket, SQLite, the
  filesystem, the daemon. Verus has no specs for these, and wrapping them in `external_body` adds
  unchecked assumptions without proving anything. `async fn` technically verifies now, but it
  brings no value without runtime specs.
- **Provider adapters, SSE/JSON parsing, OAuth flows, MCP and ACP bridges.** If parser
  correctness ever matters, the separate Vest project (`vest_lib` on crates.io) is its own
  effort.
- **TUI, web UI, the WASM plugin host, the skills and agents markdown loaders.**
- **Floating-point logic:** embedding similarity, Jev and router scores, cost estimates. Keep it
  unverified, or convert to fixed-point integers at the kernel boundary when a bound must be
  proven.
- **Real concurrency with std locks** (unsupported). Model the blackboard as a single-writer state
  machine behind the daemon's actor or lock. Consider `verus_state_machines` only if lock-free
  sharing ever becomes necessary.
- **Model behaviour, prompt quality, or learned predictions.** A verified bound should be about
  the controller, never about the model.
- **Verifying the whole workspace,** or crates that pull in large dependency trees.

**5. Hygiene for proofs maintained by agents.**
- `--no-cheating` is on for the kernel in CI.
- Any unavoidable trusted wrapper goes in a separate `aim-kernel-trusted` crate that is reviewed by
  a human, with flags limited to the roots via `--fwd-verus-args-to roots`.
- Planted-bug files like `LAB/negative/*` become regression tests that must *fail*
  verification.
- A diff of a `LOCKED` spec function requires human approval. Agents may freely change proofs and
  implementations.
- Clean the verification target before changing Verus flags.

## Open questions for the user

1. **Toolchain policy.** Should the whole workspace stay on Verus's rustc (1.98.1 today, bumped
   with Verus)? Or may non-kernel crates move ahead, with only `aim-kernel` constrained?
2. **Trust policy.** Is a strict `--no-cheating` kernel acceptable, meaning zero `external_body`
   and `assume_specification`, with time and randomness passed in as values? Or do you want a
   small reviewed trusted crate?
3. **Who may change a LOCKED spec?** Agents can edit proofs and code freely. Should changing the
   decision itself require your approval, for example through CODEOWNERS on the spec functions?
4. **CI platform.** Is running verification only on x86_64 Linux and macOS arm64 acceptable,
   given that Verus publishes no linux-arm64 build?

<!-- REPORT COMPLETE -->
