# `.agents/` conventions and cross-harness imports

Research date: 2026-09-25. `refs/<repo>/...:line` citations point into the supplied read-only reference snapshots. Official documentation links describe current published behavior; local inventory is a structural observation on this machine, with no credential values or private instruction bodies copied. Formats are versioned inputs, not one universal schema.

## TL;DR

- **FACT:** The open Agent Skills format is a `SKILL.md` with required `name` and `description`, plus optional metadata and on-demand resources. [Agent Skills specification](https://agentskills.io/specification).
- **FACT:** Codex and pi discover `.agents/skills`; Claude Code's native project root is `.claude/skills`. Codex's current skill documentation does not name `.codex/skills` as a native location. [Codex skills](https://learn.chatgpt.com/docs/build-skills); `refs/pi/packages/coding-agent/src/core/package-manager.ts:2415-2530`.
- **FACT:** Skill catalog injection is metadata-first. Codex budgets the catalog to 2% of context or 8,000 characters when context is unknown; Claude budgets descriptions to 1% of context; full skill instructions load on activation. [Codex skills](https://learn.chatgpt.com/docs/build-skills); [Claude skills](https://code.claude.com/docs/en/skills).
- **FACT:** Claude agent files are Markdown/YAML in `.claude/agents`; current Codex standalone agents are TOML in `.codex/agents`, with legacy `[agents.<name>]` config roles as another path. [Claude subagents](https://code.claude.com/docs/en/sub-agents); [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents); `refs/codex/codex-rs/core/src/config/config_tests.rs:8645-8688`.
- **FACT:** Current Claude Code can read `AGENTS.md`, but its default file selection differs from Codex/pi. Codex walks root-to-CWD with a 32-KiB combined instruction cap; Claude supports `@path` imports to depth four. [Claude memory](https://code.claude.com/docs/en/memory); [Codex AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md).
- **FACT:** Claude auto memory uses a `MEMORY.md` index with on-demand topic files, loading only the first 200 lines or 25 KiB at startup. Codex uses `memory_summary.md`, `MEMORY.md`, and rollout summaries; tny uses a small explicit JSON map. [Claude memory](https://code.claude.com/docs/en/memory); `refs/codex/codex-rs/memories/README.md:8-15,79-85`; `refs/tny/src/core/tools_ext.c:20-77`.
- **FACT:** Claude MCP configuration in `.mcp.json` can launch commands. Its project file requires trust in interactive sessions, and duplicate entries use local > project > user scope. Codex uses `[mcp_servers.<id>]` in TOML. [Claude MCP](https://code.claude.com/docs/en/mcp); [Codex config reference](https://learn.chatgpt.com/docs/config-file/config-reference).
- **FACT:** The user's tny ADR 0052 deliberately makes foreign MCP imports opt-in because they can launch executables and contain credentials. `refs/tny/docs/adr/0052-mcp-import-from-harnesses.md:6-33,63-76`.
- **FACT:** Claude/ Codex custom prompt templates and pi prompt templates use different argument substitution grammars; Codex custom prompts are deprecated in favor of skills. [Codex custom prompts](https://learn.chatgpt.com/docs/custom-prompts); `refs/pi/packages/coding-agent/docs/prompt-templates.md:1-59`.
- **FACT:** Claude and Codex expose configurable lifecycle hooks; pi uses TypeScript extension events; OMP maps hook files into its extension bus. [Claude hooks](https://code.claude.com/docs/en/hooks); [Codex hooks](https://learn.chatgpt.com/docs/hooks); `refs/pi/packages/coding-agent/docs/extensions.md:54-115`; `refs/oh-my-pi/docs/hooks.md:5-14,64-69`.
- **RECOMMENDATION:** Make `.agents/` aim's native project home, but treat other harness files as read-only imported sources with provenance, trust state, versioned parser and collision diagnostics.
- **RECOMMENDATION:** Keep instruction files, skills, agent definitions, prompts, memory and MCP separate. They have different lifecycle, authority and prompt costs.
- **RECOMMENDATION:** Support `.agents/skills` natively, and define aim agents in `.agents/agents/*.md`; import Claude Markdown and Codex/OpenCode TOML/Markdown into typed internal descriptors.

## Findings

### 1. Open Agent Skills format and discovery

- **FACT:** A skill is a directory containing `SKILL.md`. Its YAML frontmatter requires `name` (1–64 characters, lowercase letters/numbers/hyphens, no leading/trailing or repeated hyphen, matching directory name) and `description` (1–1,024 characters). [Agent Skills specification](https://agentskills.io/specification).
- **FACT:** Optional standard fields are `license`, `compatibility` (at most 500 characters), `metadata` (string-to-string mapping), and experimental `allowed-tools` (space-separated). Unknown tool-specific fields are not part of the interoperable core. [Agent Skills specification](https://agentskills.io/specification).
- **FACT:** Optional `scripts/`, `references/`, and `assets/` keep executable helpers, documentation and output materials out of the front page. The spec recommends a short metadata-only discovery phase, then SKILL.md, then referenced resources as needed; it recommends a skill body below 5,000 tokens and 500 lines and references one level deep. [Agent Skills specification](https://agentskills.io/specification).
- **FACT:** Codex scans `.agents/skills` from CWD to repo root, user `~/.agents/skills`, admin `/etc/codex/skills`, and built-in/system locations. Duplicate names can both appear rather than merge. The published local discovery table does not include `.codex/skills`. [Codex skills](https://learn.chatgpt.com/docs/build-skills).
- **FACT:** Codex inserts name, description and path into the initial list. A `$skill` mention or `/skills` chooses explicitly; description matching can activate implicitly. The list budget is 2% context or 8,000 characters with unknown context; descriptions shrink before entire skills are omitted with warning. [Codex skills](https://learn.chatgpt.com/docs/build-skills).
- **FACT:** Codex's optional `agents/openai.yaml` carries appearance, tool dependencies and invocation policy; `allow_implicit_invocation:false` still allows explicit mention. This is a Codex extension, not an Agent Skills standard field. [Codex skills](https://learn.chatgpt.com/docs/build-skills).
- **FACT:** Claude's native project location is `.claude/skills`; user `~/.claude/skills`, managed and plugin skills also load, with nested project skills relevant when Claude enters their directory. Plugin skills are namespaced. Claude's extended frontmatter includes `when_to_use`, `argument-hint`, arguments, invocation flags, tool allow/deny, model, effort, context/agent/background, hooks, paths and shell. [Claude skills](https://code.claude.com/docs/en/skills).
- **FACT:** Claude's collision order is managed > personal > project, with skill taking precedence over a same-named legacy command. Its catalog budget is 1% of context for descriptions; names remain even if descriptions truncate. [Claude skills](https://code.claude.com/docs/en/skills).
- **FACT:** pi discovers project `.pi/skills` and ancestor `.agents/skills` to git root, user `~/.pi/agent/skills` and `~/.agents/skills`; project loading has a trust boundary. Its skills catalog is XML in the system prompt; the model reads full files on demand and `/skill:name` is explicit. `refs/pi/packages/coding-agent/src/core/package-manager.ts:463-481,2415-2530`; `refs/pi/packages/coding-agent/src/core/skills.ts:347-380`.
- **FACT:** tny scans `skills/`, `.agents/skills`, `.claude/skills`, `.codex/skills`, `.cursor/skills`, `.opencode/skills` upwards, nearest first and de-duplicated by name, then home roots. It advertises metadata and offers a native `skill` tool; explicit `$name` or `/name` pre-injects the selected body into the user turn. `refs/tny/src/core/skills.c:14-20,75-160`; `refs/tny/docs/adr/0056-skill-mention-injection.md:8-31`.
- **FACT:** tny's mention injection avoids an extra skill-tool round trip and avoids changing a cached system prefix mid-session; it bounds injected content and re-injects after compaction if needed. `refs/tny/docs/adr/0056-skill-mention-injection.md:33-84`.
- **FACT:** OMP discovers native `.omp` skills first, then plugins and foreign skills including Claude, `.agents`/Codex, OpenCode and GitHub; same-name first winner and realpath dedup can hide a foreign candidate. Its fields include `globs`, `alwaysApply`, `hide`, `disableModelInvocation`. `refs/oh-my-pi/docs/skills.md:50-98,118-125`.
- **RECOMMENDATION:** Parse the standard core exactly; retain extension fields in a namespaced raw map. Warn on duplicate names and show source path in the TUI rather than silently choosing one. Explicit references should bind a specific source-qualified skill.

### 2. Local skill inventory (structural only)

- **FACT (local read-only inspection, 2026-09-25):** `~/.claude/skills` exists and contained 27 `SKILL.md` files under its tree; sample frontmatter keys were `name`, `description`, and sometimes `license`. `~/.codex/skills` existed with a `.system` child; `~/.agents` existed but no `SKILL.md` was found there.
- **FACT (local read-only inspection):** Among six sampled sibling project directories with agent-related files, `.agents/skills` appeared in two and `.claude/skills` in three; sample skill frontmatter keys were `name` and `description`. No body text or private setting values were copied.
- **FACT (local read-only inspection):** `~/.claude/settings.json` exposed top-level `hooks` and `permissions` keys; `~/.codex/config.toml` exposed `hooks`, `mcp_servers`, `memories`, `skills` and other top-level sections. One Codex MCP server entry existed; its name, endpoint, command and credentials were not inspected or printed.

### 3. Agent definitions and delegated roles

- **FACT:** Claude project agents live in `.claude/agents/*.md`; user agents in `~/.claude/agents/*.md`. `name` and `description` are required frontmatter. Optional fields include `tools`, `disallowedTools`, `model`, `permissionMode`, `maxTurns`, `skills`, `mcpServers`, `hooks`, `memory`, `background`, `omitClaudeMd`, `effort`, `isolation`, `color`, and `initialPrompt`; body is the agent's instructions. [Claude subagents](https://code.claude.com/docs/en/sub-agents).
- **FACT:** Claude priority is managed > CLI `--agents` > project > user > plugin, with nearer project definitions winning. `memory` can be `user`, `project` or `local`; `isolation: worktree` selects worktree isolation; `skills` preloads full skill bodies into that agent. Unknown frontmatter keys are ignored. [Claude subagents](https://code.claude.com/docs/en/sub-agents).
- **FACT:** Claude warns when combined subagent descriptions exceed about 15,000 tokens. Discovery metadata therefore has a real context cost even before an agent starts. [Claude subagents](https://code.claude.com/docs/en/sub-agents).
- **FACT:** Current Codex standalone role files live in `.codex/agents/*.toml` or `~/.codex/agents/*.toml`; documented required fields are `name`, `description`, `developer_instructions`. They may set normal config values including `model`, `model_reasoning_effort`, `sandbox_mode`, `mcp_servers` and `skills.config`. Built-in roles include default, worker and explorer. [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents).
- **FACT:** Codex also accepts legacy/custom role declarations under `[agents.<name>]` with `description` and `config_file`; relative config paths resolve from the declaring config file. `refs/codex/codex-rs/core/src/config/config_tests.rs:8645-8688`; [Codex config reference](https://learn.chatgpt.com/docs/config-file/config-reference).
- **FACT:** The supplied pi base does not define a universal Markdown subagent directory in its coding-agent documentation; delegation is extensible through the extension API. This is a scoped repository observation, not proof that no pi package has one. `refs/pi/packages/coding-agent/docs/extensions.md:54-115`.
- **FACT:** OMP's native `.omp/agents` and `~/.omp/agent/agents` normalize agent definitions with name, description and system prompt plus optional tools, spawn policy, model priorities, thinking level, output schema and skill autoload. It intentionally skips direct `.claude/agents` and `.codex/agents` because schemas diverge, while importing Claude plugin agents. `refs/oh-my-pi/docs/task-agent-discovery.md:25-47,138-169`.
- **FACT:** tny task presets use `--task` with `.tny/tasks/*.md` and `~/.tny/tasks/*.md`; only name/description frontmatter is needed, with 256-KiB/256-definition bounds and a saved body/digest snapshot for reproducibility. `refs/tny/docs/adr/0048-runtime-task-presets.md:6-35`.
- **FACT:** tny ADR 0136 is *session/dashboard* discovery across repository worktrees; it is not another Markdown agent-definition format. `refs/tny/docs/adr/0136-repository-scoped-agent-discovery.md:16-33`.
- **FACT:** OpenCode V2 agents use `.opencode/agents/<name>.md` and `~/.config/opencode/agents/<name>.md`, YAML frontmatter, Markdown instructions, and fields such as `description`, `mode`, `model`, ordered `permissions`, `steps`, `hidden`, `color`, `disabled`, `request`. Its V1 docs use a different singular `permission`/`tools` vocabulary; imports need version detection. [OpenCode V2 agents](https://opencode.ai/v2/docs/agents); [OpenCode V1 agents](https://opencode.ai/docs/agents).
- **FACT (local read-only inspection):** A sibling project's `.claude/agents/` contained four Markdown agent files; sampled frontmatter keys were only `name`, `description`, `model`. No agent instructions were copied. User-level `~/.claude/agents` was absent.

Proposed aim Markdown superset (opinion, **RECOMMENDATION**; not an existing interoperability standard):

```yaml
---
schema: aim.agent/v1
name: reviewer
description: Review changes for correctness and maintainability.
model: { provider: auto, id: auto, fallback: [] }
effort: auto
tools: { allow: [read, grep], deny: [write, shell] }
skills: [review]
mcp_servers: [docs]
permissions: { mode: ask, filesystem: read-only, network: ask }
memory: { scope: project, read: true, write: false }
ui: { color: blue, theme: default }
output: { schema: null, format: markdown }
budget: { max_turns: 20, max_tokens: null, max_cost: null }
isolation: { mode: shared, transport: local }
---
The agent's role and task-specific instructions follow.
```

- **RECOMMENDATION:** Keep `name`/`description` and body compatible with Claude where possible; translate camelCase Claude fields and OpenCode's `permissions` into typed internal values. Preserve unrecognized fields for round-tripping but never elevate them to permissions.
- **RECOMMENDATION:** Import Codex TOML roles as read-only source descriptors. A Markdown export can record equivalent intent but cannot promise exact Codex behavior, especially sandbox and model configuration.
- **RECOMMENDATION:** Put task budgets, output schema and memory scope in aim's own namespace; validate that an imported source actually supports each field before showing an “equivalent” badge.

### 4. Repository instructions and precedence

- **FACT:** The [AGENTS.md convention](https://agents.md/) is plain Markdown with no mandatory YAML schema. Nested files scope guidance more narrowly; a direct user instruction takes priority over repository guidance.
- **FACT:** Codex loads `$CODEX_HOME/AGENTS.override.md` in preference to `$CODEX_HOME/AGENTS.md`, then one candidate per directory from repository root to CWD. In each directory it checks override, regular name and configured fallback names. More specific content appears later. Project instruction bytes have a default 32-KiB combined cap. [Codex AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md); `refs/codex/codex-rs/core/src/agents_md.rs:1-18,145-275`.
- **FACT:** Claude loads managed policy, `~/.claude/CLAUDE.md`, `CLAUDE.md` or `.claude/CLAUDE.md`, and optional `CLAUDE.local.md` in scope order. Ancestors load at launch; nested files load when files in their directories are read. Files concatenate rather than overwrite each other. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT:** Claude `@path` imports resolve relative to the containing instruction file, can recursively import to four hops, and are skipped inside code spans/fences. Project imports outside the working directory require a first-use approval; imported text still consumes context. Claude recommends under 200 lines per CLAUDE.md and skips a file over 4 MiB. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT:** Current Claude supports `AGENTS.md` directly. By default it uses AGENTS.md as a fallback only when no CLAUDE.md/CLAUDE.local.md applies above CWD; a setting can load both. It does not interpret Codex's `AGENTS.override.md` or `.agents` as equivalents. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT:** Claude `.claude/rules/*.md` supports path-specific frontmatter and loads matching instructions on file access; this is narrower than a global CLAUDE.md. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT:** pi chooses, per directory, the first of `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`; it loads user agent-dir instructions and then ancestor directories root-to-CWD. `refs/pi/packages/coding-agent/src/core/resource-loader.ts:126-145,174-209`.
- **FACT:** OMP uses native `~/.omp/agent/AGENTS.md` and nearest nonempty ancestor `.omp/AGENTS.md`, plus several foreign instruction providers with priority and byte-identical dedup. A higher-priority foreign file can shadow standalone AGENTS.md at a depth. `refs/oh-my-pi/docs/context-files.md:18-34,55-106,125-135`.
- **FACT:** pi renders every chosen context file with an explicit source path inside `<project_instructions ...>` and gives skills a separate system-prompt section. It is therefore possible to preserve provenance at the prompt boundary rather than concatenating anonymous text. `refs/pi/packages/coding-agent/src/core/system-prompt.ts:72-78,163-170`.
- **RECOMMENDATION:** aim should read AGENTS.md and optional `.agents/instructions.md` as separate layers; expose a resolved-instruction trace with file, scope, order, bytes, and import expansion. Never silently flatten conflicting foreign sources as if they shared authority.
- **RECOMMENDATION:** Match path-scoped instructions at the file/tool boundary, not only launch CWD. Re-evaluate after SSH root changes, worktree changes and file reads, while retaining the source authority order.

### 5. Memory conventions and local example

- **FACT:** Claude distinguishes user-authored CLAUDE.md instructions from auto memory. Auto memory lives under `~/.claude/projects/<project>/memory/`; `MEMORY.md` is a startup index and other topic files load on demand. Only the first 200 lines or 25 KiB of the index load at startup; individual CLAUDE.md files may be up to 4 MiB. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT:** Auto memory is machine-local and shared across a repository's worktrees. Subagents do not automatically inherit the main auto-memory corpus; an agent `memory` field gives it a separate scope. [Claude memory](https://code.claude.com/docs/en/memory).
- **FACT (local read-only inspection):** `~/.claude/projects/-Users-tomas-projects-aim/memory/` held `MEMORY.md` plus six topic Markdown files. Its index had six lines and six Markdown links; topic files were 15–23 lines. This verifies a concise index-to-topic layout on the user's machine without copying private content.
- **FACT:** Codex's memory pipeline is asynchronous and disabled for ephemeral/subagent sessions; it extracts eligible rollouts into state DB, then consolidates `MEMORY.md`, `memory_summary.md`, rollout summaries and skills. `refs/codex/codex-rs/memories/README.md:8-15,29-77,79-85,110-137`.
- **FACT:** Codex's external-memory import guidance treats `memory_summary.md` as a compact global route, `MEMORY.md` as a searchable registry, and detailed imported files as on-demand evidence. It preserves project scope and source provenance. `refs/codex/codex-rs/external-agent-migration/src/memory_import.rs:14-32`.
- **FACT:** tny's `~/.tny/memories.json` is a key/string map with `get|set|list`; `set` writes atomically only when asked and refuses persistent writes in ephemeral mode. `refs/tny/src/core/tools_ext.c:20-77`.
- **FACT:** OMP's local memory backend is opt-in and injects a compact summary at session start; `MEMORY.md`, `memory_summary.md`, `learned.md` and generated skills are file-backed and accessible via `memory://root`. It also has alternative remote/SQLite backends. `refs/oh-my-pi/docs/memory.md:1-44,59-86`.
- **FACT:** No built-in cross-session memory file format was established for the supplied pi base; pi session persistence and extension APIs make custom memory possible. This is a scoped source finding, **UNVERIFIED** as a universal claim about pi packages.

Proposed aim layout and retrieval (**RECOMMENDATION**):

```text
.agents/memory/                 # optional project-shared, reviewable knowledge
  MEMORY.md                    # short routing index; links and scope tags
  decisions/<topic>.md         # stable project decisions with source/evidence
  workflows/<topic>.md         # repeatable steps; promote to skill when procedural
~/.aim/memory/                 # private per-user state; never automatically commit
  memory_summary.md           # tiny startup route
  MEMORY.md                   # searchable registry
  projects/<repo-id>/*.md      # scoped facts and provenance
  rollouts/<session-id>.md     # evidence derived from durable sessions
  index.sqlite                # optional FTS/vector/index metadata, not canonical prose
```

- **RECOMMENDATION:** Treat Markdown as canonical and SQLite/git as indexes/history backends. Record `scope`, `source`, `observed_at`, `confidence`, `supersedes`, and evidence links for each entry; project memory should have an explicit sharing/commit policy.
- **RECOMMENDATION:** Load only the summary/index at startup. Retrieve relevant topic files through exact path/keyword and semantic search; use Jev to rank relevance, then verify mutable facts against live source before acting.
- **RECOMMENDATION:** Separate user instructions from learned observations. A memory can suggest a search path, but cannot grant permissions, silently start MCP servers or override active user/project instructions.
- **RECOMMENDATION:** Honor `--ephemeral`/private mode by suppressing all session and memory writes. Use atomic file replacement and a lease around consolidation to avoid races across the daemon's sessions.

### 6. Slash commands and prompt templates

- **FACT:** Claude's older `.claude/commands/*.md` format is now treated as a skill-like command; skills are the recommended newer format. Command/skill arguments support `$ARGUMENTS`, indexed `$ARGUMENTS[N]` and `$N` (0-based), while shell insertion `!` followed by a backticked command is a Claude-specific dynamic context feature. [Claude slash commands](https://code.claude.com/docs/en/slash-commands).
- **FACT:** Codex custom prompts in `~/.codex/prompts/*.md` are documented as deprecated in favor of skills. Frontmatter can give `description` and `argument-hint`; invocation is `/prompts:name`. Substitution supports `$1` through `$9`, `$ARGUMENTS`, named `$FILE` from `FILE=value`, and `$$` escape. The documented location is user top-level; a project `.codex/prompts` is not established as native. [Codex custom prompts](https://learn.chatgpt.com/docs/custom-prompts).
- **FACT:** pi Markdown prompt templates live in `~/.pi/agent/prompts/` and `.pi/prompts/`, with optional `description` and `argument-hint`; quote-aware arguments support `$1`, `$@`/`$ARGUMENTS`, `${1:-default}` and `${@:N[:L]}`. Project prompts require trust. `refs/pi/packages/coding-agent/docs/prompt-templates.md:1-59`.
- **FACT:** OMP has native `.omp/commands`, plugins, Claude commands, `.agents/.agent/commands`, `.codex/commands` and OpenCode imports under an ordered collision policy. Its `.codex/commands` support is OMP-specific and should not be mistaken for Codex's `/prompts:` discovery path. `refs/oh-my-pi/docs/slash-command-internals.md:26-121,182-225`.
- **FACT (local read-only inspection):** No Markdown prompt files were found under the inspected user `~/.claude/commands`, `~/.codex/prompts` or pi prompt paths. This says nothing about installed plugins or remote commands.
- **RECOMMENDATION:** Normalize a prompt as `{name, description, body, source, argument_dialect}`. Keep each source's substitution grammar intact; show a preview before running shell-expanding legacy templates. Prefer skills for new multi-step workflows.

### 7. MCP configuration and import trust

- **FACT:** Claude project `.mcp.json` is `{ "mcpServers": { "name": { "type":"http", "url":"..." } } }` or stdio entries with `command`, `args`, `env`. Local/user entries live in `~/.claude.json`; duplicate names use local > project > user, with managed servers above all. Project `.mcp.json` is interactive-approval gated. [Claude MCP](https://code.claude.com/docs/en/mcp).
- **FACT:** Claude expands `${VAR}` and `${VAR:-default}` in command, args, env, URL and headers, but has special treatment for sensitive credential variables. It supports stdio, streamable HTTP and WebSocket configuration, with transport-specific auth behavior. [Claude MCP](https://code.claude.com/docs/en/mcp).
- **FACT:** Codex uses `$CODEX_HOME/config.toml` (usually `~/.codex/config.toml`) and project config with `[mcp_servers.<id>]` tables. Stdio fields include `command`, `args`, `cwd`, `env`, `env_vars`; HTTP uses `url`, `http_headers`, `env_http_headers` or bearer-token env name. Per-server enable, tool filters, timeouts, OAuth and approvals are documented. [Codex config reference](https://learn.chatgpt.com/docs/config-file/config-reference).
- **FACT:** tny owns `~/.tny/mcp.json` shape `{ "servers": { "name": { "command": ["..."] } } }`; ADR 0052 allows optional import of Codex, Claude, grok and Cursor configs, native tny winning collisions and source attribution in listings. It does not write foreign config. `refs/tny/docs/adr/0052-mcp-import-from-harnesses.md:18-52,63-76`.
- **FACT:** OMP's native paths are `.omp/mcp.json` and `~/.omp/agent/mcp.json` (profile-aware), with `{mcpServers,disabledServers,enabledServers}`. It imports many foreign sources including Claude, Codex, Gemini, OpenCode, Cursor and VS Code, with explicit source order. `refs/oh-my-pi/docs/mcp-config.md:13-53,70-108`.
- **FACT:** A fixed native pi MCP config format was not found in the supplied base docs; MCP can be integrated through extensions. **UNVERIFIED:** Which third-party pi MCP package, if any, a given user has installed.
- **FACT:** `claude-agent-acp` adapts Claude's agent session; it is not a new repository MCP config standard. Any MCP exposure remains governed by Claude/SDK configuration. **UNVERIFIED:** Exact adapter override precedence was not traced in this pass.

Representative source shapes (values are deliberately inert; these are parser fixtures, **not** executable recommendations):

```json
// Claude project .mcp.json; remove this comment for valid JSON
{"mcpServers":{"docs":{"type":"http","url":"https://example.invalid/mcp"}}}
```

```toml
# Codex config.toml
[mcp_servers.docs]
url = "https://example.invalid/mcp"
enabled = true
```

```json
// tny ~/.tny/mcp.json; remove this comment for valid JSON
{"servers":{"local-docs":{"command":["/path/to/server"]}}}
```

These are different top-level maps (`mcpServers`, `[mcp_servers]`, `servers`) and cannot be merged by generic JSON/TOML key union. Claude and Codex support HTTP whereas tny's cited import decision only ran stdio and listed remote entries as skipped. [Claude MCP](https://code.claude.com/docs/en/mcp); [Codex config reference](https://learn.chatgpt.com/docs/config-file/config-reference); `refs/tny/docs/adr/0052-mcp-import-from-harnesses.md:42-52`.

Proposed aim import order (**RECOMMENDATION**):

1. Managed policy, if an organization supplies one; fail closed if it conflicts.
2. Explicit current-session MCP selection or denial.
3. Native project `.agents/mcp.json`, then native user `~/.aim/mcp.json`.
4. Explicitly enabled foreign project configs: Claude `.mcp.json`, Codex `.codex/config.toml`, OMP `.omp/mcp.json`, tny only where its native user config applies.
5. Explicitly enabled foreign user configs: `~/.claude.json`, `$CODEX_HOME/config.toml`, `~/.omp/agent/mcp.json`, `~/.tny/mcp.json`.

- **RECOMMENDATION:** Import foreign MCP only after an opt-in per source, following the user's tny ADR. Show name, transport, command basename/host, source file and permission class before the first launch; never print env or auth values. Project trust and SSH execution location must be explicit.
- **RECOMMENDATION:** Collision resolution should be by exact name within a scope and then endpoint identity, with a visible diagnostic. Preserve a disabled project entry so it can suppress a lower-priority user server; do not merge fields from different definitions.
- **RECOMMENDATION:** Parse JSON/TOML as data; support stdio and streamable HTTP natively, and keep an unknown transport disabled with a reason. Give each MCP tool a stable `mcp:<server>/<tool>` permission identity as tny does.

### 8. Hooks and extension event vocabulary

- **FACT:** Claude hooks live in settings JSON under `hooks.<Event>` arrays with matcher groups and handlers; handler types include `command`, `http`, `mcp_tool`, `prompt`, and `agent`. Hook input is JSON; event-specific decisions and exit code 2 can block selected actions. [Claude hooks](https://code.claude.com/docs/en/hooks).
- **FACT:** Claude's event list includes session start/end, user prompt submit, pre/post tool use and tool failure, permission request, subagent lifecycle, compaction, stop/interrupt and other lifecycle/UI events. [Claude hooks](https://code.claude.com/docs/en/hooks).
- **FACT:** Codex documents SessionStart/End, PreToolUse, PermissionRequest, PostToolUse, Pre/PostCompact, UserPromptSubmit, SubagentStart/Stop, Stop and Interrupt. Configuration can be in `hooks.json` or config TOML; current runnable handler type is command, while other types may parse but are skipped. [Codex hooks](https://learn.chatgpt.com/docs/hooks).
- **FACT:** pi's TypeScript `pi.on(event, handler)` exposes session, input, pre-agent, tool call/result, turn, settlement and shutdown events; extensions can register tools/commands and alter prompts. `refs/pi/packages/coding-agent/docs/extensions.md:54-115`.
- **FACT:** OMP no longer treats its legacy hook runner as the default; `.omp/hooks/pre|post/*.{ts,js}` factories use the extension bus. `refs/oh-my-pi/docs/hooks.md:5-14,64-69,91-125`.
- **RECOMMENDATION:** aim's minimal typed event vocabulary: `session.start|end`, `prompt.submit`, `agent.before_turn|after_turn|settled`, `tool.before|after|failed`, `permission.request`, `subagent.start|stop`, `compact.before|after`, `run.stop|interrupt`. Include IDs, source, cwd/remote root and redacted args; event handlers get explicit blocking vs observational capability.
- **RECOMMENDATION:** Imported hooks are executable code and should default off. Do not auto-run a foreign hook merely because its config was discovered. Tie enablement to project trust, sandbox/SSH placement and an event/permission adapter; preserve original error/timeout semantics where practical.

Suggested event adapter record (**RECOMMENDATION**):

```yaml
event: tool.before
session_id: <opaque>
turn_id: <opaque>
tool_id: mcp:docs/search
execution_location: local # or ssh:<profile>
source: .agents/hooks.toml
blocking: true
input_ref: <redacted-argument-handle>
deadline_ms: 10000
```

- **RECOMMENDATION:** A pre-tool handler can return `allow`, `deny` or `ask`; observers can annotate results but cannot silently authorize a denied call. Keep source-native hook outputs as opaque diagnostics and translate only stable decision fields.
- **RECOMMENDATION:** Hooks that spawn commands must execute in the same declared location as the tool they govern, or in a separately authorized controller location. This matters when aim shadows file and shell calls over SSH.

## Implications for aim

### Native directories (opinion)

```text
repo/
  AGENTS.md                         # portable repo-wide instructions
  .agents/
    instructions.md                 # aim-only scoped instructions if desired
    rules/*.md                      # path-qualified rules
    skills/<name>/SKILL.md          # open Agent Skills format
    agents/<name>.md                # aim YAML-frontmatter agent definitions
    prompts/<name>.md               # aim-native prompt templates
    mcp.json                        # native MCP definitions; explicit trust
    hooks.toml                      # native event subscriptions; explicit trust
    memory/MEMORY.md                # optional committed project knowledge index
    memory/decisions/*.md
    workflows/*.md                  # scripts/program manifests; later code mode
~/.aim/
  config.toml                       # preferences/import-source toggles
  agents/<name>.md
  skills/<name>/SKILL.md
  prompts/<name>.md
  mcp.json
  hooks.toml
  memory/memory_summary.md
  memory/MEMORY.md
  memory/projects/<repo-id>/*.md
  sessions/                         # durable session artifacts; SQLite index
```

- **RECOMMENDATION:** Native files should be independently useful: harness tools and MCP can run without an agent, and agents can be selected without loading all tools. The daemon owns durable session state; `.agents/` contains portable declarative inputs.
- **RECOMMENDATION:** Use a source registry `{kind,path,scope,parser_version,trust,mtime,hash}`. Resolve source order per artifact kind because Claude skills, Codex instructions and MCP servers have different precedence rules.
- **RECOMMENDATION:** Show discovered/imported resources in the TUI with type, source, scope, trust and collision state; explicit `@source/name` selection disambiguates same-name skills or agents.
- **RECOMMENDATION:** Verify instruction/agent/skill parsers with fixture directories for nested cwd, symlinks, collisions, malformed YAML/TOML, path traversal, imports, and mixed trust. Prove only stable invariants in Verus (bounded traversal, precedence, no unauthorized execution), keeping format extensions adaptable.

### Compatibility/import matrix (opinion; `read` means parse-only)

| Source | Skills | Agents | Instructions | Prompts | MCP | Hooks | Memory |
| --- | --- | --- | --- | --- | --- | --- | --- |
| aim `.agents` / `~/.aim` | native | native | native + AGENTS.md | native | native | native | native |
| Claude Code | read `.claude/skills` | read `.claude/agents/*.md` | read CLAUDE.md/rules; respect AGENTS selection | read legacy commands | opt-in `.mcp.json`/`~/.claude.json` | opt-in settings hooks | index/read auto memory |
| Codex | `.agents/skills` already native | read `.codex/agents/*.toml` + legacy role tables | read AGENTS.md + override | read deprecated `~/.codex/prompts` | opt-in `[mcp_servers]` | opt-in hook config | import scoped summaries |
| pi | `.agents/skills` already native; read `.pi/skills` | extension-specific, no universal parser | read pi's selected AGENTS/CLAUDE candidate | read `.pi/prompts` | extension-specific | extension adapter | session/extension-specific |
| oh-my-pi | read `.omp/skills` | read `.omp/agents` | read `.omp/AGENTS.md` and resolved foreign contexts | read `.omp/commands` | opt-in `.omp/mcp.json` | opt-in extension hook files | import local backend files |
| tny | read `~/.tny/skills` and task presets | read `.tny/tasks` as task, not agent | AGENTS/skill config by adapter | task presets | opt-in `~/.tny/mcp.json` | adapter if available | read explicit JSON map |
| OpenCode | read its skills as Agent Skills | versioned V1/V2 Markdown parser | adapter | adapter | explicit opt-in | adapter | adapter |

- **RECOMMENDATION:** “Just works” should mean discovery and safe display without executing imported code. Activating a foreign skill can load its instructions; launching a foreign MCP server or hook needs its own trust decision.
- **RECOMMENDATION:** Do not rewrite foreign configs or auto-migrate private memory. Imports should be reversible views unless the user explicitly requests export.
- **UNVERIFIED:** Exact round-trip fidelity for Claude/OpenCode custom agent fields, pi extensions, OMP hook factories, and arbitrary third-party MCP wrappers requires implementation-specific fixtures and version pinning.

## Open questions for the user

- Should aim import foreign project MCP servers and hooks only after per-project trust, or require a separate opt-in for each server/hook? The user's tny precedent requires explicit source opt-in; a per-resource choice is stricter.
- Should `.agents/memory/` be committed by default for shared project decisions, or remain local unless an entry is explicitly published?

<!-- REPORT COMPLETE -->
