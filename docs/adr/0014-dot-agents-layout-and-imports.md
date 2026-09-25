# ADR 0014: Use .agents as the native project layout

- Status: Accepted
- Date: 2026-09-25
- Baseline: 0008, 0009
- Scope: Project and user resource discovery, imports and activation trust.

## Context

`.agents/skills` is already read by Codex and pi, while other harnesses use different
homes and parser rules (research/agents-conventions.md, TL;DR and §§1–4, lines 5–98).
MCP and hooks can launch executables, so merely discovering a foreign file cannot imply
permission to execute it (research/agents-conventions.md, §7 and TL;DR, lines 139–181,
5–20). SSH makes the source workspace itself remote (docs/architecture.md §6.8).

## Decision

- Project resources live under `.agents/`: `instructions.md`, `rules/*.md`,
  `skills/<name>/SKILL.md`, `agents/<name>.md` with `aim.agent/v1` YAML frontmatter,
  `prompts/*.md`, `workflows/*/`, `programs/`, `plugins/`, `mcp.json`, `hooks.toml`,
  and optional `memory/`. Portable `AGENTS.md` is walked root to cwd under a byte cap.
- User resources live in `~/.aim/`: `config.toml`, `agents/`, `skills/`, `prompts/`,
  `plugins/`, `programs/`, `memory/`, `mcp.json` and `hooks.toml`. Keep resource types
  distinct because their authority, lifecycle and context costs differ.
- Read project resources through `Workspace`; under SSH, load the remote project's files
  and label provenance remote. Do not accidentally apply local ancestor instructions.
- Parse foreign Claude, Codex, pi, oh-my-pi, tny and OpenCode formats as read-only typed
  descriptors carrying `{kind, path, scope, parser_version, trust, hash}`. Report
  collisions and unsupported fields. Discovery itself executes nothing.
- Require per-source opt-in before activating imported MCP servers or executable hooks,
  following tny ADR 0052 (research/agents-conventions.md, §7, lines 139–181).
- Catalog skills by metadata within a small context budget and load full bodies only on
  activation. An explicit skill mention injects the body into the user turn, preserving
  the cached system prefix (research/agents-conventions.md, §1, lines 23–38; tny ADR 0056).
- Canonical memory is Markdown with a short index and on-demand topic files; retrieved
  memory may advise but never grant permissions.

## Consequences

One native layout supports project portability and SSH-consistent instructions. Foreign
imports require maintained parsers and visible provenance. Executable imports require
an opt-in even when adjacent read-only material was accepted.

## Verification

- M4 tests `remote_project_resources`, `foreign_import_provenance`,
  `imported_mcp_requires_opt_in` and `skill_mention_user_turn` are to be added.
- A live SSH conformance case checks remote `AGENTS.md` and `.agents/` selection while
  leaving local project resources unused.
