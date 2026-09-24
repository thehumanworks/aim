# aim — vision and brief

> This is the founding brief for aim, kept verbatim so every agent working on aim builds from the
> same source of truth. Decisions that refine it live in [`docs/adr/`](adr/); the architecture
> lives in [`docs/architecture.md`](architecture.md).

## Goal

Build **aim** ("Agent I am"): a harness made for all agents, built by the agents, updated by the
agents for the agents.

## Conception (maintainer's words)

Use the following ideas and requirements as an initial scope of work to give you some direction to
what we are trying to build. Some ideas are raw and you must treat them as a chance for you to
explore, research, ideate and create. We should strive to keep the project extensible, since this
is a harness for agents, built by the agents that use it — so the MVP has to explore that
flexibility maximally, rather than locking us in a lot of assumptions and constraints that reduce
the capability of the harness to evolve.

### Requirements

- Use Rust as the programming language:
  - fully typed;
  - strict clippy rules;
  - Verus for formal verification — use verification as the proof of correctness, with the proofs
    serving as live documentation of decisions that must be locked.
- Keep the execution layer (harness tooling) separate from the agent layer in code. They should be
  separate apps, with the agent using the harness, but both working independently (the harness
  with its tools can run in isolation, locally over a unix socket, or remotely over
  http/grpc/websockets).
- Start with 3 agent integrations:
  - **codex** — via the ChatGPT codex Responses backend (the user logs in with a ChatGPT
    subscription); references: can1357/oh-my-pi, earendil-works/pi, thehumanworks/tny;
  - **claude code** — via agentclientprotocol/claude-agent-acp;
  - **openai-compatible** — OpenRouter, Vercel AI Gateway, … whatever the user configures as
    providers.
- Enable easy running of the agent over SSH, with SSH shadowing all tool calls (read files, write,
  shell, …).

### Ideas and goals

- Maximum token efficiency, maximum performance — benchmark and beat openai/codex, unreal-agent,
  pi, …
- Autocompaction that is intelligent and enables long-running agents.
- Agents should be able to semantically search previous conversations when relevant.
- Agents must be able to coordinate as swarms using a blackboard approach (async message board).
  Like fiverr for agents: a lead agent submits jobs, other agents are assigned to the jobs, the
  lead agent gets notified by webhook and can poll the result. Asynchronous messaging maximises
  parallelism.
- A "dynamic" reasoning effort (can change during a task depending on how complex the task looks,
  how much effort the agent is putting in vs reward, …) powered by TypeSafe Jev, through the
  `typesafe-jev` crate.
- An auto model router, also powered by Jev.
- From the codex backend, extract dictation (exposed to the user as a `/dictate` slash command to
  use voice), image generation and web search — exposed to the whole harness.
- Login to codex (browser and device code) and to Claude.
- Support for skills (placed in `.agents/skills`), with autocompletion and suggestions in the TUI.
- Autocompletion and suggestions for files, dirs, … — extended in the future to cloud buckets,
  with completion, so they feel like filesystem interactions to the model.
- The harness uses **code mode** and lets the agent save repeatable programs (synced to GitHub)
  and search / get suggestions for them from Jev.
- The harness must be extensible and hackable — agents can tweak their system instructions,
  tools, anything available.
- The UI is extensible both as a TUI and a web chat UI, letting the user write plugins that react
  to events, create themes and tweak design; the agent can too, modifying the UI to convey
  information. Take inspiration from pi; use WebAssembly for this, allowing all languages that can
  compile to it to be used to write extensions and UI/TUI tweaks at runtime.
- All sessions persist by default, running in a daemon with state persisted in a SQLite database.
- The memory system is intuitive, file based, and either git- or SQLite-backed.
- First-class support for defining agents as markdown files with YAML frontmatter, MCP, and
  scripting with programmable workflows.
- Expose TUI, headless CLI and web UI modes. TUI and CLI have an `--ephemeral` option and the chat
  UI a "private" mode, where the session is not stored.
- Focus a lot on recursive self-improvement, agent collaboration, continuous optimisation and
  modification of the harness by the agents.

Design with intent: decoupled, composable and verified code. It should be fast, beautiful and
lightweight.

## Clarifications from the maintainer (2026-09-24/25)

- **Tooling** is installed through [mise](https://mise.jdx.dev) and pinned in `mise.toml`.
- **tny** (the maintainer's earlier C11 harness) "works great but is unstable and is not doing a lot
  of the dynamic behavior, plugin architecture, verification, hackability and composability" that
  aim will. aim is a fresh Rust design: tny's ADRs are mined for proven decisions and pitfalls
  (see [`research/tny-lessons.md`](research/tny-lessons.md)), not ported.
- **Build order:** self-hosting first — foundations → standalone harness → agent daemon with the
  three integrations and a headless CLI (exit criterion: aim can work on its own repository) →
  TUI → everything else, increasingly built by aim itself.
- **Verification:** "prefer formal verification over tests by example." Pure decision logic lives in
  Verus-verified kernels; example-based tests are reserved for what proofs cannot reach.
- **Delivery:** work is committed and pushed to the private repository `thehumanworks/aim`.
