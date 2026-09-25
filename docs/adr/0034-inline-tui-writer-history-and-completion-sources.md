# ADR 0034: Paint the inline TUI with a relative block writer; keep scrollback terminal-owned

- Status: Proposed
- Date: 2026-09-25
- Baseline: 0014, 0015, 0017, 0026
- Scope: how the M3 TUI (`crates/aim/src/tui`) puts pixels on an inline terminal, its prompt
  history file, the completion-source contract and the test-only script mode. Not the screen
  model itself (ADR 0015), not the declarative UI protocol or DTCG themes (ADR 0017, M7).

## Context

ADR 0015 chose chat in native scrollback with a bounded pinned block, alternate-screen overlays and
one semantic transcript model. Implementing it raised questions the research answered only partly
(`docs/research/tui-ux.md` §1, §9):

- ratatui's `Viewport::Inline` fixes the viewport height when the terminal is created and locates
  itself with cursor-position queries; codex forked ratatui's terminal for the same reasons
  (`refs/codex/codex-rs/tui/src/tui.rs:1388-1407`, `custom_terminal.rs`). The block here grows and
  shrinks with the popup, chips and composer.
- On a width change both pi and codex clear the screen and scrollback and replay the transcript
  (`refs/pi/packages/tui/src/tui-main-screen.ts:276-330`; codex `app/resize_reflow.rs:292-310`).
  That erases whatever the user had in the terminal before aim started, which ADR 0015 rules out.
- Terminals that rewrap rows on resize (iTerm2, kitty, Ghostty, WezTerm, Terminal.app, VTE, tmux)
  move the block's rows under the application. Measured in tmux 3.7 while building this
  (`crates/aim/tests/tui_tmux.rs`, `AIM_TUI_TRACE_BYTES` replays): tmux rewraps its grid at once
  but the pty size and `SIGWINCH` reach the application later; and `ED 0` issued from the screen's
  top-left counts as a full clear, which tmux's `scroll-on-clear` copies into scrollback first.
  `vt100`, used by the PTY tests, truncates instead of rewrapping and shows neither effect.

## Decision

1. **Own inline writer** (`tui/inline.rs`), fed by ratatui buffers. Finished transcript rows are
   printed with plain `\r\n` (every terminal moves them into history; scroll regions can drop rows,
   per codex's notes) and never touched again: no resize clears or replays scrollback. Rows are
   wrapped at the width they are printed at; a rewrapping terminal owns them afterwards.
2. **The block is erased relative to the hardware cursor** (`CR`, `CUU n`, then `ERASE_BELOW`:
   `EL 2`, `DECSC`, `CUD 1`, `ED 0`, `DECRC` — never a bare `ED 0` that could start at the screen's
   top-left). Every frame is one write inside DEC 2026 synchronized output with autowrap off; block
   rows are at most `cols - 1` wide and carry their own SGR. The block is bounded by `max_rows` so
   the erase always covers it.
3. **Nothing rewrappable above the cursor.** While a turn streams, the hardware cursor parks
   (hidden) at the block's top and the composer draws its cursor; at idle the cursor is in the
   composer (IME, screen readers), with only a blank row above it — the rule sits between the
   composer and the status line. After a known narrowing the erase offset follows a reflow model
   (`rows_above_cursor`: `Rewraps` by default, `Truncates` for xterm, the Linux console and
   `AIM_TUI_REFLOW=0`). The terminal size is read (an ioctl) before every frame.
4. **One frame scheduler** (`tui/schedule.rs`): keys paint at once, stream updates coalesce to one
   frame per 16 ms, resize bursts settle for 75 ms then repaint.
5. **Steering is keyed by prompt id**: every sent prompt is tracked as `Sending` until the session
   answers (`Started` removes it; `Steered` makes it a chip), `SteerDelivered` marks the oldest
   undelivered chips, `SteersReturned` removes chips by text and refills the composer, and idle
   refills anything accepted but neither delivered nor returned. Any arrival order of the
   session's answer and its updates is safe.
6. **Completion sources are a contract** (`aim::tui::Source`): `complete(&Request) -> BoxFuture<Vec
   <Candidate>>`, one source per trigger (`@` files and directories, `$` skills, `/` commands and
   their arguments). The broker cancels the request of an older input generation; the app drops
   any answer whose generation is not current. Local sources walk with `ignore` (gitignore-aware,
   ≤ 50 000 entries, depth ≤ 16, cached 10 s) and rank with `nucleo-matcher`; skills come from
   `<workspace>/.agents/skills/*/SKILL.md` and `~/.aim/skills` (ADR 0014). A harness-backed file
   source for SSH workspaces implements the same trait.
7. **Prompt history file**: `aim_home()/history`, UTF-8, one prompt per line with `\` and newline
   escaped (`\\`, `\n`), created with mode 0600, appended on send; the last 1 MiB is read at start
   (1000 entries kept). Commands are recorded only when they take an argument. Ephemeral runs
   neither read nor write it (docs/architecture.md §5.4).
8. **Theme** (`tui/theme.rs`): `AIM_THEME` (`dark`/`light`/`plain`), then `NO_COLOR`, then
   `COLORFGBG`; dark otherwise. No OSC 11 background query: first paint must not wait on the
   terminal. Colours are the 16 named ANSI colours plus attributes, in one struct for M7's themes.
9. **Test-only script mode**: `aim --script <file>` (feature `test-support`) runs the real TUI on a
   scripted provider and a fake workspace. `cargo test` enables the feature through a self
   dev-dependency; default and release builds do not contain it.
10. The environment block is hidden by matching `context::environment`'s `<environment>…
    </environment>` text at the head of a user item (its own part, or the head of `aim run`'s
    part). A dedicated `Part` kind would make this exact; proposed for the contract owner.

## Consequences

Scrollback and pre-existing terminal output survive every resize and every overlay. The price is
that history printed at one width stays wrapped at that width (terminals that rewrap do so
themselves), and that the parked caret hides the hardware cursor while a turn streams (steering
typed then shows a drawn cursor; an IME composes at the block's top). The reflow model is a
heuristic for idle narrowing with multi-row drafts; the bounded erase and the layout keep its
error inside the block. New terminals with other erase quirks are diagnosed with `AIM_TUI_TRACE`
(erase log) and `AIM_TUI_TRACE_BYTES` (byte chunks per resize, replayable).

## Verification

- PTY tests on the real binary (`crates/aim/tests/tui_pty.rs`, `vt100`):
  `inline_scrollback_preserved`, `overlay_restores_inline`, `fullscreen_transcript_parity`,
  `completion_stale_result_fenced` (ADR 0015), plus steering, paste chips, history across restarts
  and Ctrl+C cancel.
- `tmux_resize_mid_stream_keeps_scrollback_clean` (`crates/aim/tests/tui_tmux.rs`, ignored: needs
  tmux) resizes 80→50→100→44→30 during a stream and with a popup open: 15/15 runs clean with
  items 2–3; before them, 4/10 to 10/15 runs were clean.
- Unit tests of the app (steering in every arrival order, fence, cancel), the writer
  (`rows_above_cursor`, parked caret, erase), the scheduler (a 500-delta burst paints once) and
  `TestBackend` snapshots of the block, fullscreen and picker.
- Measurements (`crates/aim/tests/tui_bench.rs`, release, through the PTY): first paint p50 8.7 ms,
  keystroke-to-paint p50 0.97 ms / p95 1.06 ms, streams at ≈ 55 fps, 10 000-item replay 88–112 ms
  at 38–40 MiB RSS (9 MiB empty).
- Live: `live_tui_codex_turn` and `live_tui_openrouter_turn` (`crates/aim/tests/tui_live.rs`).
