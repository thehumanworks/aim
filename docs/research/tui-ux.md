# R7 — TUI and interaction design survey

Source snapshot: the read-only `refs/codex`, `refs/pi`, `refs/oh-my-pi`, and first-party `refs/tny`
clones supplied for aim. Claims are about these checkouts, not an installed binary or newer upstream
release. `UNVERIFIED` marks behavior or measurements the inspected source does not establish.

## TL;DR

- **FACT:** Codex uses an inline viewport that keeps native terminal scrollback, switching to alternate
  screen for fullscreen overlays. Pending history is inserted under synchronized output before drawing.
  `refs/codex/codex-rs/tui/src/tui.rs:431-445,973-1052,1135-1182`.
- **FACT:** Pi offers both normal main-screen native scrollback and fullscreen alternate-screen chat.
  Its main renderer diffs changed lines, uses DEC 2026 synchronized output, and coalesces paints to a 16
  ms minimum interval. `refs/pi/packages/coding-agent/src/modes/interactive/tui-renderer.ts:8-47`;
  `refs/pi/packages/tui/src/tui-main-screen.ts:276-304,449-489`;
  `refs/pi/packages/tui/src/tui.ts:946-1005`.
- **FACT:** Oh-my-pi also preserves native scrollback for normal chat and borrows alternate screen for
  modal overlays. It adds output-backlog throttling, resize settling, and image-specific paint delays.
  `refs/oh-my-pi/packages/tui/src/tui.ts:126-138,825-863,3019-3039`.
- **FACT:** tny uses append-only terminal scrollback and repaints only a bottom block containing
  streaming partial line, overlay, completion list, queue, status, and composer. Its full-screen agents
  dashboard clears display *and scrollback* rather than using alternate screen.
  `refs/tny/src/tui/tui_draw.c:417-482,503-523`;
  `refs/tny/docs/adr/0138-full-screen-agents-dashboard.md:15-34`.
- **FACT:** Codex uses ignore-aware `nucleo` file matching; pi and oh-my-pi have distinct fuzzy
  completion pipelines; tny scans at most 6,000 files with a limited top-level `.gitignore` parser.
  These are materially different quality/performance contracts.
  `refs/codex/codex-rs/file-search/src/lib.rs:315-336,408-452`;
  `refs/pi/packages/tui/src/autocomplete.ts:770-835`;
  `refs/oh-my-pi/packages/tui/src/autocomplete.ts:1107-1148`; `refs/tny/src/tui/tui_commands.c:140-250`.
- **FACT:** Codex has Markdown via `pulldown-cmark`, syntax via `syntect`, semantic OSC 8 links and
  unified diffs; pi has word-level edit diff and Kitty/iTerm2 terminal images; oh-my-pi has `marked`
  plus incremental cached highlight rendering. `refs/codex/codex-rs/tui/src/markdown.rs:22-25,178`;
  `refs/codex/codex-rs/tui/src/diff_render.rs:1-33`;
  `refs/pi/packages/coding-agent/src/modes/interactive/components/diff.ts:23-24,74`;
  `refs/oh-my-pi/packages/tui/src/components/markdown.ts:1756-1796`.
- **FACT:** Pi's `ExtensionUIContext` can replace header, footer, status, editor and renderers; add
  widgets, overlays, dialogs, completion providers and themes. These APIs use TUI `Component`/`Theme`
  factories, so they are not a renderer-neutral web contract.
  `refs/pi/packages/coding-agent/src/core/extensions/types.ts:143-294,1451-1493`.
- **FACT:** Oh-my-pi adds richer selectors/dialog timing and a more expressive status-line preset
  system. Codex offers configurable status segments/themes but no comparable third-party TUI component
  API was verified. tny has fixed ANSI styling and no verified plugin UI contract.
  `refs/oh-my-pi/packages/coding-agent/src/extensibility/extensions/types.ts:235-350`;
  `refs/oh-my-pi/packages/tui/src/status-line/presets.ts:4-95`;
  `refs/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:56-157`;
  `refs/tny/src/tui/tui_draw.c:166-198`.
- **FACT:** tny's historical startup measurement was 3.892→3.942 ms median to first prompt, with 20 PTY
  launches per candidate, not a measured per-frame cost. Its provider host prewarm ADR is now historical
  for Codex; current docs keep first paint free of provider I/O.
  `refs/tny/docs/verification/cpp-series/evidence.md:125-129`;
  `refs/tny/docs/adr/0002-tui-provider-prewarm.md:1-4`; `refs/tny/docs/tui.md:329-335`.
- **RECOMMENDATION:** Give aim normal inline chat with native scrollback, a bounded pinned composer, and
  temporary alternate-screen views for search/dashboard/inspection. Provide fullscreen chat as an
  option. Treat scrollback-clearing as an explicit destructive view action.
- **RECOMMENDATION:** Let plugins return bounded, declarative UI nodes and semantic actions/events. The
  TUI and web client should render the same nodes, with the host retaining focus, keybinding, permission
  and output sanitization authority.
- **RECOMMENDATION:** A single async completion broker should return typed, cancelable, ranked
  candidates from files, directories, skills, slash commands, agents, sessions and saved programs; cloud
  buckets can join later with stale-result fencing and source-aware latency budgets.

## Findings

### 1. Screen model, scrollback and paint

#### Codex

**FACT:** Codex explicitly keeps terminal-native scrollback in its inline viewport. Its fullscreen
overlays enter alternate screen after flushing pending history and restore the saved viewport on return.
This hybrid prevents ordinary chat from disappearing on exit while giving pickers a controlled canvas.
`refs/codex/codex-rs/tui/src/tui.rs:431-445,973-1052`.

**FACT:** History lines are batched and inserted before the draw inside crossterm synchronized output.
Scrollback growth/resize differs by terminal: normal terminals use a partial scroll region and line
insertion, Windows can clear/scroll the full display, and Zellij has a direct-scroll path. The draw
scheduler caps at 120 frames/second; that is a cap, **not** a measured frame time.
`refs/codex/codex-rs/tui/src/tui.rs:1135-1182,1225-1249`;
`refs/codex/codex-rs/tui/src/tui/scrollback.rs:19-102`;
`refs/codex/codex-rs/tui/src/tui/frame_rate_limiter.rs:1-35`.

**FACT:** ChatWidget maintains finalized history cells plus one mutable in-flight cell. A transcript
overlay keeps a cached live tail, so active tool groups can appear there without rebuilding every
historical cell on each draw. `refs/codex/codex-rs/tui/src/chatwidget.rs:1-17`.

#### Pi

**FACT:** Pi's `createInteractiveTui` selects `TuiMainScreen` for regular chat or `TuiAltScreen` for
fullscreen. Fullscreen has a searchable scroll viewport, jump-to-latest, selection copy and right-click
paste. `refs/pi/packages/coding-agent/src/modes/interactive/tui-renderer.ts:8-47`;
`refs/pi/packages/coding-agent/src/modes/interactive/chat-viewport.ts:21-45`.

**FACT:** Main screen leaves preceding terminal output intact on first paint and works with terminal
scrollback. It uses DEC 2026 synchronized output and a differential line renderer rather than repainting
all rows; a width/height change can clear and replay because old line wrapping becomes invalid. Render
requests coalesce under a 16 ms minimum interval, while urgent input can trigger immediate paint.
**UNVERIFIED:** no comparable measured p50/p95 frame time or large-transcript memory number was found.
`refs/pi/packages/tui/src/tui-main-screen.ts:276-304,330-349,449-489`;
`refs/pi/packages/tui/src/tui.ts:946-1005`.

#### Oh-my-pi

**FACT:** The normal screen tracks history rows and viewport in native terminal scrollback; a
modal/fullscreen overlay temporarily borrows the alternate buffer and returns without rewriting the
normal transcript. It conditionally uses synchronized output and throttles if stdout has backlog. A
resize settle window is 120 ms; image painting includes a Ghostty-specific 100 ms delay. These are
source timing controls, not published frame-cost measurements.
`refs/oh-my-pi/packages/tui/src/tui.ts:126-138,409-423,825-863,3019-3039`.

#### tny

**FACT:** tny intentionally acts like a shell rather than an IDE: append-only transcript, bottom
status/composer, no ncurses. Completed streaming lines move from `partial` to `out` and terminal
scrollback; the unfinished line is redrawn with the bottom block. `erase_block` clears that block, then
`tui_render` paints partial, overlay, completion, queue, status and composer. No alternate-screen chat
path is documented. `refs/tny/docs/tui.md:1-14`; `refs/tny/src/tui/tui_draw.c:417-482,503-523`.

**FACT:** It probes actual terminal size via `TIOCGWINSZ` plus terminal cursor-position response because
nested/sandbox/browser PTYs can report wrong dimensions. DECAWM is off while painting the bottom block,
so an overlong row clips rather than unexpectedly wraps and leaves stale status rows. Each reasoning
line owns its SGR reset so a completed line has no dependency on the repaint state of the next one.
`refs/tny/docs/tui.md:25-33,320-323`; `refs/tny/docs/adr/0012-self-contained-sgr-lines.md:31-53`;
`refs/tny/src/tui/tui_draw.c:435-481,529-551`.

**FACT:** Menus live in a transient overlay buffer, bounded by rows left after
status/composer/popover/partial; overflow says how many rows are hidden. It is never written to
transcript scrollback and falls back to plain lines without a TTY. Full-screen agents dashboard
explicitly clears visible screen and scrollback (`CSI 3 J`) instead of alt screen; this removes previous
chat from the terminal's copyable scrollback until reattachment/replay.
`refs/tny/docs/adr/0003-transient-menu-overlay.md:16-35`;
`refs/tny/docs/adr/0138-full-screen-agents-dashboard.md:15-34`.

**FACT:** A source-bound historical startup comparison reports a 3.942 ms median first prompt after a
3.892 ms baseline, measured over 20 PTY launches; local mock TTFT was 538.9 ms versus 541.8 ms baseline.
The product contract asks below 10 ms to first prompt and no provider/skill/MCP walk before it. ADR
0002's earlier host prewarm happened only after first paint and was superseded for current native HTTP
Codex. `refs/tny/docs/verification/cpp-series/evidence.md:125-129`;
`refs/tny/docs/size-and-speed.md:35-44,69-75`;
`refs/tny/docs/adr/0002-tui-provider-prewarm.md:1-4,15-29`; `refs/tny/docs/tui.md:329-335`.

### 2. Composer, paste, history and mid-turn input

#### Codex

**FACT:** Terminal mode enables bracketed paste, raw mode and enhanced keyboard input. Composer has
persisted/local history and Ctrl+R history search. It supports Vim mode; Ctrl+G opens an external
editor. During a busy turn, Enter submits while Tab can queue a follow-up. Large paste over 1,000
characters collapses to an atomic placeholder, reducing composer repaint/cursor costs and accidental
massive draft display. Unbracketed paste bursts, including on Windows, receive a heuristic path.
`refs/codex/codex-rs/tui/src/tui.rs:238-258`;
`refs/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:33-34,64-90,116-143,202-254,448-450`;
`refs/codex/codex-rs/tui/src/keymap.rs:1651`.

#### Pi and oh-my-pi

**FACT:** Pi terminal enables bracketed paste; its editor tracks collapsed paste markers, 100-entry
history and abortable/debounced completion. Ctrl-V reads an image into an attachment path, otherwise
pastes text. Mid-turn submission distinguishes steer and follow-up, including a queue during compaction.
The same editing surface is exposed to extensions through editor text, paste and editor-replacement
APIs. A custom Vim editor is an extension example, not evidence of a built-in Vim mode.
`refs/pi/packages/tui/src/terminal.ts:184-188`;
`refs/pi/packages/tui/src/components/editor.ts:30-54,322-347,424-435`;
`refs/pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts:3027-3069,3250-3260,4327-4337,4599-4686`;
`refs/pi/packages/coding-agent/src/core/extensions/types.ts:239-272`.

**FACT:** Oh-my-pi supports opt-in built-in Vim editing, external editor action and collapsed paste
atoms that expand at submit. Its active-turn submission distinguishes steer and follow-up. The extension
UI type additionally permits multiline editor prompt style and cancellable focused custom UI. Pi has an
external-editor action, but the inspected source gives no built-in Pi Vim mode.
`refs/oh-my-pi/packages/tui/src/components/editor.ts:562,789,1549-1561,2429-2493,2704,2933`;
`refs/oh-my-pi/packages/coding-agent/src/modes/interactive-mode.ts:2767-2777,4654-4698,4818-4855`;
`refs/pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts:975-981`.

#### tny

**FACT:** Composer wraps with real newlines (`Ctrl-J`, Alt-J/Alt-Enter, CSI-u Shift-Enter,
backslash+Enter); plain Enter submits. It enables bracketed paste and normalizes pasted CR/CRLF to LF,
so a pasted newline never submits. Ctrl-V materializes a clipboard image in `/tmp` and inserts its path
as inline code; explicit `/image PATH` attaches pixels. This avoids silently selecting a
provider-specific image input format. `refs/tny/docs/tui.md:13-24,83-108`;
`refs/tny/docs/adr/0025-clipboard-images-paste-as-paths.md:19-42`.

**FACT:** Persistent prompt history is `~/.tny/history` with multiline escaping and a bounded in-memory
load; ephemeral history never writes. Up/Down at draft edge navigates history. Ctrl-R dictates into
editable draft; Ctrl-O optimizes a draft for review. No Vim mode or external-editor binding is
documented in inspected TUI source/docs. `refs/tny/src/tui/tui_hist.c:1-93`;
`refs/tny/docs/tui.md:83-106`.

**FACT:** During an active native turn, Enter steers by parking a user message before the next provider
POST; unsupported host loops queue. Queue state appears by the composer and disappears on send;
interruption/failure drops queued messages. ADR 0013 closes a lost-text race: a backend accepting
ownership must emit `STEER_REJECTED` containing the text before `TURN_END` if it was not delivered, and
the TUI requeues it. Historical Codex app-server steer bookkeeping in that ADR was removed by direct
Responses backend. `refs/tny/docs/adr/0011-mid-turn-input-steer-or-queue.md:38-79`;
`refs/tny/docs/adr/0013-steer-rejection-owns-the-text.md:28-70`;
`refs/tny/docs/adr/0065-codex-chatgpt-responses-backend.md:1-9`.

### 3. Completion and suggestions

#### Codex

**FACT:** The composer has `@` completion spanning plugins/files/skills and `$` for skills/apps, with
popups above the composer. It can show a dim follow-up suggestion in an empty composer; Tab accepts that
draft without submitting. File search uses a per-query session; each keystroke updates the query and
stale sessions are invalidated.
`refs/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:19-23,36-43,74-77`;
`refs/codex/codex-rs/tui/src/file_search.rs:1-6,52-72,108-124`.

**FACT:** File search walks with `ignore::WalkBuilder`, requires Git semantics where configured, then
scores with `nucleo`; default visible result count is 20, ordered by score with path tie-breaking.
**UNVERIFIED:** median/p95 discovery latency on a million-file repo is not established by this source
survey. `refs/codex/codex-rs/file-search/src/lib.rs:7-8,43-55,107-130,315-336,408-452`.

#### Pi

**FACT:** Pi's `@` search uses `fd` and ignore-aware scanning, collects at most 100 search candidates,
ranks by score/depth/length and presents 20. Slash commands and skills use `fuzzyFilter`; commands may
implement `getArgumentCompletions`, allowing completion after the command name rather than only at the
first token. The API is also extension-accessible through `addAutocompleteProvider`. **UNVERIFIED:** no
comparable measured latency on very large repositories found.
`refs/pi/packages/tui/src/autocomplete.ts:148-205,257-280,338-397,770-835`;
`refs/pi/packages/coding-agent/src/core/extensions/types.ts:143-294`.

#### Oh-my-pi

**FACT:** Oh-my-pi uses native `fuzzyFind` with Git ignore behavior, a result cap of 100 and abortable
search; local directory reads have a two-second cache. It recognizes `skill:` and bare-name skill forms
inside a prompt, with collapse/escape behavior so a skill token can be inserted without accidentally
rewriting other text. Its `addAutocompleteProvider` extension hook is ignored in headless modes.
`refs/oh-my-pi/packages/tui/src/autocomplete.ts:1-30,447-518,535-545,1107-1148`;
`refs/oh-my-pi/packages/coding-agent/src/extensibility/extensions/types.ts:314-320`.

#### tny

**FACT:** `/` at draft start filters commands; `@` at token start filters files and `$` filters skills.
Command names get prefix matches first, then case-insensitive subsequence matches. Files get basename
substring matches first, then full-path subsequence matches, capped at 40 displayed results. Skills use
name subsequence filtering. `refs/tny/src/tui/tui_input.c:117-179`;
`refs/tny/src/tui/tui_commands.c:90-137,222-259`.

**FACT:** The file list is lazily built once by a depth-10 recursive scan, capped at 6,000 files. It
skips hidden paths and selected directories (`.git`, `node_modules`, `target`, etc.). Its `.gitignore`
support is only simple top-level, slash-free patterns (max 128), so it is **not** complete Git ignore
semantics and has no directory candidate rows. This limits correctness and scalability for aim's
file/dir/cloud scope. `refs/tny/src/tui/tui.h:23-26`; `refs/tny/src/tui/tui_commands.c:140-250`.

**FACT:** Popovers are transient and bounded to eight rows; approval/clarification focus suppresses
special `/ @ $` interpretation, so an answer such as `/tmp/x` remains literal.
`refs/tny/src/tui/tui.h:23-26`; `refs/tny/docs/tui.md:108-121`;
`refs/tny/docs/adr/0003-transient-menu-overlay.md:16-35`.

### 4. Transcript: Markdown, diff, tools, reasoning, images and links

#### Codex

**FACT:** Markdown rendering uses `pulldown-cmark`, code highlighting uses `syntect`, and unified diffs
have line numbers and syntax-aware coloring. The link layer carries semantic link metadata until final
OSC 8 emission, reducing the chance that a clipped or restyled line breaks hyperlink control sequences.
Diff palettes differ for light/dark terminals. `refs/codex/codex-rs/tui/src/markdown.rs:22-25,178`;
`refs/codex/codex-rs/tui/src/render/highlight_streaming.rs:11-14`;
`refs/codex/codex-rs/tui/src/diff_render.rs:1-33`;
`refs/codex/codex-rs/tui/src/terminal_hyperlinks.rs:1-4,651-701`.

**FACT:** MCP tool result cards preview a shared three-row budget across content blocks; expanded
transcript retains more detail. Completed reasoning summary is transcript-only rather than compact main
scrollback; raw reasoning replay requires an explicit `show_raw_agent_reasoning` setting. MCP images
show a `Returned image` text marker. Kitty/Sixel detection exists for ambient pet art, but general
tool-image terminal rendering is **UNVERIFIED**.
`refs/codex/codex-rs/tui/src/history_cell/mcp.rs:1-4,240-305`;
`refs/codex/codex-rs/tui/src/history_cell/messages.rs:732-746`;
`refs/codex/codex-rs/tui/src/chatwidget/replay.rs:23-39`;
`refs/codex/codex-rs/tui/src/pets/mod.rs:175-210`.

#### Pi

**FACT:** Pi uses `marked` for Markdown and highlight.js with language registration/lazy full import.
Edit diffs use word-level change emphasis (`diffWords`); tool output truncates with an expand hint, and
reasoning can show a hidden label or Markdown toggled per block. Terminal image output supports
Kitty/iTerm2 paths behind capability detection; OSC 8 links are terminal-gated.
`refs/pi/packages/tui/src/components/markdown.ts:1`;
`refs/pi/packages/coding-agent/src/utils/syntax-highlight.ts:1-21,57`;
`refs/pi/packages/coding-agent/src/modes/interactive/components/diff.ts:23-24,74`;
`refs/pi/packages/coding-agent/src/modes/interactive/components/tool-execution.ts:170`;
`refs/pi/packages/coding-agent/src/modes/interactive/components/assistant-message.ts:118-166`;
`refs/pi/packages/tui/src/terminal-image.ts:7-10,78-133`.

#### Oh-my-pi

**FACT:** Oh-my-pi uses `marked` for Markdown and incremental, cached highlighting. It prewarms TS/TSX
highlight work on a native worker to avoid a roughly 250 ms first regex compilation on the interactive
path. Terminal image paint is budgeted alongside screen updates. **UNVERIFIED:** the inspected evidence
does not establish a universal side-by-side diff or image protocol fallback matrix.
`refs/oh-my-pi/packages/tui/src/components/markdown.ts:1-9,1756-1796,2287-2341`;
`refs/oh-my-pi/packages/coding-agent/src/modes/interactive-mode.ts:1902-1904`;
`refs/oh-my-pi/packages/tui/src/tui.ts:833-863`.

#### tny

**FACT:** Transcript is intentionally Markdown-ish rather than a full parsed Markdown widget: headings,
lists, fenced code, plain-text diffs with +/- coloring. Reasoning is dim by line; leading
whitespace-only assistant/reasoning deltas are hidden visually, while raw deltas persist to
JSON/session. A streamed tool detail is flattened to one line with terminal controls removed before
paint. No general terminal inline-image renderer or OSC 8 link renderer was verified in `src/tui`;
clipboard images become paths, with explicit image attachment separately.
`refs/tny/docs/tui.md:13-24,303-323`; `refs/tny/src/tui/tui.c:82-93`;
`refs/tny/docs/adr/0025-clipboard-images-paste-as-paths.md:19-42`.

### 5. Status, notifications and chrome

#### Codex

**FACT:** A status setup picker can enable/reorder/preview segments including model, reasoning effort,
cwd, Git/PR/diff state, context and token use, rate limits, estimated thread cost and progress. It emits
sanitized OSC 0 terminal titles. Notifications coalesce and prioritize turn-complete, approvals and user
questions. The inspected TUI sources establish mechanisms but do not by themselves verify delivery
behavior for every terminal's OSC 9/777 path.
`refs/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:1-20,56-157`;
`refs/codex/codex-rs/tui/src/terminal_title.rs:1-16,46-66`;
`refs/codex/codex-rs/tui/src/chatwidget/notifications.rs:5-34,72-99`.

#### Pi and oh-my-pi

**FACT:** Pi footer includes cwd/Git branch, token and monetary usage, context percentage,
model/thinking level and extension-provided statuses. Its terminal can emit OSC 9;4 progress and set a
title; OSC 9/777 notifications and bell behavior remain **UNVERIFIED** here. Oh-my-pi provides named
status presets (`default`, `minimal`, `compact`, `full`, `nerd`, `ascii`) spanning
model/effort/vim/path/git/PR/context/cost/token/cache/rate/subagents/time. These are user-facing
composition controls; extension-provided strings still need host clipping/sanitization at output.
`refs/pi/packages/coding-agent/src/modes/interactive/components/footer.ts:47-49,103-119,141-161,170-194,234-242`;
`refs/pi/packages/tui/src/terminal.ts:9-10,520`;
`refs/oh-my-pi/packages/tui/src/status-line/presets.ts:4-95`.

#### tny

**FACT:** tny status row includes provider, model, permission mode, session ID, task, input/output
tokens, image count, note or spinner, and cwd or SSH target. Reverse-video structure survives
`NO_COLOR`, while `--color=never` uses text delimiters. There is no verified configurable status-segment
or theme-plugin API. `refs/tny/src/tui/tui_draw.c:166-198`;
`refs/tny/docs/adr/0026-color-vs-attribute-sgr.md:26-68`.

### 6. Themes and terminal capabilities

**FACT:** Codex has bundled and custom `.tmTheme` selection with live preview, plus truecolor/256/16
palette detection and quantization; diff light/dark palettes are separate.
`refs/codex/codex-rs/tui/src/theme_picker.rs:1-12,302-404`;
`refs/codex/codex-rs/tui/src/terminal_palette.rs:6-20,33-88`;
`refs/codex/codex-rs/tui/src/diff_render.rs:10-21`.

**FACT:** Pi themes have a JSON schema and bundled dark/light definitions; terminal color-scheme query
has a background fallback. Pi exposes `theme`, `getAllThemes`, `getTheme`, `setTheme` to extensions.
Oh-my-pi has custom JSON themes with 256-color conversion/truecolor selection and async theme
getters/setter in its extension UI type. tny's visible contract separates color from structural SGR
attributes and respects `NO_COLOR`, `CLICOLOR_FORCE`, `--color=always/never`; no external theme file was
verified. `refs/pi/packages/coding-agent/src/modes/interactive/theme/theme-schema.json:1`;
`refs/pi/packages/coding-agent/src/modes/interactive/theme/theme.ts:616-728`;
`refs/pi/packages/coding-agent/src/core/extensions/types.ts:143-294`;
`refs/oh-my-pi/packages/tui/src/theme/theme.ts:722-784`;
`refs/oh-my-pi/packages/coding-agent/src/extensibility/extensions/types.ts:235-350`;
`refs/tny/docs/adr/0026-color-vs-attribute-sgr.md:26-68`.

**UNVERIFIED:** the clone inspection did not establish a single uniform light/dark auto-detection
behavior across all four clients. Aim should probe capability and offer explicit override rather than
infer color safety from `$TERM` alone.

### 7. UI extensibility: exact API surface and portability limit

**FACT:** Pi's `ExtensionUIContext`
(`refs/pi/packages/coding-agent/src/core/extensions/types.ts:143-294`) exposes these method families
(names retained exactly):

```text
select, confirm, input, notify, onTerminalInput
setStatus, setWorkingMessage, setWorkingVisible, setWorkingIndicator,
setHiddenThinkingLabel
setWidget (string[] or component factory; above/below editor)
setFooter, setHeader, setTitle
custom (focused overlay with options/handle)
pasteToEditor, setEditorText, getEditorText, editor
addAutocompleteProvider
setEditorComponent, getEditorComponent
theme, getAllThemes, getTheme, setTheme
getToolsExpanded, setToolsExpanded
```

**FACT, representative exact signatures:** Pi types `setStatus(key: string, text: string | undefined): void`,
`setWidget(key: string, content: string[] | undefined, options?: ExtensionWidgetOptions): void`, and
an overload whose `content` is `((tui: TUI, theme: Theme) => Component & { dispose?(): void }) | undefined`.
It types `setFooter(factory: ((tui: TUI, theme: Theme, footerData: ReadonlyFooterDataProvider) =>
Component & { dispose?(): void }) | undefined): void`,
`addAutocompleteProvider(factory: AutocompleteProviderFactory): void`, and
`setEditorComponent(factory: EditorFactory | undefined): void`. The focused `custom<T>` returns `Promise<T>`
and receives a factory with `tui`, `theme`, `keybindings`, and a `done(result: T)` callback; overlay options
can be static or computed. `refs/pi/packages/coding-agent/src/core/extensions/types.ts:159-222,236-275`.

**FACT:** Pi extension registration additionally exposes `registerMessageRenderer`,
`registerMarkdownTransformer`, `registerEntryRenderer`, commands and keyboard shortcuts. The
widget/footer/header/editor/renderers are host-TUI `Component` or `Theme` factories; JavaScript
extensions can execute inside UI context, which is powerful but couples them to one renderer and trust
model. `refs/pi/packages/coding-agent/src/core/extensions/types.ts:1451-1493`.

**FACT:** Oh-my-pi's related UI type adds selector items/descriptions, optional `askDialog`,
`timeoutStartsOnPresentation`, cancellable `custom` via signal, multiline editor prompt style and async
theme access. Compared with the inspected pi type, it does not expose `setWorkingVisible`,
`setWorkingIndicator`, `setHiddenThinkingLabel`, or `getEditorComponent`. `addAutocompleteProvider` is
supported in TUI and ignored in headless modes.
`refs/oh-my-pi/packages/coding-agent/src/extensibility/extensions/types.ts:235-350`.

**FACT:** Codex offers in-product status/theme configuration and specialized panes; a general
third-party TUI component API equivalent to Pi's was **UNVERIFIED** in inspected source. tny's Python
extension parity covers agent/tool lifecycle and redacted provider events, but its TUI has fixed ANSI
rows and no verified per-plugin status/widget/renderer registration. These are evidence limits, not
claims of impossibility. `refs/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:1-20`;
`refs/codex/codex-rs/tui/src/theme_picker.rs:1-12`;
`refs/tny/docs/adr/0028-extension-parity-contract.md:61-114`; `refs/tny/src/tui/tui_draw.c:166-235`.

### 8. Multi-session and background-agent UX

**FACT:** Codex resume/fork picker supports cursor pagination, local search, lazy transcript preview and
provider/source/cwd filtering. `/subagents` picker and Alt+Left/Right switch among agents; rows expose
nickname, role, path and running/closed state. Descendant thread listing is refreshed from app-server
`thread/list` in pages (100/page, cap 1,000), with ancestor filtering; some parent-owned threads disable
direct input. `refs/codex/codex-rs/tui/src/resume_picker.rs:156-157,352-367`;
`refs/codex/codex-rs/tui/src/multi_agents.rs:1-5,34-45,76-131`;
`refs/codex/codex-rs/tui/src/app/agent_picker.rs:13-55,89-146`.

**FACT:** tny's agents dashboard lists all saved sessions across workspaces. Entry via `tny agents`,
`/agents`, Ctrl-X or acknowledged Left-arrow background handoff uses a full-screen clear; the running
turn continues in its detached runner. Sections place current cwd first, then other paths
alphabetically, sessions newest first; path filter is case-insensitive subsequence and preserves
selection as workspace bucket plus session ID across refresh. Enter attaches to an owned live runner or
inspects saved transcript read-only, with explicit continuation rather than silent replay.
`refs/tny/docs/adr/0138-full-screen-agents-dashboard.md:15-34`;
`refs/tny/docs/adr/0167-workspace-dashboard-navigation.md:12-38`; `refs/tny/docs/tui.md:123-176`.

**FACT:** Pi defaults to regular main-screen mode; `--tui-mode fullscreen` is optional, and exiting
fullscreen can print transcript by switching through regular mode. Its session selector offers a
parent-child tree, search/sort/name filtering, rename/delete and path toggles. Oh-my-pi has a pinned
subagent HUD with collapsed/expanded role/model/task preview, plus resume/branch/tree flows; exact
picker parity with Pi is **UNVERIFIED**. A tny-style global cwd-grouped dashboard was not verified in
either. `refs/pi/packages/coding-agent/src/core/settings-manager.ts:159-162,1266`;
`refs/pi/packages/coding-agent/src/cli/args.ts:325`;
`refs/pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts:835-841`;
`refs/pi/packages/coding-agent/src/modes/interactive/components/session-selector.ts:155-178,206-263,281-311,566-600`;
`refs/oh-my-pi/packages/coding-agent/src/modes/interactive-mode.ts:806-900,3815-3818`.

### 9. Performance and failure lessons

**FACT:** Four distinct rendering tactics appear: Codex batches scrollback insertion under synchronized
draw and caps at 120 FPS; Pi diffs lines and coalesces for 16 ms; oh-my-pi responds to output
backlog/resize and image costs; tny commits complete lines and redraws a small bottom block. These are
design observations, not matched benchmark results. `refs/codex/codex-rs/tui/src/tui.rs:1135-1182`;
`refs/codex/codex-rs/tui/src/tui/frame_rate_limiter.rs:1-35`;
`refs/pi/packages/tui/src/tui-main-screen.ts:449-489`; `refs/pi/packages/tui/src/tui.ts:946-1005`;
`refs/oh-my-pi/packages/tui/src/tui.ts:825-863`; `refs/tny/src/tui/tui_draw.c:417-482`.

**FACT:** tny's transient-overlay ADR and dashboard ADR both require visible-screen PTY assertions,
because raw escape-byte matching misses visual residue. Its steering ADR makes accepted text ownership
explicit and ensures a failed steer returns the actual text. These are good regression targets for a TUI
whose backend and UI are separate processes. `refs/tny/docs/adr/0003-transient-menu-overlay.md:37-43`;
`refs/tny/docs/adr/0138-full-screen-agents-dashboard.md:48-63`;
`refs/tny/docs/adr/0013-steer-rejection-owns-the-text.md:28-70`.

## Implications for aim

1. **RECOMMENDATION — screen:** Default to inline/native scrollback with an independently rendered
   bottom composer/status/popup region. Use alternate screen for sessions, search, diff inspection, and
   focused plugin dialogs; offer optional fullscreen chat. On resize, rebuild wrapped history from
   semantic message cells rather than replaying arbitrary plugin ANSI bytes. Copy/paste and history
   should remain usable after TUI exit. Tradeoff: hybrid terminal state has more resize/cursor tests
   than a single alt-screen canvas. Basis: `refs/codex/codex-rs/tui/src/tui.rs:431-445,973-1052`;
   `refs/pi/packages/tui/src/tui-main-screen.ts:276-304,330-349`;
   `refs/tny/docs/adr/0138-full-screen-agents-dashboard.md:15-34`.
2. **RECOMMENDATION — component architecture:** Keep a semantic transcript model and a bounded view
   tree. Make renderer tasks pure from snapshot+terminal capabilities to cell/grid output; serialize
   writes through one frame scheduler. Use dirty-region diffing and synchronized output when supported.
   Rate-limit nonurgent streams but render keystrokes/approval promptly. Verify each terminal feature
   via capability fallback, and benchmark paint p50/p95, bytes/frame, startup-to-first-paint, and
   long-transcript RSS. Basis: `refs/pi/packages/tui/src/tui-main-screen.ts:449-489`;
   `refs/pi/packages/tui/src/tui.ts:946-1005`; `refs/oh-my-pi/packages/tui/src/tui.ts:825-863`.
3. **RECOMMENDATION — plugin/agent UI protocol:** A WASM plugin should submit versioned declarative
   nodes (`Text`, `Markdown`, `Code`, `Diff`, `List`, `Table`, `Progress`, `ImageRef`, `Action`,
   `FormField`, `Stack`, `Tabs`) under a named slot (`message`, `tool_result`, `status`, `header`,
   `footer`, `sidebar`, `modal`, `suggestion`). Nodes carry stable IDs, semantic style tokens, text
   alternatives, bounded sizes, capability requirements and action IDs. The host translates an action
   into a typed event for the plugin; plugins never emit terminal escapes, seize raw input, bypass
   permission, or mutate stored transcript directly. The same node schema renders to TUI cells and web
   DOM. Tradeoff: a declarative subset is less expressive than Pi's arbitrary `Component` factories;
   escape hatches can be introduced as separately trusted host-specific extensions. Basis:
   `refs/pi/packages/coding-agent/src/core/extensions/types.ts:143-294,1451-1493`;
   `refs/tny/docs/adr/0003-transient-menu-overlay.md:27-35`.
4. **RECOMMENDATION — completion broker:** Each provider returns
   `{source,id,label,insert_text,kind,description,score,scope,revision}` against a draft/cursor
   snapshot, with cancellation token and result deadline. Merge by stable identity and source-aware
   score normalization, prioritize exact/prefix path/name matches, then fuzzy subsequence, preserve
   current selection across refresh and drop stale responses. Sources: file/dir index (full Git ignore
   rules), slash commands with argument completions, `.agents/skills`, markdown agents, sessions, saved
   code-mode programs; later cloud buckets return paginated remote candidates. Tradeoff: indexing and
   cloud freshness require cache invalidation and visible “searching”/“more” states. Basis:
   `refs/codex/codex-rs/tui/src/file_search.rs:52-72,108-124`;
   `refs/pi/packages/tui/src/autocomplete.ts:338-397,770-835`;
   `refs/oh-my-pi/packages/tui/src/autocomplete.ts:1107-1148`;
   `refs/tny/src/tui/tui_commands.c:222-259`.
5. **RECOMMENDATION — composer:** Support real multiline editing, bracketed paste, UTF-8 grapheme-safe
   movement, collapsed large-paste chips, history search, optional Vim/external editor, explicit
   image-attach versus path-paste, and visible steer/queue outcome. A rejected steer should carry its
   text back to the queue before turn settlement. Tradeoff: rich editing adds state-machine complexity;
   isolate it from the agent loop and test with recorded key/terminal streams. Basis:
   `refs/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:33-34,64-90,202-254`;
   `refs/tny/docs/adr/0013-steer-rejection-owns-the-text.md:28-70`.
6. **RECOMMENDATION — chrome and themes:** Let users configure status segments and order, but make
   segments semantic and width-budgeted (context, tokens, cost, rate limit, model/effort, cwd/SSH,
   branch, session/agent, approval/connection). Themes should map named tokens to color/attribute
   variants for dark/light, truecolor/256/16 and no-color. A plugin may request a theme token, never raw
   SGR/OSC. Tradeoff: theme token/version stability becomes a public contract. Basis:
   `refs/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:56-157`;
   `refs/codex/codex-rs/tui/src/terminal_palette.rs:6-20,33-88`;
   `refs/tny/docs/adr/0026-color-vs-attribute-sgr.md:26-68`.
7. **RECOMMENDATION — session UX:** Put cwd-first grouped sessions behind one key, keep background turns
   attached to durable daemon state, show tool/approval/worker progress, and distinguish **Attach
   live**, **Inspect read-only**, **Continue saved**, and **Fork** as separate actions. Never silently
   re-run a turn from a picker. For swarms, show the worker path and evidence/hand-off status in a
   drilldown rather than mixing workers into one flat transcript. Basis:
   `refs/tny/docs/adr/0167-workspace-dashboard-navigation.md:12-38`; `refs/tny/docs/tui.md:151-176`;
   `refs/codex/codex-rs/tui/src/multi_agents.rs:34-45,76-131`.
8. **RECOMMENDATION — signature UX:** (a) A single command palette searches commands, paths, skills,
   agents, sessions and saved programs with type badges and argument hints; (b) hover/focus-like inline
   inspection of a result without injecting it into transcript; (c) a compact “what is running, where”
   strip for local/SSH/tool/agent scopes; (d) one-keystroke durable background handoff with a visible
   receipt; (e) opt-in next-action suggestions that insert an editable draft, never auto-submit. These
   combine proven picker, queue and background patterns while keeping agent authority explicit. Basis:
   `refs/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:74-77`;
   `refs/pi/packages/tui/src/autocomplete.ts:338-397`; `refs/tny/docs/tui.md:83-100,123-149`.

## Open questions for the user

- Should aim default to inline native scrollback with an optional fullscreen mode, or to fullscreen with
  explicit export/copy actions? This changes the core renderer contract and how terminal history
  survives exit.
- Should third-party WASM UI plugins be limited to the shared declarative TUI/web schema, with a
  separately trusted host-native escape hatch for custom rendering? This decides portability and
  terminal-control authority.

<!-- REPORT COMPLETE -->
