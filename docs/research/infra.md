# R6: SSH-shadowed execution, infrastructure crates, semantic conversation search

Sources:
- Refs under `refs/`, cited as `refs/<repo>/<path>:<line>`.
- Cargo registry sources (`~/.cargo/registry/src/index.crates.io-*/<crate>-<ver>/`).
- The crates.io API and `cargo info`, 2026-09-25.
- Zed `main@8720fd1d` (2026-09-24), cited as `zed:<path>:<line>`, permalinks at
  `https://github.com/zed-industries/zed/blob/8720fd1d093963be0aeb36954c15fd1aca1b94d2/<path>#L<n>`.
- herdr `master` fetched 2026-09-25, cited as `herdr:<path>:<line>`, links at
  `https://github.com/herdrdev/herdr/blob/master/<path>#L<n>`.
- Local measurements on an Apple M3 Ultra.

## TL;DR

1. **Treat SSH as a transport.** The harness already has to speak a protocol over unix sockets and WebSockets, so "SSH shadowing" should be the same protocol carried over a byte stream: `ssh host aimd proxy`.
   - Zed, herdr, VS Code and codex (whose `environments.toml` accepts `program = "ssh"`) all converge on this design.
   - Keep an agentless fallback for hosts that cannot take a binary: sftp plus `sh -c` over one ControlMaster, which is tny ADR 0022.
2. **Use the system OpenSSH binary as the transport.** Every exemplar does, and it covers all of ssh_config, agents, ProxyJump, FIDO, Kerberos and known_hosts.
   - Send prompts to the TUI or web UI with `SSH_ASKPASS` plus `SSH_ASKPASS_REQUIRE=force`, as Zed does.
   - The `openssh` crate cannot do this on its own. Its own master is started with `BatchMode=yes`, so it cannot prompt, and it never allocates a tty.
   - So aim spawns the master itself and then attaches with `Session::resume_mux`.
3. **`russh` 0.63 is capable but its config support is partial.** It supports PTY, agent auth, certificates, GSSAPI, direct-streamlocal and ML-KEM. But the Rust ssh_config parsers are incomplete: russh-config handles 10 directives, and ssh2-config has no `Match` and no `%` tokens.
   - Keep russh as an optional backend, for example for Windows clients.
4. **Latency and concurrency favour aimd.**
   - Agentless costs about 2 RTT per tool call, plus a local `ssh` spawn (13.6 ms measured here) and a remote `sh` spawn. It is also capped at **10 concurrent channels**, because sshd's `MaxSessions` defaults to 10.
   - aimd costs 1 RTT per operation on one channel, with pipelining and batching.
   - Search, glob and watch run on the remote with aimd, so for search-heavy work the gap is orders of magnitude.
5. **aimd = detached daemon + per-connection `proxy`.**
   - Output needs sequence numbers and must be replayable, so that commands and PTYs survive network drops.
   - Reference points: Zed's daemon exits after 10 min idle; codex keeps a detached session for 30 s; VS Code allows a 3 h reconnection grace.
6. **Bootstrap sequence.**
   1. Run `uname -sm`.
   2. Look for a versioned binary path.
   3. If it is missing, download the release asset locally, verify its sha256, and stream it over ssh stdin (herdr's method). Then `gunzip && chmod && mv` (Zed's method).
   - Ship static musl builds for x86_64 and aarch64 Linux, plus macOS. Zed ships exactly these, plus Windows.
   - Gate compatibility on a **protocol generation** number, not an exact version (herdr).
7. **Claude Code through claude-agent-acp runs its built-in tools in its own process.** The mechanism this adapter offers for shadowing them is to restrict the built-ins (`_meta.claudeCode.options.tools`, a list of tool names; `[]` disables them all) and pass aim's tools as ACP `mcpServers` (`refs/claude-agent-acp/src/acp-agent.ts:8374-8380`). tny refuses `--ssh` for ACP hosts for this reason (ADR 0022:60).
8. **Trait boundary.** Model it on codex's `ExecutorFileSystem` and `ExecBackend`/`ExecProcess`, which use URI paths and resumable `read(after_seq)` output. Add `Search`, `Watch` and capability flags. Enforce "tools never touch `std::fs`/`std::process`" with clippy `disallowed-methods`.
   - The failure this prevents: pi's ssh example still greps locally.
9. **Cloud buckets (fs half only).** OpenDAL 0.59.3 covers s3, gcs, azblob, webdav and more. Its sftp service is itself built on `openssh` plus `openssh-sftp-client`. Completion is `list(prefix)` with a cache.
10. **Crate flags.**
    - `reqwest-eventsource` requires reqwest 0.12, not 0.13.
    - `tui-textarea` requires ratatui 0.29, not 0.30.
    - `bincode` is unmaintained (RUSTSEC-2025-0141).
    - `fuzzy-matcher` has had no release since 2020.
    - `ort` has only release candidates.
    - axum's `ws` feature pins tokio-tungstenite 0.29.
    - The highest MSRV is sqlx at 1.94. Verus pins rustc 1.98.1, so nothing conflicts today.
11. **Store: rusqlite 0.40 with the bundled SQLite 3.53.2.** FTS5 is compiled in (verified `ENABLE_FTS5=1`), and sqlite-vec lives in the same DB file.
12. **Measured (best run; 2–3× slower on a shared machine).**
    - model2vec potion-base-8M: 0.6 ms per 4 KB, 14 MB/s, 16 µs per query.
    - model2vec potion-retrieval-32M: 11 MB/s, and it found the right documents where base-8M did not.
    - sqlite-vec KNN over 100k×256 vectors: 20 ms (13 ms with int8), rising linearly to 61 ms at 300k. Versions 0.1.6 and 0.1.9 are equivalent.
    - FTS5 bm25 top-20: 0.7 ms.
13. **Search design.**
    - Chunks: user messages, assistant final messages, tool-call digests, and compaction summaries.
    - Pipeline: FTS5 plus vector search, fused with RRF, then a Jev `Noul` rerank. The rerank is calibrated, costs about $0.001, and is a single request.
    - Surface results automatically only above a probability threshold; otherwise leave them to an on-demand tool.
14. **Jev facts.** Input costs $0.042 per million tokens and output is free. All questions in a request are evaluated in parallel against one shared state. jevgrep puts 250 candidates in the state per request and caps each chunk at 14k tokens.

## Findings

### 1. SSH shadowing — prior art (FACTS)

**tny (the user's own decisions: ADR 0022, ADR 0040, `src/core/ssh.c`, `src/core/tools_ssh.c`)**
- **Scope.** tny itself stays local: config, sessions, provider connection, TUI, permissions, MCP, skills and memory. Every workspace tool of the native loop is routed through one OpenSSH ControlMaster (`refs/tny/docs/adr/0022-ssh-execution-boundary.md:18-24`).
- **Tool mapping.**
  - `list_files`, `glob_files`, `grep_files` map to `ls -1Ap`, `find -path`, and `find … | xargs grep -nIF`.
  - `read_file` maps to `cat`; offset and limit are applied locally.
  - `write_file` sends content on stdin, then temp file, then `mv` (`tools_ssh.c:281-292`).
  - `edit_file` runs `cat`, then the local exact-match engine, then writes back (`tools_ssh.c:384-392`).
  - `semantic_search` is `grep -ci` per term.
  - A background terminal uses `nohup`, logging to `~/.tny-bg/`.
- **Connection.** Options: `ControlMaster=auto`, `ControlPersist=600`, ControlPath `~/.tny/ssh/%C` (mode 0700). The master is opened interactively *before* the TUI takes the terminal, so OpenSSH can prompt. Tool calls then use `BatchMode=yes`, at "~10–30 ms per call" (0022:36-44; `ssh.c:120-139,282-316`).
- **Shell handling.** Works whatever the remote shell is: `cd '<cwd>' && exec sh -c '<script>'`, single-quote escaping, and file content never on the command line (`ssh.c:25-32,160-167`).
- **Model awareness and instructions.** The model is told that the workspace is remote. Remote-cwd `AGENTS.md` is loaded and labelled; local ancestor `AGENTS.md` files are not (ADR 0040).
- **Limits.**
  - Native loop only: `--ssh` is refused for Codex and ACP hosts (0022:60-63).
  - tny never adds `StrictHostKeyChecking=no` (0022:65).
  - The remote needs only sshd, POSIX sh and coreutils (0022:70).
  - There is no remote `/undo` (0022:72).

**pi** (`refs/pi/packages/coding-agent/examples/extensions/ssh.ts`)
- **Injection point.** Each tool takes an injectable `*Operations` object: `ReadOperations` (`src/core/tools/read.ts:35-42`), `BashOperations` (`bash.ts:59-71`), `WriteOperations`, `EditOperations`, `LsOperations`, `FindOperations`, `GrepOperations`.
- **Grep and find are not shadowed.** The example overrides only read, write, edit and bash. `grep` and `find` still spawn local `rg` and `fd` (`grep.ts:119,168`; `find.ts:172,216`).
- **Connection.** One `ssh` process per operation, with no ControlMaster.
- **Quoting bug.** Paths are quoted with `JSON.stringify` (`ssh.ts:50`), which does not neutralise `$` or backticks inside double quotes.

**oh-my-pi**
- **`ssh://` scheme.** `ssh://host/<path>` is accepted by every FS-shaped tool; it is capped at 1 MiB and needs a POSIX shell (`refs/oh-my-pi/packages/coding-agent/src/prompts/internal-urls/ssh.md:1`). sshfs mounts are also supported (`src/ssh/sshfs-mount.ts`).
- **ssh options.** `ControlMaster=auto`, `ControlPersist=3600`, `BatchMode=yes`, `StrictHostKeyChecking=accept-new` (`src/ssh/connection-manager.ts:264-271`).
- **ControlPath length budget.** `sun_path` is 104 bytes on macOS and 108 elsewhere. OpenSSH first binds a temporary path (ControlPath + "." + 16 characters) (`connection-manager.ts:41-60`).

**codex: `codex exec-server`**, a JSON-RPC server for processes and filesystem access (`refs/codex/codex-rs/exec-server/README.md`)
- **Methods** (`exec-server-protocol/src/protocol.rs:22-57`):
  - Handshake: `initialize` → `initialized`.
  - Processes: `process/{start,read,write,signal,terminate}`, plus notifications `process/{output,exited,closed}`.
  - Environment: `environment/{info,status}`.
  - Filesystem: `fs/{readFile,open,readBlock,close,writeFile,createDirectory,getMetadata,canonicalize,readDirectory,walk,remove,copy}`.
  - HTTP: `http/request`.
- **Process API.**
  - `process/start {processId, argv, cwd:"file:///…", env, tty, pipeStdin}`.
  - Output arrives as base64 chunks with a per-process `seq`.
  - `process/read {afterSeq, maxBytes, waitMs}` is a long poll.
  - Requests are sequential unless `--concurrent-requests` is set. Closing the ws kills the connection's processes (README:175-182).
- **Transports.**
  - WebSocket: one JSON-RPC message per ws message.
  - stdio.
  - A Noise-encrypted relay whose frames carry `seq`, `ack`, `ack_bits`, `resume` and `heartbeat` (README "Remote Relay Message Format").
- **SSH is just a stdio environment.** An environment is either a `url` or `program` + `args`. The fixture is `program="ssh", args=["dev","codex exec-server --listen stdio"]` (`exec-server/src/environment_toml.rs:39-51,417-423`). There is no auto-install.
- **Resume.** A detached session is kept for **30 s** so a client can resume with `resume_session_id` (`exec-server/src/server/session_registry.rs:17-20`).
- **Traits.**
  - `ExecutorFileSystem` has `canonicalize`, `read_file`, `read_file_stream`, `write_file`, `create_directory`, `get_metadata`, `read_directory`, `walk`, `remove` and `copy`. Every method takes a `&PathUri` and an optional sandbox, and returns a boxed future (`codex-rs/file-system/src/lib.rs:625-700`).
  - `ExecBackend::start` plus `ExecProcess::{read(after_seq,max_bytes,wait_ms), write, signal, terminate, subscribe_events}` (`exec-server/src/process.rs:199-245`).
- **stdio-to-uds.** codex also ships a stdio↔unix-socket bridge (`codex-rs/stdio-to-uds/src/lib.rs:10-30`).

**herdr 0.9.1** (Rust; `herdr machine add`, `--machine <id> <api cmd>`, `--remote`)
- **Platform detection.** Runs `uname -s; uname -m` and maps Linux/Darwin × x86_64/amd64/aarch64/arm64 (`herdr:src/remote/attach.rs:1415,195-207`).
- **Install source, in order:**
  1. `HERDR_REMOTE_BINARY`.
  2. The local executable, if the remote has the same platform and the executable is not package-manager-managed.
  3. A release asset **downloaded on the client** with curl and checked against sha256 (`attach.rs:1759-1776,2320-2346`).
- **Upload.** A prepare script creates a temp path, the file is streamed into the stdin of `ssh … 'cat > tmp'`, and a commit script moves it into place (`attach.rs:809-850`).
  - Existing installs on PATH, Homebrew, mise or Nix are tried first (docs).
- **ssh options.** `BatchMode=yes`, `StrictHostKeyChecking=yes`, `ServerAliveInterval=15`, `ServerAliveCountMax=4`, `ControlMaster=auto`, `ControlPersist=600` (`attach.rs:1116-1150`). A temporary ssh config includes the user's config first.
- **Bridge.** On the remote, a bridge connects ssh stdio to the running server's unix socket (`herdr:src/remote/host.rs:1-37`). The bridge's idle timeout is 60 s, measured on a clock that includes suspend (`herdr:src/platform/remote_bridge.rs:8-20`).
- **Compatibility.** Compatibility means `ENDPOINT_PROTOCOL_GENERATION` is equal; otherwise the server must stop or do a live handoff (`herdr:src/remote/restart_policy.rs:14-40`). The docs say "compatible client and server versions do not have to match".
- **Reconnect and secrets.** Reconnects run independently per machine, with backoff capped at 2 min. Passwords and control sockets are never stored (connecting-machines docs).

**Zed** (`zed@8720fd1`)
- **Transport.**
  - Uses the system `ssh`, `scp` and `sftp`; there is no Rust SSH library (`zed:crates/remote/src/transport/ssh.rs:197,1204,1229,1356`).
  - The master is started with `ssh -N -o ControlPersist=no -o ControlMaster=yes -o ControlPath=<tmpdir>/ssh.sock`, with `SSH_ASKPASS_REQUIRE=force` and `SSH_ASKPASS=<sh script piping to zed --askpass=<unix sock>>`. Passwords, passphrases and host-key prompts therefore appear in the UI (`ssh.rs:180-213`; `zed:crates/askpass/src/askpass.rs:276-346`).
  - It reuses the user's live ControlMaster, found through `ssh -G` and `ssh -O check` (`ssh.rs:557-631`).
  - It never sets BatchMode, StrictHostKeyChecking or ServerAlive.
- **Bootstrap.**
  - Probes run as separate execs over the mux: a Windows check, `echo $SHELL`, `uname -sm`, `os-release`/`sw_vers`, then `<bin> version` (`ssh.rs:791-828`).
  - The binary lives at `~/.zed_server/zed-remote-server-{channel}-{semver+build.sha}`. It counts as installed if `version` exits 0; there is **no hash check**.
  - By default the remote downloads it with curl or wget. As a fallback or opt-in, the client downloads it (keeping 5 versions) and uploads with `sftp -b -` or scp. Install is `gunzip -f && chmod 755 && mv` (`ssh.rs:966-1157`).
  - Dev builds use `cargo zigbuild` for `*-linux-musl` with `+crt-static`.
  - Release assets: linux-{x86_64,aarch64} static musl (asserted to have no libssl), macos-{x86_64,aarch64}, and windows zips.
- **Processes.**
  - `proxy --identifier <id> [--reconnect]` runs once per SSH connection. It bridges stdio to the three unix sockets of the `run` daemon, which keeps a pid file.
  - A fresh (non-reconnect) connect restarts the daemon.
  - `--reconnect` with a dead daemon exits with code 90.
  - The daemon exits after 10 min without a connection (`zed:crates/remote_server/src/server.rs:410`).
  - There is no explicit setsid.
- **Wire format.**
  - Frames are a `u32` little-endian length followed by a prost `Envelope{id, responding_to, ack_id, oneof payload}` (`zed:crates/remote/src/protocol.rs:8-51`).
  - A stream is many responses sharing one `responding_to`, ended by `EndStream`.
  - Every envelope carries `ack_id=max_received`. The sender keeps unacknowledged envelopes in an unbounded `VecDeque` (`zed:crates/remote/src/remote_client.rs:1703-2071`).
- **Reconnect.**
  - Heartbeat every 5 s with a 5 s timeout; 5 missed heartbeats trigger a reconnect.
  - At most 3 attempts, with no backoff (`remote_client.rs:160-166`).
  - A reconnect re-runs the probe and install steps, starts `proxy --reconnect`, and then both sides replay their buffers (`remote_client.rs:587-777`).
- **What runs remotely.** Worktree scan and watch, buffers, LSP, git, tasks, DAP, and **project search**, which streams `FindSearchCandidatesChunk` messages.
- **Terminals bypass the server.** A local PTY runs `ssh -t` through the ControlPath (`zed:crates/project/src/terminals.rs:520-577`).
- **Connection abstraction.** The `RemoteConnection` trait has `start_proxy`, `upload_directory`, `kill`, `build_command`, `build_forward_ports_command`, `path_style`, `remote_platform`, `shell`, and more. It is implemented by SSH, WSL and Docker/Podman connections; Docker runs `docker exec -i <ctr> <bin> proxy` and copies the binary with `docker cp` (`remote_client.rs:1617-1666,1271-1336`).
- **Security.** The daemon listens only on unix sockets: no TCP and no token. Socket modes are left to the umask (UNVERIFIED whether that gives 0700).

**VS Code Remote-SSH** (closed source; from its docs and VSIX strings)
- **Install.** The server is downloaded on the host by default. `remote.SSH.localServerDownload` can be `auto`, `always` or `off`; the client-side path uses scp. Servers are pinned by commit in `~/.vscode-server/bin/<commit>`.
- **Connection.** The server listens on a localhost TCP port, protected by a connection-token file and reached through SSH forwarding. Builds are glibc ≥ 2.28 only, so Alpine is not supported.
- **Reconnect.** A 13-byte frame header (`type|id|ack|len`) with resend of unacknowledged frames. Keepalive every 5 s, 20 s timeout, and a **3 h reconnection grace** (`microsoft/vscode@5ae24bc:src/vs/base/parts/ipc/common/ipc.net.ts:289-313`).

**distant** (Rust)
- The last release is 0.20.0 (2023-07-15).
- `launch` starts a distant binary that must already be installed on the remote; it installs nothing.
- The pure-SSH mode emulates the API over sftp + exec. It is not usable as a dependency.

**claude-agent-acp**
- **Built-in tools run locally.** Claude Code's built-in tools use the `claude_code` preset unless `_meta.claudeCode.options.tools` is given, or `_meta.disableBuiltInTools === true`, which sets `tools: []` (`refs/claude-agent-acp/src/acp-agent.ts:8374-8380`). `tools` is a list of tool-name strings; the tests pass `tools: ["Read", "Glob"]` and `tools: []` (`src/tests/create-session-options.test.ts:324,357`).
- **MCP servers.** A session accepts ACP `mcpServers` (`acp-agent.ts:1238,1319`).
- **ACP fs methods are unused by the built-ins.** The ACP client methods `readTextFile`/`writeTextFile` are only passthrough definitions; the built-in tools do not call them (`acp-agent.ts:1876-1916,7300-7308`).

### 2. SSH crates (FACTS)

**openssh 0.11.6** (MSRV 1.63)
- **Platform and concurrency.** Unix only (`openssh-0.11.6/src/lib.rs:1`). "the maximum number of multiplexed remote commands is 10 by default", because of sshd `MaxSessions` (`lib.rs:17-19`). `sshd_config(5)` confirms the default is 10.
- **Builder.** `user`, `port`, `keyfile`, `known_hosts_check`, `connect_timeout`, `server_alive_interval`, `control_directory`, `control_persist`, `config_file`, `compression`, `jump_hosts`, `user_known_hosts_file`, `ssh_auth_sock`. `connect` gives process-mux; `connect_mux` gives native-mux (`builder.rs:121-388`).
- **Own master cannot prompt.** It is launched as `ssh -E log -S ctl -M -f -N -o ControlPersist… -o BatchMode=yes` (`builder.rs:413-425`). `Session::resume_mux(ctl, log)` attaches to a master someone else started (`session.rs:173`).
- **No PTY.** "native_mux_impl never allocates a tty" (`native_mux_impl/child.rs:41`). The session API is `command`, `raw_command`, `subsystem`, `shell`, `request_port_forward`, `close`, `detach` and `check`.

**openssh-sftp-client 0.15.9**
- Created with `Sftp::from_session(session, SftpOptions)` (`src/sftp/openssh_session.rs:144`), with a pipelined request queue.
- `Fs` methods: `open_dir`, `create_dir`, `remove_*`, `canonicalize`, `hard_link`, `symlink`, `rename`, `read_link`, `set_permissions`, `metadata`, `read` and `write` (`src/fs/mod.rs:48-440`).
- Capability probes: `support_fsync`, `support_posix_rename`, `support_copy`, `support_expand_path`, `max_read_len`, `max_write_len`.

**russh 0.63.3** (MSRV 1.89)
- **Features.** Defaults are `flate2`, `aws-lc-rs` and `rsa`; `ring` is an alternative crypto backend (`Cargo.toml:36-54`).
- **Auth.** none, password, keyboard-interactive, publickey, `authenticate_publickey_with(Signer)` for agent auth, OpenSSH certificates, and GSSAPI-with-MIC (`src/client/mod.rs:319-634`). The agent client works over `SSH_AUTH_SOCK` or Pageant (`src/keys/agent/client.rs:65-97`). Helpers exist to check and learn known_hosts entries (`src/keys/known_hosts.rs:15-137`).
- **Channels** (`client/mod.rs:795-937`):
  - session and x11.
  - `direct-tcpip`: ProxyJump has to be built by hand, via `connect_stream` (`:1102`).
  - `direct-streamlocal`: can dial aimd's unix socket directly.
  - `tcpip`/`streamlocal` forwarding.
  - Per channel: `request_pty`, `window_change`, `exec`, `subsystem`, `signal` and `agent_forward` (`src/channels/mod.rs:205-316`).
- **Keepalive and kex.** `keepalive_interval`, `keepalive_max` and `inactivity_timeout` (`client/mod.rs:1295-1346`). Key exchange includes curve25519 and `mlkem768x25519` (`src/kex/mod.rs:17-40`).

**Config parsers and `ssh -G`**
- **russh-config 0.58.0** parses only Host, User, HostName, Port, IdentityFile, ProxyCommand, ProxyJump, AddKeysToAgent, UserKnownHostsFile and StrictHostKeyChecking; everything else is ignored (`src/lib.rs:222-293`).
- **ssh2-config 0.8.0** has no `Match` and no `%` tokens (`README.md:81-84`).
- **`ssh -G host`** prints the effective config after evaluating Host and Match blocks (ssh(1)).

**Measured here** (OpenSSH_10.4p1, 50 iterations each)
- Spawning `ssh -G` (process plus config parse, no network): **13.6 ms**.
- `sh -c true`: 4.7 ms.

**Comparison of the three approaches** (r = RTT; per-operation costs are derived from the mechanisms above, except where a measurement is cited)

| | (a) OpenSSH ControlMaster, agentless (`openssh` + sftp) | (b) `russh` + `russh-sftp`, agentless | (c) `aimd` bootstrapped over one ssh channel (the design proposed in §A, modelled on Zed/herdr/codex) |
|---|---|---|---|
| Latency per operation | process-mux: 13.6 ms local spawn + about 2r + remote `sh` spawn (tny ADR 0022:44 reports ~10–30 ms per call). native-mux: about 2r + remote spawn. sftp on an open channel: 1r (stat) to 2r (open, then read+close) | about 2r per exec; 1–2r per sftp op; no local spawn | 1r; pipelined; `read_many`/`stat_many` batches cost 1r |
| Concurrency | at most `MaxSessions`=10 channels in total (exec + sftp + PTY) | same | unbounded on 1 channel |
| Search-heavy work | remote `rg`/`grep` via exec, if installed; over sftp, every byte crosses the wire and each directory costs ≥ 2r | same | ripgrep libraries in-process on the remote; only hits cross the wire |
| PTY | none in the crate; `ssh -tt -S ctl` under a local portable-pty (what Zed does for terminals) | `request_pty` / `window_change` | owned by the daemon; survives disconnects |
| Watch | none (poll, or `inotifywait` if present) | none | remote `notify`, events pushed |
| Reconnect | the master dies with its TCP connection, taking in-flight commands (unless nohup'd) | same, but handled in-process | daemon keeps processes and seq'd output buffers; client resumes |
| Auth coverage | everything OpenSSH supports: Include/Match, ProxyJump/ProxyCommand, agent, certificates, FIDO/PKCS#11, GSSAPI, askpass, known_hosts CA and hashing | password, keyboard-interactive, pubkey, agent, certificates, GSSAPI; config handling and ProxyJump must be rebuilt | same as its transport |
| Zero-install | yes (POSIX sh + coreutils) | yes | no; uploads a few MB and needs an executable directory on a supported OS/arch |
| Security surface | the user's ssh policy; the ControlPath socket gives a full shell to anyone who can open it | host-key checking is aim's own code | adds binary integrity checks and daemon socket permissions |
| Client OS | unix only | any, including Windows | same as its transport |

Search-heavy workload, as a model (not measured). Assume r = 50 ms and a repo of 20k files in 2k directories, 200 MB.
- **sftp:**
  - Walking costs ≥ 2k × 2r divided by the number of requests in flight.
  - Reading every byte at 10 MB/s takes about 20 s.
- **aimd or remote `rg`:**
  - One request, taking 1–2r.
  - Plus a ripgrep run at local speed.
  - Plus the hits.

### 3. Infrastructure crates (FACTS)

Versions and dates come from the crates.io API. "stable" is `max_stable_version` and the date is its publish date. MSRV comes from `cargo info` or the manifest; "–" means none declared. The verdict column is opinion.

| crate | latest (stable) | MSRV | published | purpose | verdict |
|---|---|---|---|---|---|
| tokio / tokio-util | 1.53.1 / 0.7.19 | 1.71 | 2026-07 | runtime; codecs, CancellationToken | use |
| axum | 0.8.9 | 1.80 | 2026-04 | HTTP/WS server (web UI, remote harness) | use; `ws` feature pins tokio-tungstenite ^0.29 |
| hyper / hyper-util | 1.11.1 / 0.1.21 | 1.63 / 1.85 | 2026-08/09 | HTTP core | indirect |
| tonic / prost | 0.14.6 / 0.14.4 | **1.88** / 1.85 | 2026-05/06 | gRPC / protobuf | defer; codex uses both |
| tokio-tungstenite | 0.30.0 | 1.85 | 2026-07 | WebSocket | use 0.29 to share a version with axum; codex runs a fork of 0.28 |
| jsonrpsee | 0.26.0 | 1.85 | 2026-05 | JSON-RPC framework | avoid; ACP, MCP and codex hand-roll JSON-RPC |
| reqwest | 0.13.5 | 1.85 | 2026-09 | async HTTP; default TLS is rustls + aws-lc-rs + platform-verifier | use (codex is still on 0.12) |
| ureq | 3.4.2 | 1.85 | 2026-09 | blocking HTTP | only indirectly (typesafe-jev uses it) |
| eventsource-stream | 0.2.3 | – | 2022-02 | SSE parser over any byte stream | ok: finished, used by codex |
| reqwest-eventsource | 0.6.0 | – | 2024-03 | SSE + reqwest + retry | **avoid: requires reqwest ^0.12** |
| sse-stream | 0.3.0 | – | 2026-09 | SSE over `http-body` (rmcp) | alternative |
| rusqlite (libsqlite3-sys) | 0.40.2 (0.38.2) | – | 2026-08 | SQLite; bundled 3.53.2 with FTS5 | **use** |
| tokio-rusqlite | 0.8.0 | – | 2026-09 | async wrapper around a DB thread | optional |
| sqlx | 0.9.0 | **1.94** | 2026-05 | async SQL with compile-time checks | avoid for the MVP (macros, MSRV); codex uses it |
| libsql | 0.9.30 (0.10-pre) | – | 2026-03 | SQLite fork with vectors and replication | avoid (focus moved to `turso`: UNVERIFIED) |
| turso | 0.7.2 (0.8-pre) | – | 2026-07 | SQLite rewrite in Rust: MVCC, FTS via tantivy, exact vector search only; "not yet 1.0" | watch |
| sqlite-vec | 0.1.9 (0.1.10-α4) | – | 2026-03 | `vec0` KNN; brute force; ANN (DiskANN/IVF) only in alphas | use 0.1.9 |
| tantivy | 0.26.2 | **1.86** | 2026-09 | Lucene-style search | skip; FTS5 is enough |
| model2vec-rs | 0.3.0 | **1.88** | 2026-09 | static embeddings | **use** (`local-only`) |
| fastembed | 7.1.0 | **1.88** | 2026-09 | ONNX embedders and rerankers | later; pulls `ort`, downloads ONNX Runtime, native-tls by default |
| ort | 2.0.0-rc.13 (no stable) | **1.88** | 2026-07 | ONNX Runtime | avoid for the MVP |
| usearch / hnsw_rs | 2.26.2 / 0.3.4 | – | 2026-08 / 02 | HNSW (C++ via cxx / pure Rust) | later, above ~1M vectors |
| ratatui / crossterm | 0.30.2 / 0.29.0 | **1.88** / 1.63 | 2026-06 / 2025-04 | TUI | use; codex patches a crossterm fork |
| tui-textarea | 0.7.0 | 1.56 | 2024-10 | textarea widget | **avoid: ratatui ^0.29** |
| ratatui-textarea / tui-textarea-2 | 0.9.2 / 0.13.2 | **1.86** / **1.88** | 2026-06 / 08 | textarea (ratatui org / fork) | use ratatui-textarea, or write our own |
| edtui | 0.11.7 | – | 2026-08 | vim-style editor widget | alternative; heavy (2.8 MB, syntect, arboard) |
| pulldown-cmark / comrak | 0.13.4 / 0.55.0 | 1.71 / 1.85 | 2026-05 / 09 | markdown pull parser / GFM AST | pulldown-cmark; comrak if the AST must round-trip |
| syntect + two-face | 5.3.0 / 0.5.2 | – / 1.79 | 2025-09 / 2026-08 | TextMate highlighting + bat's syntaxes | use |
| tree-sitter-highlight | 0.27.0 | **1.90** | 2026-08 | tree-sitter highlighting | later; needs one grammar crate per language |
| portable-pty | 0.9.0 | – | 2025-02 | PTY (wezterm) | use; old dependencies (nix 0.28, winapi); codex uses it |
| notify (+debouncer-full) | 8.2.0 (9.0-rc) / 0.7.0 | 1.77 / 1.85 | 2025-08 / 2026-01 | filesystem events | use 8.2 |
| ignore | 0.4.33 | **1.88** | 2026-08 | gitignore-aware parallel walk | use |
| grep / grep-searcher / grep-regex | 0.4.1 / 0.1.17 / 0.1.14 | – | 2025-10 / 2026-07 | ripgrep as a library | use, inside aimd |
| similar / imara-diff | 3.2.0 / 0.2.0 | 1.85 / 1.71 | 2026-08 / 2025-06 | diffs | similar; imara-diff for large inputs |
| schemars | 1.2.2 | 1.74 | 2026-07 | JSON Schema for tool definitions | use (codex is still on 0.8) |
| serde_json / simd-json / sonic-rs | 1.0.151 / 0.18.1 / 0.5.10 | 1.71 / **1.88** / – | 2026 | JSON | serde_json only |
| thiserror / anyhow | 2.0.21 / 1.0.104 | 1.77 / 1.68 | 2026 | errors | thiserror in libraries, anyhow only in binaries |
| tracing (+subscriber) | 0.1.44 / 0.3.23 | 1.65 | 2025-12 | tracing | use |
| clap | 4.6.7 | 1.85 | 2026-09 | CLI | use |
| keyring | 4.2.0 | **1.88** | 2026-08 | OS keychains; v4 = keyring-core + per-store crates | use (codex is on 3.6) |
| etcetera / dirs / directories | 0.11.0 / 7.0.0 / 6.0.0 | **1.87** / – / – | 2025-10 / 2026-09 / 2025-01 | config/data directories | etcetera (explicit XDG strategy); RUSTSEC-2020-0054 against directories was withdrawn |
| webbrowser / open | 1.2.4 / 5.4.4 | 1.85 / 1.62 | 2026-08 / 09 | OAuth browser launch / open files | both |
| cpal | 0.18.2 | 1.85 | 2026-08 | audio capture | use (codex voice-host pins =0.18.2) |
| hound | 3.5.1 | – | 2023-09 | WAV writer, no dependencies | use: PCM16 at 24 kHz, as in tny ADR 0079 |
| opus | 0.4.0 | – | 2026-08 | libopus via opusic-sys (C) | skip unless upload bandwidth matters |
| arboard | 3.6.1 | 1.71 | 2025-08 | clipboard | use (codex enables `wayland-data-control`) |
| nucleo / nucleo-matcher | 0.5.0 / 0.3.1 | – | 2024-04 / 02 | fuzzy matching (helix) | use the matcher; crates.io release is stale, so codex pins a git revision |
| frizbee | 0.13.0 | **1.89** | 2026-08 | SIMD Smith-Waterman fuzzy matching (blink.cmp) | alternative |
| fuzzy-matcher | 0.3.7 | – | 2020-10 | skim matcher | **avoid (dead)** |
| bpe-openai / tiktoken-rs | 0.3.1 / 0.12.1 | – / 1.85 | 2026-08 / 09 | OpenAI BPE token counting | bpe-openai (linear-time BPE); tiktoken-rs as an alternative |
| tokenizers (HF) | 0.23.2 (1.0-rc) | – | 2026-09 | HF tokenizers (onig C) | only indirectly (model2vec pulls 0.21) |
| rustls / rustls-platform-verifier | 0.23.45 / 0.7.1 | 1.71 / 1.85 | 2026-09 | TLS / OS trust store | use |
| tikv-jemallocator / mimalloc | 0.7.0 / 0.1.52 | – | 2026-05 | allocators | jemalloc on linux-musl only (codex `cli/src/main.rs:45-51`); benchmark before adopting elsewhere |
| openssh / openssh-sftp-client | 0.11.6 / 0.15.9 | 1.63 / 1.64 | 2025-12 / 2026-09 | OpenSSH mux client / sftp | **use** |
| russh / russh-sftp | 0.63.3 / 3.0.0 | **1.89** / – | 2026-09 | pure-Rust SSH | optional backend |
| russh-config / ssh2-config / ssh_config | 0.58.0 / 0.8.0 / 0.1.0 | 1.85 / **1.88** / – | 2026-03 / 2026-08 / 2020 | ssh_config parsers | avoid; use `ssh -G` instead |
| ssh2 / async-ssh2-tokio / wezterm-ssh / distant | 0.9.6 / 0.13.0 / 0.4.0 / 0.20.0 | – | – | libssh2 bindings / wrappers / dead | avoid |
| opendal / object_store | 0.59.3 / 0.14.2 | **1.91** / 1.85 | 2026-09 | storage abstraction (70+ services incl. s3/gcs/azblob/sftp/webdav) / Arrow object store | opendal for buckets |
| postcard / bincode / rkyv / tarpc | 1.1.3 / 3.0.0 / 0.8.18 / 0.38.0 | – / 1.85 / 1.81 / 1.85 | – | wire encodings / RPC | postcard as an optional binary encoding; **bincode unmaintained (RUSTSEC-2025-0141)**; skip tarpc |

MSRVs above 1.85:
- sqlx: 1.94.
- opendal: 1.91.
- tree-sitter-highlight: 1.90.
- russh, frizbee: 1.89.
- ratatui, tonic, fastembed, model2vec-rs, ort, keyring, ignore, notify-rc, simd-json, ssh2-config, tui-textarea-2: 1.88.
- etcetera: 1.87.
- tantivy, ratatui-textarea: 1.86.

Verus pins `channel = "1.98.1"` (`verus-lang/verus/rust-toolchain.toml`, 2026-09-24), which is the same as the local rustc 1.98.1.

### 4. Semantic search (FACTS)

**Measurements.** Scratch benchmark `r6bench`: release build, single thread, on an Apple M3 Ultra. The corpus was `refs/tny/docs/adr/*.md`, packed into 716 chunks of about 1.5 KB each (0.85 MB). How the numbers were chosen:
- **Best run first, with a range.** Each cell gives the best (first, quiet) run, then in parentheses the range over 5 further runs at 1-minute load average 2–3.
- **Other agents shared the machine.** It was running other agents at the time; single-threaded timings varied 2–3×, and one cold model load took 922 ms.
- **Discarded runs.** Runs at load average 16–22 were 5–13× slower, including the pure-Rust loop, and are not used.
- **Quote with care.** Re-run on a dedicated machine before quoting these numbers in a design document.

| operation | potion-base-8M (256-d, 30 MB) | potion-retrieval-32M (512-d, 129 MB) |
|---|---|---|
| model load (local safetensors) | 17.9 ms (21–72) | 49.5 ms (74–109) |
| encode one 4 KB text | 0.61 ms (0.8–1.5) | 0.71 ms (1.2–2.0) |
| encode one query (~70 chars) | 16 µs (16–42) | 17 µs (17–47) |
| encode the corpus (default `max_length=512`) | 60 ms: 11.9k chunks/s, 14.1 MB/s (3.9–11.9 MB/s) | 75 ms: 9.5k chunks/s, 11.3 MB/s (1.9–6.4 MB/s) |

Retrieval sanity check (cosine, top-3 files). This corpus is clean markdown, not transcripts with tool dumps, so real sessions will differ.
- **"ssh without installing":** both models return 0022 and 0040.
- **"log in with a ChatGPT subscription":**
  - base-8M returns wrong files, with *higher* scores: 0070 at 0.57, 0152 at 0.55, 0067 at 0.52.
  - retrieval-32M returns the right files: 0066 at 0.34 and 0065 at 0.33.
  - So raw cosine thresholds are neither portable across models nor calibrated.
- **"sandboxing shell commands":** retrieval-32M's top-3 are all ADR 0060; base-8M ranks 0110 first.

SQLite (rusqlite 0.40.2 bundled, sqlite-vec 0.1.6 in the best run). Two quiet reruns with **0.1.9** matched within about 15%: KNN 21–24 ms, int8 14 ms, bm25 0.69–0.80 ms, Rust loop 11.5–13.3 ms.
- `sqlite_version()=3.53.2`, and `sqlite_compileoption_used('ENABLE_FTS5')=1`. The source is `libsqlite3-sys-0.38.2/build.rs:159`, which passes `-DSQLITE_ENABLE_FTS5`.
- FTS5 with `porter unicode61`, 14,320 rows: insert takes 218 ms; a bm25 top-20 query takes **0.70 ms**.
- `vec0` with `float[256]` and cosine, 100k rows: insert takes 561 ms; a k=10 KNN takes **20.5 ms**; with `int8[256]` it takes **13.0 ms**. A naive scalar Rust loop takes 11.3 ms.
- At 300k rows: 61.0 ms, 38.8 ms and 33.7 ms respectively. Scaling is linear, about 0.2 µs per 256-d vector, which extrapolates to about 200 ms at 1M.

**Components**
- **model2vec-rs 0.3.0.**
  - `encode` defaults to `max_length=512` tokens and `batch_size=1024` (`model.rs:416`).
  - The `local-only` feature disables Hugging Face hub downloads.
  - It is single-threaded (no rayon) and depends on tokenizers 0.21 (onig C), safetensors and ndarray 0.15.
  - Its README claims 8000 samples/s on a single thread.
- **sqlite-vec releases.**
  - 0.1.7 (2026-03) was a "revival release after hiatus".
  - 0.1.9 is brute force, with metadata and partition columns.
  - 0.1.10-alpha.1..4 (2026-03 to 05) add `rescore`, `ivf` (experimental) and DiskANN indexes (GitHub releases).
- **API embeddings.** OpenRouter offers `POST https://openrouter.ai/api/v1/embeddings` in the OpenAI format. Examples: `baai/bge-m3` at $0.01/M tokens, `openai/text-embedding-3-large` at $0.13/M, `google/gemini-embedding-001` at $0.15/M (openrouter.ai model pages).
- **codex.** Uses the `bm25` crate (2.3.2) in memory for tool search and skill selection (`codex-rs/core/src/tools/handlers/tool_search.rs:11-13`). Its "memories" pipeline has a model extract a `raw_memory` and a `rollout_summary` per idle session into the state DB, then consolidates them (`codex-rs/memories/README.md`). There is no vector search.
- **Jev / typesafe-jev 0.2.0.**
  - Endpoint and model: `DEFAULT_BASE_URL=https://api.typesafe.ai/v1/systemone`, `jev-latest`.
  - Price: `USD_PER_INPUT_MTOK = 0.042`, and output is free (`src/lib.rs:123-129`).
  - Question types: `Noul` (a calibrated P(yes)), `Choice` (up to 255 options) and `Score` (2–10 ordered levels). All are evaluated in parallel against one `state`.
  - Client: blocking `ureq` with rustls, `Send+Sync` with a shared pool. An `AdaptiveLimiter` halves concurrency on throttling. Retries honour `Retry-After`. An over-long request returns `Error::TokenLimit`, meaning "split and retry" (README).
- **jevgrep, the user's reference design.**
  - Its triage step puts candidates *in the state* keyed `p0..pN` and asks one `Noul` per candidate referencing `paths.p{i}`, 250 per request (`refs/jevgrep/src/search.rs:344-375`).
  - Text that is asked about per-question stays out of the shared state, because "state is shared, and there it skews the relevance answers" (`search.rs:210`).
  - Chunk budget: `max_tokens: 14_000` (`src/files.rs:432`).
  - Observed throughput: 72 requests, 301k tokens, about $0.0127, in 2.2 s with 32 lanes (README).

## Implications for aim (OPINION)

### A. Execution layer: one protocol, pluggable transports, capability-typed backends

**A1. Shape.** `aimd` is the harness; the agent layer is a client of it.

```
agent layer (local daemon: sessions/SQLite, providers, TUI, web)
   └─ HarnessClient ── transport ──> aimd (tools, fs, exec, pty, search, watch, git)
        transports: in-process | unix socket | ws/http | stdio of a spawned command
        stdio examples: `ssh [mux] host ~/.aim/bin/aimd-g3-0.4.1-x86_64-linux-musl proxy`,
                        `docker exec -i ctr aimd proxy`, `kubectl exec -i pod -- aimd proxy`
   └─ AgentlessSsh backend (no aimd): same client trait, emulated with sftp + `sh -c` over the mux
   └─ Bucket backend (OpenDAL Operator): fs capability only
```

This is the codex `environments.toml` model (url | program+args) plus Zed and herdr's bootstrap-then-proxy. The SSH-specific pieces are only (1) the connection manager, (2) the bootstrapper and (3) the agentless emulation. Every tool is written once, against the trait.

**A2. Trait boundary.** Protocol-first: typed request and response enums live in an `aim-harness-proto` crate, and a thin ergonomic trait sits on top of them. The local backend dispatches the same enums in-process without serialising. That gives one conformance suite for every backend, record/replay, and state machines Verus can check.

```rust
pub struct EnvId(..);                      // "local", "ssh:dev", "s3:bucket", ...
pub struct WsPath { env: EnvId, path: RemotePath }   // POSIX bytes; URI form on the wire (file:///…, s3://…)
pub struct Caps { exec: bool, pty: bool, watch: bool, native_search: bool, atomic_rename: bool,
                  max_concurrency: Option<u16>, proto_gen: u32, os: Os, arch: Arch, shell: Option<String> }

pub trait Workspace: Send + Sync {
    fn caps(&self) -> &Caps;
    fn fs(&self) -> &dyn Fs;                              // every backend
    fn exec(&self) -> Option<&dyn Exec>;                   // None for buckets
    fn search(&self) -> &dyn Search;                       // native or emulated over Fs/Exec
    fn watch(&self) -> Option<&dyn Watch>;
}
pub trait Fs: Send + Sync {            // boxed futures as in codex ExecutorFileSystemFuture
    fn stat(&self, p: &WsPath) -> BoxFut<'_, Meta>;
    fn read(&self, p: &WsPath, range: Option<ByteRange>) -> BoxFut<'_, Bytes>;
    fn read_many(&self, ps: &[WsPath], cap: usize) -> BoxFut<'_, Vec<Result<Bytes>>>; // 1 RTT
    fn write(&self, p: &WsPath, data: Bytes, pre: Precondition /* IfAbsent | IfHash(h) | Any */) -> BoxFut<'_, Meta>;
    fn edit(&self, p: &WsPath, edits: &[ExactEdit], pre: Precondition) -> BoxFut<'_, EditOutcome>; // runs remote in aimd
    fn list(&self, p: &WsPath, o: ListOpts /* prefix, page_token, limit */) -> BoxFut<'_, Page<DirEntry>>;
    fn mkdir / remove / rename / copy ...
}
pub trait Exec: Send + Sync {          // codex ExecBackend/ExecProcess shape
    fn spawn(&self, s: ProcSpec /* argv|shell, cwd, env, pty: Option<Size>, stdin */) -> BoxFut<'_, ProcId>;
    fn read(&self, id: ProcId, after_seq: Option<u64>, max: usize, wait: Duration) -> BoxFut<'_, Chunk>;
    fn write_stdin / resize / signal / wait ...
}
pub trait Search: Send + Sync { fn grep(&self, q: GrepQuery) -> BoxStream<'_, Hit>;
                                fn glob(&self, q: GlobQuery) -> BoxStream<'_, WsPath>; }
```

Rules that make "every tool call is shadowed" enforceable, not merely conventional:
- Tool crates list `std::fs::*`, `std::process::Command`, `tokio::fs::*` and `tokio::process::*` under clippy `disallowed-methods`/`disallowed-types`. Only `aim-backend-local` may use them. This avoids pi's leak, where grep and find ran locally.
- Paths the model sees are workspace-relative, or absolute paths on the remote. `WsPath` confinement (no `..` escape, symlink policy) is a pure function, a good first Verus target.
- `Precondition::IfHash` stops lost updates when the model edits a file that changed remotely. Stat plus hash is cheap in aimd.

**A3. Protocol over stdio/ssh.**
- **Framing.** `u32` length prefix (Zed uses little-endian) around serde-typed messages. Use JSON in v1 (debuggable; the same shape as codex exec-server, ACP and MCP). Optionally negotiate a binary encoding later (postcard, not bincode).
- **Handshake.** `hello{proto_gen, min_gen, build_id, caps}`. Accept any matching protocol generation (herdr) rather than pinning a commit (Zed, VS Code). Install side by side as `aimd-g<gen>-<ver>-<target>` so several local aim versions can coexist.
- **Streams.** Per-stream `seq` numbers for process, PTY, search and watch output. The server keeps ring buffers (for example 8 MiB per process). A client reconnects with `resume{session_id, last_seq per stream}`.
- **Replay.** Replay unacknowledged request frames the way Zed does (`ack_id=max_received` on every frame), but **bounded**, and with idempotency keys so that duplicate writes and spawns are detected. Zed has an unbounded buffer and no duplicate suppression.
- **Liveness.** Heartbeat every 5 s. Reconnect with exponential backoff from 1 s up to 2 min (herdr), not 3 immediate attempts (Zed).
- **Daemon.** `aimd serve` detaches with setsid, a pid file and a flock. It listens only on `$XDG_RUNTIME_DIR/aim/aimd-g<gen>.sock` or `~/.aim/run/…` (directory 0700, short path because of the `sun_path` limit). There is **no TCP listener**.
  - Detached sessions live for a configurable time, 30 min by default. Codex's 30 s is too short for a laptop that goes to sleep.
  - The daemon exits after an idle period with no clients and no live processes.
  - `aimd proxy` is spawned once per ssh connection and just copies frames; herdr's bridge and codex's `stdio-to-uds` are the same idea.

**A4. SSH connection manager (system OpenSSH).**
1. **Resolve.** `ssh -G <dest>` gives the effective config (Zed does this). Reuse a live user ControlMaster if `ssh -O check` succeeds.
2. **Master.** Use one shared, stable ControlPath, `~/.aim/ssh/%C` (tny/herdr-style sharing). `%C` is a 40-hex-character hash, so the path is well under the 104-byte limit.
   1. Probe it with `ssh -O check`.
   2. Only if no master is alive, take a local flock `~/.aim/ssh/<hash>.lock` and start `ssh -f -N -o ControlMaster=yes -o ControlPersist=<n> -o ControlPath=~/.aim/ssh/%C <dest>` with `SSH_ASKPASS=<aim exe> askpass --sock …` and `SSH_ASKPASS_REQUIRE=force`.
   3. All other invocations use `ControlMaster=no`.
   - Two aim sessions to the same host then share one master without racing.
   - Zed avoids the race differently, with `ControlMaster=yes` and a per-session temporary socket.
   - Prompts (password, 2FA, host key, key passphrase) reach the TUI or web UI even when the headless daemon starts the master (Zed's askpass approach).
   - Add `ServerAliveInterval=15`/`ServerAliveCountMax=4` only when the user's config does not set them (herdr).
   - Never override `StrictHostKeyChecking`; make `accept-new` an explicit opt-in.
3. **Attach.** Use `openssh::Session::resume_mux(ctl)` for exec and subsystem channels, and `Sftp::from_session` for the agentless fallback.
   - `resume_mux` needs the *expanded* socket path. Read it from `ssh -G -o ControlPath='~/.aim/ssh/%C' <dest>` (`controlpath /Users/…/.aim/ssh/cb4e5d…`, verified locally), which is also how Zed reads it.
   - PTY in agentless mode: `ssh -tt -S ctl host …` inside a local `portable-pty` (Zed's terminals).
   - The same TCP connection carries aimd's single stdio channel.
4. **Keep russh behind the same `SshConnector` trait** for Windows clients, or where no OpenSSH exists. It is not the default.

**A5. Bootstrap.** The happy path is one probe round trip.

1. **Probe.** `uname -sm; printf '%s\n' "$HOME"; ls -1 ~/.aim/bin/ 2>/dev/null`. Accept Linux/Darwin × x86_64/aarch64 (Zed and herdr parsing). Anything else goes agentless.
2. **Choose a binary, in order:**
   1. The local cache `~/.cache/aim/aimd/<ver>/<target>`.
   2. The local executable, if the target matches.
   3. The release asset from GitHub, with sha256 from a signed manifest.
   4. In dev, `cargo zigbuild --target *-linux-musl -C target-feature=+crt-static` (Zed).
3. **Upload over the mux stdin** (herdr): `umask 077; mkdir -p ~/.aim/bin; t=$(mktemp …); gzip -dc > "$t"; chmod 700 "$t"; mv "$t" <dst>`.
   - This needs no remote internet, no sftp and no remote curl.
   - Afterwards, verify with `sha256sum`/`shasum -a 256` on the remote when either exists.
   - Otherwise, the trust chain is the manifest-checked local download plus the authenticated SSH channel. A binary's own `version --sha256` is a self-report, not an integrity check.
   - Zed does no hash check at all.
4. **Start.** `exec <dst> proxy --session <id>` becomes the harness channel.
5. **Fall back to agentless when needed:** noexec home, unknown architecture, policy `ssh.bootstrap = "never"`, or a failed upload. Agentless mode advertises reduced `Caps`: no watch, PTY via `ssh -tt`, search through `rg --json` if present and `find|grep` otherwise, and edit as read, local apply, `IfHash` write (tny's mapping).
6. **Distribution targets:** `x86_64`/`aarch64-unknown-linux-musl` (static, with jemalloc as the global allocator, as codex does for musl) and `aarch64`/`x86_64-apple-darwin`. Windows comes later.
   - aimd needs **no TLS and no provider credentials**, since LLM traffic stays local. It is therefore a small static binary with the ripgrep libraries, `ignore`, `notify`, `portable-pty` and `git` via exec.
   - Publish with the same GitHub-release plus `mise use github:…` flow that jevgrep uses.

**A6. External agents.**
- The codex (Responses) and OpenAI-compatible integrations use aim's own tool loop, so they are shadowed automatically.
- For Claude Code through ACP, start the session with `_meta.claudeCode.options.tools = [only non-fs/shell built-ins]` and pass `mcpServers:[{name:"aim", command:"aim", args:["mcp","--env","ssh:dev"]}]`. aim exposes its harness tools as an MCP server whose calls route to the chosen `Workspace`.
  - The same MCP bridge shadows any MCP-capable agent.
  - Without it, aim must refuse SSH mode for that agent, as tny does (0022:60), rather than silently running its tools locally.

**A7. Security defaults.**
- Credentials: the remote never sees provider keys, OAuth tokens or the session DB.
- Agent forwarding is off by default; enable it per host for `git push` from the remote.
- Remote `AGENTS.md` is loaded and labelled as remote (ADR 0040), and its content is treated as untrusted tool data.
- Binary and socket: the uploaded binary is sha256-pinned, the socket directory is 0700, and there is no TCP listener. Permission prompts show `user@host:/path` (tny).
- The model's preamble states that the workspace is remote (tny ADR 0022, "Model awareness").

**A8. Verus targets (pure, synchronous, no I/O):**
- `WsPath` normalisation and confinement.
- POSIX single-quote escaping (`ssh.c:25-32` semantics).
- Frame codec bounds.
- The seq/ack replay buffer: no loss, no duplicate delivery, bounded memory.
- Exact-match edit application: a unique match, or an error.

Keep async I/O outside the verified kernel. Verus pins its rustc (1.98.1 today), so only the small kernel crate should be tied to that toolchain.

**A9. Cloud buckets.** Implement `Fs` over OpenDAL `Operator` (`services-s3/gcs/azblob/webdav/...`), with `Caps{exec:false, pty:false, watch:false}`.
- Directories come from `list(prefix, delimiter="/")`.
- Completion is served from a per-prefix TTL cache (for example 30 s) with a background prefetch of the next level, so typing `@s3://bucket/pa…` stays interactive despite LIST latency.
- Tools that need exec are hidden for that environment.

### B. Crate stack (picks)

- **Runtime and protocol:** tokio + tokio-util `LengthDelimitedCodec`; hand-rolled serde JSON-RPC; axum 0.8 for ws and http; tokio-tungstenite 0.29.
  - Skip tonic and jsonrpsee until gRPC is actually required.
- **HTTP:** reqwest 0.13 (rustls, aws-lc-rs, platform verifier) plus eventsource-stream, or a 50-line SSE parser. Never reqwest-eventsource.
  - typesafe-jev brings ureq. Call it from `spawn_blocking`, and accept the second HTTP stack or offer a Transport PR later.
- **Storage:** rusqlite 0.40 `bundled` + sqlite-vec 0.1.9 + FTS5 in one WAL database, owned by one DB actor thread. Keep it behind a `Store` trait so turso can be evaluated after 1.0.
- **Search and files:** `ignore`, `grep-searcher`, `grep-regex`, `globset`, `notify` 8.2 with debouncer-full, `similar` 3, `portable-pty` 0.9.
- **SSH:** `openssh` 0.11 (native-mux) + `openssh-sftp-client` 0.15; askpass mode built into the `aim` binary. Buckets via `opendal` 0.59.
- **TUI:** ratatui 0.30, crossterm 0.29, ratatui-textarea 0.9 (or our own composer), pulldown-cmark 0.13, syntect with two-face, arboard, nucleo-matcher (pinned git revision, as codex does) or frizbee.
- **Platform:** keyring 4, etcetera, webbrowser, clap 4, tracing, thiserror 2, schemars 1.
- **Audio:** cpal 0.18, rubato, hound, producing a 24 kHz mono PCM16 WAV as tny ADR 0079 does.
- **Allocator:** jemalloc on musl only.
- **Token counting:** bpe-openai for OpenAI encodings. Claude has no public tokenizer, so use the provider's count endpoint or an estimate.

### C. Semantic search over past conversations

**C1. Schema, in the session DB.** An FTS5 contentless table can mirror `chunks` via triggers.

```sql
chunks(id INTEGER PRIMARY KEY, session_id, turn_id, kind /* user|assistant|tool_digest|summary|title */,
       project /* repo root or remote env id */, created_at, tokens, content_hash UNIQUE, text)
chunks_fts USING fts5(text, content='chunks', content_rowid='id', tokenize='porter unicode61')
chunks_trigram USING fts5(text, content='chunks', content_rowid='id', tokenize='trigram') -- optional: identifiers, paths
chunks_vec USING vec0(project TEXT partition key, embedding float[512] distance_metric=cosine)
  -- partition keys (max 4), metadata and +auxiliary columns exist in sqlite-vec 0.1.9 (sqlite-vec.c:1988-2042,2143,3484)
embed_meta(model_id, dims, created_at)   -- re-embed in the background on model change
```

**C2. Chunking.** Prefer semantic units to fixed windows.
- **User messages:** whole, split at 400 tokens with 15% overlap. model2vec truncates at 512 tokens by default.
- **Assistant final messages:** same treatment.
- **Tool calls:** embed a **digest**, not the raw output. For example: `ran "cargo test -p x" → 3 failures in src/a.rs (E0308)`, or `edited src/a.rs (+12/-3): <first changed hunk header>`.
  - Raw tool output is noisy, huge and often full of secrets. Keep it retrievable by id, and index it with FTS5 only, capped at the first N KB.
- **Compaction summaries:** every summary aim produces becomes a `summary` chunk at no extra LLM cost.
- **Session title and summary:** generated when the session goes idle, like codex memories Phase 1.
- **Exclusions:** redact secrets before indexing. `--ephemeral` and private sessions are never indexed. The index lives in the local daemon even for SSH sessions.

**C3. Retrieval pipeline.** Steps run in execution order. Everything is local except the optional Jev calls.
0. **Gating, for automatic surfacing only.** Add one `Noul("Does the current request refer to or benefit from earlier sessions?")` to the per-turn Jev call that aim already makes for dynamic reasoning effort.
   - Jev answers questions in parallel, so this adds tokens but almost no latency.
   - An on-demand `search_sessions` call skips this step.
1. **Embed the query:** model2vec, about 20 µs.
2. **Candidates:** FTS5 bm25 top-50 (about 1 ms) ∪ vec0 KNN top-50 within the current project partition, plus a global pass (about 20 ms per 100k chunks). Filter by time and project.
3. **Fuse:** Reciprocal Rank Fusion, `score = Σ 1/(60+rank)`, which needs no score calibration. Collapse to at most 3 chunks per session.
4. **Rerank (optional):** one Jev request.
   - `state = {task:"decide which past-conversation excerpts help the current task", current:<goal + last user msg, ≤1k tokens>, candidates:{c0:…, c29:…}}`.
   - Questions: `c{i}: Noul("Would candidates.c{i} give information that materially helps the current task?")`.
   - Size: 30 × 300 tokens ≈ 10k tokens, under jevgrep's 14k chunk budget, costing about $0.0004. The observed latency class is 1–2 s (UNVERIFIED for this payload).
   - Keep candidate text in the state and use per-candidate questions, following jevgrep's triage pattern.

**C4. Surfacing policy.**
- **Always available:** `search_sessions(query, scope=project|all, since?, k=8)` returns `session_id`, `turn_id`, `kind`, date and a 1–2 line snippet. `read_session(session_id, turn_id, window)` expands a hit on demand.
- **Automatic surfacing** happens only when the gating `Noul` is at least 0.7 *and* at least one reranked candidate is at least 0.8.
  - Inject at most 3 hits as a ≤200-token "related past sessions" note, with ids and one-line summaries, never raw text.
  - Surface at session start and on topic shifts, never on every turn.
  - Calibration is what makes fixed thresholds meaningful. Raw cosine is not calibrated: relevant hits here scored anywhere from 0.33 to 0.65, depending on the model.
- **Without a Jev key:** automatic surfacing is off by default, and only the tool remains.

**C5. Budget per query.**
- Without Jev: under 50 ms for p50 at 100k chunks, and about 250 ms at 1M.
- With Jev: +1–2 s and about $0.0005.
- Indexing runs in the background at about 10k chunks/s on one core.
- A heavy user producing about 5k chunks/day reaches 1M in about 6 months. At that point, switch on int8 or binary quantisation with rescoring, partitioning, or ANN (sqlite-vec DiskANN once stable, or usearch) behind an `Index` trait.

**C6. Embedding choice.**
- **Default:** local `potion-retrieval-32M` (512-d). It is better than base-8M on the checks above and still sub-millisecond per chunk.
  - Download it on first use into the aim data directory (129 MB f32), or ship a PCA/int8 variant.
- **Optional:** an OpenAI-compatible embeddings endpoint (OpenRouter `/api/v1/embeddings`; bge-m3 at $0.01/M tokens). Record `model_id` per row, and re-embed in the background when the model changes.
- **Skip for the MVP:** fastembed/ort (BGE/gte). They are heavier to build and ship, `ort` has no stable release, and the gain over retrieval-32M + BM25 + Jev rerank is unmeasured here.

## Open questions for the user

1. May aim upload and run its `aimd` binary on remote hosts by default? Or should it ask each time per host, or stay agentless-only on some hosts, such as production boxes?
2. Are password or 2FA SSH logins in scope? This decides whether the askpass-to-UI path is MVP work.
3. Which remote OS/arch targets matter first: Linux x86_64/aarch64 only, or macOS and Windows hosts too?
4. For Claude Code over SSH: is it acceptable to replace its built-in Read/Write/Bash tools with aim's MCP tools, which changes Claude Code's tool prompts? Or should SSH mode be refused for that integration?
5. Which bucket providers come first (S3, GCS, R2, Azure), and must bucket paths be writable or read-only?
6. May conversation excerpts be sent to TypeSafe (Jev rerank) or to an embeddings API? Or must search stay fully local, with no calls to Jev or embedding services?

<!-- REPORT COMPLETE -->
