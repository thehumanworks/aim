# ADR 0033: Discover resources into one bounded catalog and activate skills in the user turn

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0014, 0021, 0022, 0031
- Scope: how a session discovers, ranks and uses project and user resources (instructions, rules, skills, agent definitions, prompt templates, memory indexes): the `aim.agent/v1` fields, the `resources::Files` trait, precedence, budgets, mention injection and agent tool allowlists. Not MCP servers, hooks, plugins or workflows (their opt-in is ADR 0014's), not memory writes or retrieval, and not how UIs show the catalog.
- Amended by: [0038](0038-session-authority-effort-source-and-config-outcomes.md)

## Context

ADR 0014 fixed the layout (`.agents/`, `~/.aim/`, foreign formats read-only, discovery executes nothing, remote projects read through the workspace) but left the mechanics open: which files, in which order, how much of the context they may use, and how a named agent narrows a session.

The evidence (research/agents-conventions.md):
- Codex budgets the skill catalog to 2% of the context window (8,000 characters when unknown); Claude to 1%. Both shrink descriptions before dropping skills (§1).
- Codex walks `AGENTS.md` from the repository root to the working directory under a 32 KiB combined cap; pi does the same with `CLAUDE.md` as a per-directory fallback (§4).
- Claude loads the first 200 lines or 25 KiB of `MEMORY.md` and reads topic files on demand (§5).
- tny ADR 0056 injects an explicitly mentioned skill into the user turn, not the system prompt, so the provider's prompt cache survives (§1).
- Prompt templates have three argument grammars (§6). Claude's `$N` is **0-based**, arguments split shell-style, and unused arguments are appended as `ARGUMENTS: …` (code.claude.com/docs/en/slash-commands, checked 2026-09-25).

`yaml-rust2`'s loader (0.13.0, `src/yaml.rs`, `Event::Alias`) expands an alias by cloning the anchored subtree. A hostile `SKILL.md` could therefore exhaust memory ("billion laughs"), and release builds abort on panic.

## Decision

**Catalog.** Discovery (`aim::resources::discover`) runs once per session start and builds a `Catalog` of typed resources. Each carries a `Descriptor { kind, name, description, path, scope, source, trust, location, hash, parser_version }`. Problems are `Diagnostic`s (invalid, unsupported, collision, truncated, limit, not importable, unreadable), never errors. Nothing is executed and nothing is reloaded mid-session.

**Where files come from.** Project files are read through the `resources::Files` trait:
- `HarnessFiles` uses `fs.list` with `include_hidden` and ignores `.gitignore`, then `fs.read_many` in batches of 32. Under `--ssh` the remote project applies and every descriptor is labelled `ssh:<destination>`.
- `LocalFiles` reads `~/.aim` on this machine.
- `MemoryFiles` serves tests and scripted sessions.

`host::Connected.project` is now `Option<Arc<dyn Files>>`.

| Scope / source | Files |
| --- | --- |
| project, aim | `AGENTS.md` per directory root→cwd, `.agents/instructions.md`, `.agents/rules/*.md`, `.agents/skills/<n>/SKILL.md`, `.agents/agents/*.md`, `.agents/prompts/*.md`, `.agents/memory/MEMORY.md` |
| user, aim | `~/.aim/skills/<n>/SKILL.md`, `~/.aim/agents/*.md`, `~/.aim/prompts/*.md`, `~/.aim/memory/MEMORY.md` |
| project, Claude Code | `CLAUDE.md` (in a directory without `AGENTS.md`), `.claude/skills`, `.claude/agents/*.md`, `.claude/commands/[<ns>/]*.md` (addressed `ns:name`), `.claude/rules/*.md` |
| project, Codex / pi / oh-my-pi | `.codex/agents/*.toml`; `.pi/skills`, `.pi/prompts/*.md`; `.omp/skills` |

**Bounds** (`resources::Bounds`, per scope): 256 files, 64 KiB per file (the rest is reported truncated), 4 MiB in total, 256 entries per directory, 10 s.

**Frontmatter** is parsed from `yaml-rust2`'s event stream by aim's own builder:
- anchors and aliases are refused;
- at most 16 KiB, 8 levels and 1,024 nodes;
- scalars stay strings (`yes` is not a boolean);
- duplicate keys are refused.

**Precedence.** For one kind and name the nearest source wins: native project, then native user, then foreign project in the order Claude, Codex, pi, oh-my-pi. A shadowed resource is a collision diagnostic unless its content is byte-identical (e.g. a symlinked tree). A skill is addressed by its directory name; an aim agent by its file name; a Claude or Codex agent by its `name`.

**`aim.agent/v1`** is YAML frontmatter plus a Markdown body:
- `schema: aim.agent/v1` is required; a missing or other schema skips the file;
- `name` and `description`;
- optional `provider`, `model`, `effort` and `tools`, a list of tool names (a YAML list or a comma- or space-separated string);
- the body is the agent's instructions.

`SessionSpec.agent` selects a definition. An unknown name is `not_found`; an unimportable one is `invalid_params`. The definition then applies as follows:
- Its `model` and `effort` are defaults. Explicit session values win, and they apply only when the session's provider is the agent's (a model id belongs to one catalog).
- Its `tools` are enforced by `AllowedTools`: other tools are not offered, and a call to one anyway is refused with `denied` before it reaches the harness, which the loop turns into a failed tool result.
- Its body follows the other instructions.
- The agent's name joins the prompt-cache key (`aim:<root>:agent:<name>`).

**Imports apply only when exact.**
- A Claude agent is refused (listed "not importable", with the reason) when:
  - its `tools` name anything outside aimx's Claude-shaped set (`Read Write Edit LS Glob Grep Bash BashOutput KillShell`);
  - it declares `hooks`;
  - it uses `permissionMode: plan`.
- For a Claude agent, `model` (a Claude alias), `mcpServers` and the other fields are ignored with a diagnostic, and `disallowedTools` always maps exactly.
- A Codex role with `sandbox_mode = "read-only"` is refused. Its `model` implies provider `codex`.
- Fields that would start code (hooks, MCP servers) are never activated by discovery (ADR 0014).

**The instruction prefix**, fixed at session start, follows aim's system prompt in this order:
1. Project instructions: the `AGENTS.md` walk, `.agents/instructions.md` and rules without `paths:`, sharing one 32 KiB cap filled outermost first. Every cut and omission is marked with the file to read.
2. A rules index (`name`, `paths:` globs, description, file). Bodies are read on demand by the model rather than auto-injected. Rules govern files being changed, which prompts rarely name, so prompt matching would miss most uses and change the user turn. Matching at the tool boundary belongs to the dispatcher (architecture §6.3).
3. The skill catalog: name, description, and a workspace path (user skills say they arrive by `$name`).
   - Budget: 2% of the model's context window at 4 bytes per token, clamped to 2–32 KiB. It is 4 KiB when the window is unknown; the provider's catalog is asked once, bounded to 2 s.
   - When over budget, descriptions shrink to a common length (at least 48 bytes) before skills are dropped and counted.
   - A Claude skill marked `disable-model-invocation` is not listed.
4. Memory: the first 80 lines, at most 6 KiB, of each `MEMORY.md`, under a note that memory advises and never grants a permission.
5. The agent's instructions.

**Explicit activation.** A prompt mentions a skill with `$name` or by starting with `/skill name`. The `$name` matcher is tny ADR 0056's: the token starts the text or follows whitespace, and ends at the end, whitespace, or punctuation other than `/ - _`; a `.` ends a name only before whitespace or the end.
- Each mentioned skill is injected once per prompt, as `<skill name path>…</skill>`, just before the first part that mentions it: after aim's environment block and before the user's unchanged text.
- A skill is injected on every mention, so a mention after compaction still delivers it.
- Unknown mentions (`$HOME`) change nothing.
- `resources::backend::WithSkills` wraps the native loop to do this. Steering is not expanded.

**Prompt templates** keep their source's grammar:
- aim and Codex: `$ARGUMENTS`, `$1…`, `$$`;
- Claude: `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N` from zero, named `arguments`, `ARGUMENTS:` appended when unused;
- pi: `$@`, `${N:-default}`, `${@:N[:L]}`.

`Catalog::expand_prompt(name, args)` serves UIs. Claude's `` !`cmd` `` insertions stay text.

## Consequences

- One catalog serves the loop now and the TUI's completion and slash commands later (`Catalog::skills`, `Catalog::prompts`).
- The prefix stays byte-stable within a session. Skill bodies cost context only when mentioned or read.
- An agent's allowlist is a narrowing ceiling, enforced in the agent layer before aimx.
- Maintained parsers: one per foreign format (each with a `parser_version`), and the Claude tool-name table.
- Known gaps, left to their owners:
  - A resumed session does not know its agent: `SessionMeta` does not record it, and `live_or_resume` passes `agent: None`.
  - An agent's `provider` cannot switch the session's provider, because `SessionSpec.provider` is required.
  - User-scope skill files and memory topic files are outside the workspace, so the workspace tools cannot read them. Mentions still deliver user skills.
  - Diagnostics reach only the log (`tracing`) until UIs show them.

## Verification

- `crates/aim/tests/resources.rs`:
  - ADR 0014's `foreign_import_provenance`, `remote_project_resources` (a fake harness peer; two read round trips), `imported_mcp_requires_opt_in` and `skill_mention_user_turn`;
  - `remote_sessions_never_read_the_local_project` and `real_aimx_serves_project_resources` (the real aimx over stdio; a symlink out of the root is refused);
  - the budgets and bounds;
  - `agent_allowlist_refuses_calls`.
- Unit tests in `crates/aim/src/resources/*` cover frontmatter bounds, the three prompt grammars, the mention matcher and the catalog fitter (every budget respected).
- Live (OpenRouter, `anthropic/claude-sonnet-5`, 2026-09-25):
  - `live_skill_mention_via_aim_run_openrouter`: `aim run -p openrouter --ephemeral --json '$haiku Describe the sea.'` recorded the skill block in the user turn and answered with a three-line haiku, in 3.3 s, 2,866 input and 37 output tokens, $0.0075. The instruction prefix was 1,847 bytes.
  - `live_read_only_agent_on_openrouter`: an agent allowing only `Read` read the file, answered, declined to create one, and none was created, in 4.9 s. Prefix 1,935 bytes.
