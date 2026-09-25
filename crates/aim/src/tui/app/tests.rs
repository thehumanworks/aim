//! Unit tests of the app state machine: update and key sequences in, view model and effects out.

use std::time::{Duration, Instant};

use aim_proto::conversation::Usage;
use aim_proto::daemon::Location;
use aim_proto::event::{EffortSource, SessionMeta};
use aim_proto::tool::ToolResult;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::tui::schedule::{Frame, Scheduler};
use crate::tui::view;

const ENV: &str = "<environment>\nworkspace: /w\nos: macos\ndate: 2026-09-25\n</environment>";

fn spec(persistence: Persistence) -> SessionSpec {
    SessionSpec {
        workspace: "/w".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence,
    }
}

fn summary(id: &str, state: SessionState) -> SessionSummary {
    SessionSummary {
        meta: SessionMeta {
            id: id.into(),
            created_ms: 0,
            workspace: "/w".into(),
            location: "local".into(),
            provider: "scripted".into(),
            model: "m1".into(),
            title: None,
            parent: None,
            agent: None,
        },
        state,
        persistence: Persistence::Ephemeral,
        last_activity_ms: 0,
        turns: 0,
    }
}

fn new_app(persistence: Persistence) -> App {
    let config =
        AppConfig { spec: spec(persistence), hyperlinks: false, home: None, persist_history: persistence == Persistence::Persistent };
    App::new(Theme::plain(), config, false)
}

/// An app attached to session `s1`, idle, with nothing in it.
fn attached() -> App {
    let mut app = new_app(Persistence::Ephemeral);
    assert_eq!(app.start(None), [Effect::Create { spec: spec(Persistence::Ephemeral), attempt: 1 }]);
    let effects = app.handle(Input::Created { attempt: 1, result: Ok(summary("s1", SessionState::Idle)) });
    assert_eq!(effects, [Effect::Attach { session: "s1".into(), resync: false, attempt: 2 }]);
    let effects =
        app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript: Vec::new(), resync: false, attempt: 2 });
    assert!(effects.is_empty());
    app
}

fn update(app: &mut App, update: SessionUpdate) -> Vec<Effect> {
    let attempt = app.attempt;
    app.handle(Input::Update { session: "s1".into(), attempt, update })
}

fn running() -> App {
    let mut app = attached();
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Running });
    app
}

fn press(code: KeyCode) -> Input {
    Input::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> Input {
    Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn typed(app: &mut App, text: &str) -> Vec<Effect> {
    let mut effects = Vec::new();
    for c in text.chars() {
        effects.extend(app.handle(press(KeyCode::Char(c))));
    }
    effects
}

fn text_of(parts: &[Part]) -> String {
    user_text(parts)
}

fn user(text: &str) -> Item {
    Item::User { parts: vec![Part::Text { text: text.into() }] }
}

fn prompts(effects: &[Effect]) -> Vec<(u64, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Prompt { id, parts, .. } => Some((*id, text_of(parts))),
            _ => None,
        })
        .collect()
}

fn states(app: &App) -> Vec<SteerState> {
    app.steers.iter().map(|s| s.state).collect()
}

fn notices(app: &App) -> Vec<String> {
    app.transcript
        .entries()
        .iter()
        .filter_map(|e| match e {
            Entry::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Sends `text` while a turn runs; returns the prompt's id.
fn steer(app: &mut App, text: &str) -> u64 {
    typed(app, text);
    let sent = prompts(&app.handle(press(KeyCode::Enter)));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, text);
    sent[0].0
}

#[test]
fn a_prompt_while_idle_starts_a_turn_and_is_echoed_by_the_session() {
    let mut app = attached();
    typed(&mut app, "hello");
    let effects = app.handle(press(KeyCode::Enter));
    assert_eq!(prompts(&effects), [(1, "hello".to_owned())]);
    assert!(app.composer.is_empty());
    let block = view::block(&app, 60, 20);
    assert!(!block.rows.iter().any(|r| r.to_string().contains("sending")), "an idle prompt shows no chip");
    app.handle(Input::PromptDone { id: 1, result: Ok(PromptOutcome::Started { turn: 1 }) });
    assert!(app.steers.is_empty());
    update(
        &mut app,
        SessionUpdate::ItemAdded { item: Item::User { parts: vec![Part::Text { text: ENV.into() }, Part::Text { text: "hello".into() }] } },
    );
    assert_eq!(app.transcript.entries(), [Entry::User { text: "hello".into() }], "echoed from the session, environment hidden");
}

#[test]
fn steering_goes_sending_queued_delivered_and_clears_when_idle() {
    let mut app = running();
    let id = steer(&mut app, "also run clippy");
    assert_eq!(states(&app), [SteerState::Sending]);
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    assert_eq!(states(&app), [SteerState::Queued]);
    let block = view::block(&app, 60, 20);
    assert!(block.rows.iter().any(|r| r.to_string().contains("⧗ queued also run clippy")));
    update(&mut app, SessionUpdate::SteerDelivered { count: 1 });
    update(&mut app, SessionUpdate::ItemAdded { item: user("also run clippy") });
    assert_eq!(states(&app), [SteerState::Delivered], "the delivered user item names the chip");
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert!(app.steers.is_empty());
    assert!(app.composer.is_empty(), "delivered steering is not refilled");
}

#[test]
fn delivery_before_the_prompt_answer_neither_duplicates_nor_refills() {
    let mut app = running();
    let id = steer(&mut app, "check the docs");
    update(&mut app, SessionUpdate::SteerDelivered { count: 1 });
    update(&mut app, SessionUpdate::ItemAdded { item: user("check the docs") });
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    assert_eq!(states(&app), [SteerState::Delivered], "the late answer does not add a second chip");
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert!(app.steers.is_empty());
    assert!(app.composer.is_empty());
}

#[test]
fn returned_steers_refill_the_composer_whatever_the_order() {
    // Returned after the session accepted it.
    let mut app = running();
    let id = steer(&mut app, "fix the test");
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    typed(&mut app, "draft");
    update(&mut app, SessionUpdate::SteersReturned { steers: vec![vec![Part::Text { text: "fix the test".into() }]] });
    assert!(app.steers.is_empty());
    assert_eq!(app.composer.text(), "fix the test\ndraft", "returned text goes first, the draft is kept");
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert_eq!(app.composer.text(), "fix the test\ndraft");

    // Returned before the session's answer arrived.
    let mut app = running();
    let id = steer(&mut app, "one more thing");
    update(&mut app, SessionUpdate::SteersReturned { steers: vec![vec![Part::Text { text: "one more thing".into() }]] });
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    assert!(app.steers.is_empty(), "the late answer does not resurrect the chip");
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert_eq!(app.composer.text(), "one more thing", "refilled exactly once");
}

#[test]
fn a_prompt_that_raced_the_turn_end_starts_a_new_turn() {
    let mut app = running();
    let id = steer(&mut app, "next");
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Started { turn: 2 }) });
    assert!(app.steers.is_empty());
    assert!(app.composer.is_empty());
}

#[test]
fn cancel_interrupts_then_settles_and_keeps_what_streamed() {
    let mut app = running();
    update(&mut app, SessionUpdate::ItemAdded { item: user("go") });
    update(
        &mut app,
        SessionUpdate::ItemAdded {
            item: Item::ToolCall { call_id: "c1".into(), name: "echo".into(), arguments: "{}".into(), native: None },
        },
    );
    update(&mut app, SessionUpdate::TextDelta { delta: "partial ans".into() });
    assert_eq!(app.handle(ctrl('c')), [Effect::Cancel("s1".into())]);
    assert_eq!(app.hint.as_deref(), Some("cancelling…"));
    update(&mut app, SessionUpdate::ItemAdded { item: Item::ToolResult { call_id: "c1".into(), result: ToolResult::error("cancelled") } });
    update(&mut app, SessionUpdate::TurnEnded { stop: StopReason::Cancelled });
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert!(app.live_text.is_empty());
    assert!(app.transcript.entries().contains(&Entry::Assistant { text: "partial ans".into(), interrupted: true }));
    assert_eq!(notices(&app), ["cancelled"]);
    assert_eq!(app.hint, None);
    assert_eq!(app.take_history(60).iter().filter(|r| r.text().contains("⏺ echo")).count(), 1);
    assert!(app.transcript.held().next().is_none());
}

#[test]
fn ctrl_c_when_idle_clears_then_exits_and_ctrl_d_exits_on_empty() {
    let mut app = attached();
    typed(&mut app, "abc");
    app.handle(ctrl('c'));
    assert!(app.composer.is_empty());
    assert!(!app.quitting());
    assert_eq!(app.handle(ctrl('c')), [Effect::Quit]);
    assert!(app.quitting());

    let mut app = attached();
    app.handle(ctrl('c'));
    typed(&mut app, "x");
    app.handle(ctrl('c'));
    assert!(!app.quitting(), "typing disarms the second ctrl+c");

    let mut app = attached();
    typed(&mut app, "ab");
    app.handle(press(KeyCode::Left));
    app.handle(ctrl('d'));
    assert_eq!(app.composer.text(), "a", "ctrl+d deletes when there is text");
    app.handle(ctrl('a'));
    app.handle(ctrl('k'));
    assert_eq!(app.handle(ctrl('d')), [Effect::Quit]);
}

#[test]
fn a_failed_prompt_puts_its_text_back() {
    let mut app = attached();
    typed(&mut app, "hello");
    let id = prompts(&app.handle(press(KeyCode::Enter)))[0].0;
    app.handle(Input::PromptDone { id, result: Err("session closed".into()) });
    assert_eq!(app.composer.text(), "hello");
    assert_eq!(notices(&app), ["not sent: session closed"]);
}

#[test]
fn commands_set_the_config_and_are_remembered() {
    let mut app = new_app(Persistence::Persistent);
    app.start(None);
    app.handle(Input::Attached { summary: persistent_summary("s1"), transcript: Vec::new(), resync: false, attempt: app.attempt });
    typed(&mut app, "/model gpt-x");
    let effects = app.handle(press(KeyCode::Enter));
    assert!(effects.contains(&Effect::SaveHistory("/model gpt-x".into())));
    assert!(effects.contains(&Effect::SetConfig(SessionConfigParams { session: "s1".into(), model: Some("gpt-x".into()), effort: None })));
    update(
        &mut app,
        SessionUpdate::ConfigChanged { model: "gpt-x".into(), effort: Some("high".into()), effort_source: EffortSource::Explicit },
    );
    let session = app.session.as_ref().unwrap();
    assert_eq!((session.model.as_str(), session.effort.as_deref()), ("gpt-x", Some("high")));
    typed(&mut app, "/dictate");
    app.handle(press(KeyCode::Enter));
    assert!(notices(&app).iter().any(|n| n == "/dictate arrives in M8"));
    typed(&mut app, "/nope");
    assert!(app.handle(press(KeyCode::Enter)).is_empty());
    assert_eq!(app.composer.text(), "/nope", "an unknown command stays for editing");
    app.handle(ctrl('u'));
    typed(&mut app, "/fullscreen");
    app.handle(press(KeyCode::Enter));
    assert_eq!(app.layout, Layout::Fullscreen);
}

fn completion_generation(effects: &[Effect]) -> Option<u64> {
    effects.iter().rev().find_map(|e| match e {
        Effect::Complete(request) => Some(request.generation),
        _ => None,
    })
}

fn candidate(label: &str) -> Candidate {
    Candidate { label: label.into(), insert: format!("@{label} "), detail: String::new(), kind: Kind::File }
}

#[test]
fn a_stale_completion_is_fenced_and_the_current_one_accepted() {
    let mut app = attached();
    let old = completion_generation(&typed(&mut app, "@s")).unwrap();
    let new = completion_generation(&typed(&mut app, "r")).unwrap();
    assert!(new > old);
    app.handle(Input::Completed(Completed { generation: old, candidates: vec![candidate("docs/stale.md")] }));
    assert!(!app.popup.open(), "an answer to an older generation is dropped");
    app.handle(Input::Completed(Completed { generation: new, candidates: vec![candidate("src/main.rs")] }));
    assert!(app.popup.open());
    app.handle(Input::Completed(Completed { generation: old, candidates: vec![candidate("docs/stale.md")] }));
    assert_eq!(app.popup.items, [candidate("src/main.rs")]);
    app.handle(press(KeyCode::Tab));
    assert_eq!(app.composer.text(), "@src/main.rs ");
    let effects = app.handle(press(KeyCode::Esc));
    assert!(effects.is_empty());
    assert!(!app.popup.open());
}

#[test]
fn escape_keeps_a_dismissed_popup_closed_until_the_token_changes() {
    let mut app = attached();
    let generation = completion_generation(&typed(&mut app, "@s")).unwrap();
    app.handle(Input::Completed(Completed { generation, candidates: vec![candidate("src/")] }));
    assert!(app.popup.open());
    app.handle(press(KeyCode::Esc));
    assert!(completion_generation(&typed(&mut app, "r")).is_none(), "no request for the dismissed token");
    assert!(completion_generation(&typed(&mut app, " @d")).is_some(), "a new token asks again");
}

#[test]
fn a_delta_burst_accumulates_and_paints_once() {
    let mut app = running();
    let start = Instant::now();
    let mut scheduler = Scheduler::default();
    scheduler.painted(start);
    for n in 0..500 {
        let effects = update(&mut app, SessionUpdate::TextDelta { delta: format!("w{n} ") });
        assert!(effects.is_empty());
        scheduler.stream(start + Duration::from_micros(n));
    }
    let frames: Vec<Frame> = [1, 16, 17, 40].iter().filter_map(|ms| scheduler.poll(start + Duration::from_millis(*ms))).collect();
    assert_eq!(frames, [Frame::Normal], "one frame for the whole burst");
    assert!(app.live_text.starts_with("w0 w1 ") && app.live_text.ends_with("w499 "));
    let block = view::block(&app, 60, 20);
    assert!(block.rows.iter().any(|r| r.to_string().contains("w499")), "the frame shows the latest text");
    let finished = app.live_text.clone();
    update(
        &mut app,
        SessionUpdate::ItemAdded { item: Item::Assistant { id: None, parts: vec![Part::Text { text: finished }], native: None } },
    );
    assert!(app.live_text.is_empty(), "the finished item replaces the partial");
}

#[test]
fn the_environment_block_never_shows_on_attach() {
    let mut app = new_app(Persistence::Ephemeral);
    app.start(Some("s1".into()));
    let transcript = vec![
        Item::User { parts: vec![Part::Text { text: ENV.into() }, Part::Text { text: "first".into() }] },
        Item::Assistant { id: None, parts: vec![Part::Text { text: "ok".into() }], native: None },
        Item::User { parts: vec![Part::Text { text: format!("{ENV}\n\nfrom aim run") }] },
        Item::User { parts: vec![Part::Text { text: ENV.into() }] },
    ];
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript, resync: false, attempt: app.attempt });
    let users: Vec<&Entry> = app.transcript.entries().iter().filter(|e| matches!(e, Entry::User { .. })).collect();
    assert_eq!(users, [&Entry::User { text: "first".into() }, &Entry::User { text: "from aim run".into() }]);
    let printed: Vec<String> = app.take_history(60).iter().map(Row::text).collect();
    assert!(!printed.iter().any(|r| r.contains("environment") || r.contains("workspace: /w")), "{printed:?}");
}

#[test]
fn a_dropped_stream_reattaches_and_applies_only_new_items() {
    let mut app = attached();
    update(&mut app, SessionUpdate::ItemAdded { item: user("a") });
    let effects = app.handle(Input::StreamEnded { session: "s1".into(), attempt: app.attempt });
    assert_eq!(effects, [Effect::Attach { session: "s1".into(), resync: true, attempt: app.attempt }]);
    let transcript = vec![user("a"), user("b")];
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript, resync: true, attempt: app.attempt });
    let users = app.transcript.entries().iter().filter(|e| matches!(e, Entry::User { .. })).count();
    assert_eq!(users, 2, "`a` once, then `b`");
    assert!(notices(&app).iter().any(|n| n.starts_with("reconnected")));
}

#[test]
fn updates_of_another_session_are_ignored() {
    let mut app = attached();
    app.handle(Input::Update { session: "other".into(), attempt: app.attempt, update: SessionUpdate::TextDelta { delta: "x".into() } });
    assert!(app.live_text.is_empty());
}

#[test]
fn usage_accumulates_for_the_status_line() {
    let mut app = running();
    let usage = Usage { input_tokens: 1_500, cached_input_tokens: 1_000, output_tokens: 200, reasoning_tokens: 50, ..Usage::default() };
    update(&mut app, SessionUpdate::Usage { usage: usage.clone() });
    update(&mut app, SessionUpdate::Usage { usage });
    assert_eq!(app.tokens, Tokens { input: 3_000, cached: 2_000, output: 400, reasoning: 100 });
    let status = view::status(&app, 120).to_string();
    assert!(status.contains("↑3.0k (2.0k cached) ↓400 (100 reasoning)"), "{status}");
}

#[test]
fn a_prompt_before_the_session_exists_is_sent_once_attached() {
    let mut app = new_app(Persistence::Ephemeral);
    app.start(None);
    typed(&mut app, "early");
    assert!(prompts(&app.handle(press(KeyCode::Enter))).is_empty());
    let effects = app.handle(Input::Attached {
        summary: summary("s1", SessionState::Idle),
        transcript: Vec::new(),
        resync: false,
        attempt: app.attempt,
    });
    assert_eq!(prompts(&effects), [(1, "early".to_owned())]);
}

#[test]
fn a_closed_session_keeps_the_text_and_says_why() {
    let mut app = attached();
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Closed });
    typed(&mut app, "hello");
    assert!(prompts(&app.handle(press(KeyCode::Enter))).is_empty());
    assert_eq!(app.composer.text(), "hello");
}

#[test]
fn the_picker_filters_and_attaches_another_session() {
    let mut app = attached();
    typed(&mut app, "/sessions");
    assert!(app.handle(press(KeyCode::Enter)).contains(&Effect::ListSessions));
    let mut other = summary("s2", SessionState::Closed);
    other.meta.workspace = "/elsewhere".into();
    app.handle(Input::Sessions(Ok(vec![summary("s1", SessionState::Idle), other])));
    typed(&mut app, "elsewhere");
    assert_eq!(app.picker.as_ref().unwrap().visible().len(), 1);
    let effects = app.handle(press(KeyCode::Enter));
    assert_eq!(effects, [Effect::Attach { session: "s2".into(), resync: false, attempt: app.attempt }]);
    assert!(app.picker.is_none());
    let mut s2 = summary("s2", SessionState::Idle);
    s2.meta.workspace = "/elsewhere".into();
    app.handle(Input::Attached { summary: s2, transcript: vec![user("old prompt")], resync: false, attempt: app.attempt });
    assert_eq!(app.session.as_ref().unwrap().id, "s2");
    assert!(notices(&app).iter().any(|n| n.starts_with("session s2")));
    assert!(app.transcript.entries().contains(&Entry::User { text: "old prompt".into() }), "the attached transcript is replayed");
}

#[test]
fn compaction_is_a_notice_and_the_transcript_keeps_everything() {
    let mut app = attached();
    update(&mut app, SessionUpdate::ItemAdded { item: user("early prompt") });
    update(
        &mut app,
        SessionUpdate::Compacted { replaced: 1, items: Vec::new(), method: "remote".into(), tokens_before: 180_000, tokens_after: 12_500 },
    );
    assert!(app.transcript.entries().contains(&Entry::User { text: "early prompt".into() }));
    assert_eq!(notices(&app), ["context compacted (remote): ~180.0k → ~12.5k tokens"]);
}

#[test]
fn jev_decisions_show_the_effort_in_force_without_touching_the_transcript() {
    let mut app = running();
    let decision = aim_proto::event::DecisionRecord {
        model: "m1".into(),
        ladder: vec!["low".into(), "medium".into(), "high".into()],
        current: 0,
        lo: 0,
        hi: 2,
        since_change: 3,
        hysteresis: 2,
        raw_score: 1.7,
        raw_confidence: 0.8,
        raw_probabilities: vec![0.1, 0.2, 0.7],
        raw_noul: [0.1, 0.8, 0.1],
        proposed_bp: 8_500,
        noul_bp: [1_000, 8_000, 1_000],
        output: 2,
        latency_ms: 40,
        input_tokens: None,
        cost_micro_usd: None,
    };
    let entries = app.transcript.entries().len();
    update(&mut app, SessionUpdate::Decision { decision });
    assert_eq!(app.jev_effort.as_deref(), Some("high"));
    assert_eq!(app.transcript.entries().len(), entries);
    assert!(view::status(&app, 120).to_string().contains("m1 · high (jev)"));
}

#[test]
fn a_successful_request_does_not_advance_the_turn_clock() {
    let mut app = running();
    app.handle(Input::Tick);
    app.handle(Input::Noop);
    assert_eq!(app.turn_seconds, 1);
}

// ---- REV10 regressions (each written to fail on the reviewed code) ----

fn eph_summary(id: &str) -> SessionSummary {
    let mut s = summary(id, SessionState::Idle);
    s.persistence = Persistence::Ephemeral;
    s
}

fn persistent_summary(id: &str) -> SessionSummary {
    let mut s = summary(id, SessionState::Idle);
    s.persistence = Persistence::Persistent;
    s
}

fn saves(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::SaveHistory(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// REV10 #1 (blocker): a persistent TUI attached to an ephemeral session neither shows disk history
/// nor writes the session's prompts to it.
#[test]
fn rev10_ephemeral_session_prompts_never_reach_disk_history() {
    let config = AppConfig { spec: spec(Persistence::Persistent), hyperlinks: false, home: None, persist_history: true };
    let mut app = App::new(Theme::plain(), config, false);
    let mut effects = app.start(Some("e1".into()));
    app.handle(press(KeyCode::Up));
    assert!(app.composer.is_empty(), "no disk history before the session's mode is known");
    effects.extend(app.handle(Input::Attached { summary: eph_summary("e1"), transcript: Vec::new(), resync: false, attempt: app.attempt }));
    assert!(!effects.contains(&Effect::LoadHistory), "the history file is not even read for an ephemeral session");
    app.handle(press(KeyCode::Up));
    assert!(app.composer.is_empty(), "disk history stays hidden in an ephemeral session");
    typed(&mut app, "private prompt");
    let mut effects = app.handle(press(KeyCode::Enter));
    typed(&mut app, "/model gpt-x");
    effects.extend(app.handle(press(KeyCode::Enter)));
    assert!(saves(&effects).is_empty(), "{effects:?}");
    assert_eq!(prompts(&effects).len(), 1);
    // A persistent session asks for the file once, then offers it.
    typed(&mut app, "/sessions");
    app.handle(press(KeyCode::Enter));
    app.handle(Input::Sessions(Ok(vec![persistent_summary("p1")])));
    app.handle(press(KeyCode::Enter));
    let effects =
        app.handle(Input::Attached { summary: persistent_summary("p1"), transcript: Vec::new(), resync: false, attempt: app.attempt });
    assert_eq!(effects.iter().filter(|e| **e == Effect::LoadHistory).count(), 1);
    app.handle(Input::HistoryLoaded(vec!["old secret".into()]));
    app.handle(press(KeyCode::Up));
    assert_eq!(app.composer.text(), "old secret");
}

/// REV10 #2: returned steering keeps an unsent paste chip's content.
#[test]
fn rev10_returned_steering_keeps_a_pasted_draft() {
    let mut app = running();
    let id = steer(&mut app, "steer me");
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    let big = "x".repeat(1_500);
    app.handle(Input::Paste(big.clone()));
    update(&mut app, SessionUpdate::SteersReturned { steers: vec![vec![Part::Text { text: "steer me".into() }]] });
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    let sent = prompts(&app.handle(press(KeyCode::Enter)));
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.contains(&big), "the paste went out in full: {:?}", sent[0].1.get(..60));
}

/// REV10 #3: text put back into the composer invalidates the completion popup.
#[test]
fn rev10_returned_text_fences_the_completion_popup() {
    let mut app = running();
    let id = steer(&mut app, "hello");
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    let old = completion_generation(&typed(&mut app, "@s")).unwrap();
    app.handle(Input::Completed(Completed { generation: old, candidates: vec![candidate("src/main.rs")] }));
    assert!(app.popup.open());
    update(&mut app, SessionUpdate::SteersReturned { steers: vec![vec![Part::Text { text: "hello".into() }]] });
    app.handle(press(KeyCode::Tab));
    assert!(app.composer.text().starts_with("hello"), "Tab did not edit the returned text: {:?}", app.composer.text());
    app.handle(Input::Completed(Completed { generation: old, candidates: vec![candidate("docs/")] }));
    assert!(!app.popup.items.iter().any(|c| c.label == "docs/"), "an old answer cannot fill the popup");
}

/// REV10 #4: a resync reconciles the partial text and steering against the snapshot.
#[test]
fn rev10_resync_reconciles_partial_text_and_steering() {
    let mut app = running();
    update(&mut app, SessionUpdate::ItemAdded { item: user("go") });
    update(&mut app, SessionUpdate::TextDelta { delta: "partial ans".into() });
    let id = steer(&mut app, "and this");
    app.handle(Input::PromptDone { id, result: Ok(PromptOutcome::Steered) });
    // The stream drops the delivery, the finished answer and Idle; a resync brings the snapshot.
    let effects = app.handle(Input::StreamEnded { session: "s1".into(), attempt: app.attempt });
    assert_eq!(effects, [Effect::Attach { session: "s1".into(), resync: true, attempt: app.attempt }]);
    let answer = Item::Assistant { id: None, parts: vec![Part::Text { text: "the full answer".into() }], native: None };
    let snapshot = vec![user("go"), user("and this"), answer];
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript: snapshot, resync: true, attempt: app.attempt });
    assert!(app.live_text.is_empty(), "no stale partial beside the snapshot");
    let answers: Vec<&Entry> = app.transcript.entries().iter().filter(|e| matches!(e, Entry::Assistant { .. })).collect();
    assert_eq!(answers, [&Entry::Assistant { text: "the full answer".into(), interrupted: false }]);
    assert!(app.steers.is_empty(), "the steer was delivered: {:?}", app.steers);
    assert!(app.composer.is_empty(), "delivered steering is not refilled");
}

fn picker_to(app: &mut App, target: &str) -> Vec<Effect> {
    typed(app, "/sessions");
    app.handle(press(KeyCode::Enter));
    app.handle(Input::Sessions(Ok(vec![summary("s1", SessionState::Idle), summary(target, SessionState::Idle)])));
    typed(app, target);
    app.handle(press(KeyCode::Enter))
}

/// REV10 #5: a prompt typed while switching sessions goes to the new session only.
#[test]
fn rev10_a_prompt_during_a_switch_goes_to_the_new_session() {
    let mut app = attached();
    let effects = picker_to(&mut app, "s2");
    assert!(effects.iter().any(|e| matches!(e, Effect::Attach { session, .. } if session == "s2")));
    typed(&mut app, "for B");
    let sent = app.handle(press(KeyCode::Enter));
    assert!(!sent.iter().any(|e| matches!(e, Effect::Prompt { session, .. } if session == "s1")), "{sent:?}");
    let effects = app.handle(Input::Attached {
        summary: summary("s2", SessionState::Idle),
        transcript: Vec::new(),
        resync: false,
        attempt: app.attempt,
    });
    let to_b: Vec<&Effect> = effects.iter().filter(|e| matches!(e, Effect::Prompt { session, .. } if session == "s2")).collect();
    assert_eq!(to_b.len(), 1, "{effects:?}");
}

/// REV10 #6: the old session's stream ending does not undo a requested switch.
#[test]
fn rev10_an_old_stream_end_does_not_cancel_a_switch() {
    let mut app = attached();
    let old = app.attempt;
    picker_to(&mut app, "s2");
    let effects = app.handle(Input::StreamEnded { session: "s1".into(), attempt: old });
    assert!(effects.is_empty(), "no re-attach to the old session: {effects:?}");
}

/// REV10 #7: several prompts before the first attach are all kept; a failed start returns them.
#[test]
fn rev10_prompts_before_attach_are_all_kept() {
    let mut app = new_app(Persistence::Ephemeral);
    app.start(None);
    for text in ["first", "second"] {
        typed(&mut app, text);
        assert!(prompts(&app.handle(press(KeyCode::Enter))).is_empty());
    }
    let effects = app.handle(Input::Attached {
        summary: summary("s1", SessionState::Idle),
        transcript: Vec::new(),
        resync: false,
        attempt: app.attempt,
    });
    let sent: Vec<String> = prompts(&effects).into_iter().map(|(_, t)| t).collect();
    assert_eq!(sent, ["first", "second"]);

    let mut app = new_app(Persistence::Ephemeral);
    app.start(None);
    typed(&mut app, "keep me");
    app.handle(press(KeyCode::Enter));
    app.handle(Input::Created { attempt: app.attempt, result: Err("no provider".into()) });
    assert_eq!(app.composer.text(), "keep me", "the unsent prompt is editable again");
}

/// REV10 #8: a delayed answer to the turn's first prompt cannot take a later steer's delivery.
#[test]
fn rev10_a_delayed_initial_ack_does_not_steal_a_delivery() {
    let mut app = attached();
    typed(&mut app, "A");
    let a = prompts(&app.handle(press(KeyCode::Enter)))[0].0;
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Running });
    update(&mut app, SessionUpdate::ItemAdded { item: user("A") });
    let b = steer(&mut app, "B");
    update(&mut app, SessionUpdate::SteerDelivered { count: 1 });
    update(&mut app, SessionUpdate::ItemAdded { item: user("B") });
    app.handle(Input::PromptDone { id: a, result: Ok(PromptOutcome::Started { turn: 1 }) });
    app.handle(Input::PromptDone { id: b, result: Ok(PromptOutcome::Steered) });
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert!(app.composer.is_empty(), "B was delivered, not returned: {:?}", app.composer.text());
    assert!(app.steers.is_empty());
}

/// REV10 #11: `/new` keeps the attached session's provider and location.
#[test]
fn rev10_new_keeps_the_attached_provider_and_location() {
    let mut app = new_app(Persistence::Persistent);
    app.start(Some("r1".into()));
    let mut remote = summary("r1", SessionState::Idle);
    remote.meta.provider = "openrouter".into();
    remote.meta.location = "ssh:box".into();
    remote.meta.workspace = "/srv/app".into();
    app.handle(Input::Attached { summary: remote, transcript: Vec::new(), resync: false, attempt: app.attempt });
    typed(&mut app, "/new");
    let effects = app.handle(press(KeyCode::Enter));
    let spec = effects
        .iter()
        .find_map(|e| match e {
            Effect::Create { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(spec.provider, "openrouter");
    assert_eq!(spec.location, Location::Ssh { destination: "box".into() });
    assert_eq!(spec.workspace, "/srv/app");
}

/// REV10 #12: a finished call behind a running one stays visible in the block.
#[test]
fn rev10_a_finished_call_behind_a_running_one_stays_visible() {
    let mut app = running();
    for (id, name) in [("a", "slow"), ("b", "fast")] {
        update(
            &mut app,
            SessionUpdate::ItemAdded {
                item: Item::ToolCall { call_id: id.into(), name: name.into(), arguments: "{}".into(), native: None },
            },
        );
    }
    update(&mut app, SessionUpdate::ItemAdded { item: Item::ToolResult { call_id: "b".into(), result: ToolResult::text("fast result") } });
    let block = view::block(&app, 60, 30);
    let text: Vec<String> = block.rows.iter().map(ToString::to_string).collect();
    assert!(text.iter().any(|r| r.contains("fast result")), "{text:?}");
    assert!(text.iter().any(|r| r.contains("⏺ slow")), "{text:?}");
}

/// REV10 #15: attaching a session in another workspace rebinds completion to it (and a remote
/// one to non-workspace sources only).
#[test]
fn rev10_attaching_another_workspace_rebinds_completion() {
    let mut app = attached();
    assert!(!app.outbox.iter().any(|e| matches!(e, Effect::Rebind { .. })), "same workspace: no rebind");
    let mut other = persistent_summary("b1");
    other.meta.workspace = "/elsewhere".into();
    app.handle(Input::Attached { summary: other, transcript: Vec::new(), resync: false, attempt: app.attempt });
    let old = app.attempt;
    typed(&mut app, "/sessions");
    app.handle(press(KeyCode::Enter));
    let mut remote = persistent_summary("r1");
    remote.meta.location = "ssh:box".into();
    remote.meta.workspace = "/srv".into();
    app.handle(Input::Sessions(Ok(vec![remote.clone()])));
    app.handle(press(KeyCode::Enter));
    assert!(app.attempt > old);
    let effects = app.handle(Input::Attached { summary: remote, transcript: Vec::new(), resync: false, attempt: app.attempt });
    assert!(effects.contains(&Effect::Rebind { workspace: "/srv".into(), local: false }), "{effects:?}");
}

// ---- REV12 regressions (each written to fail on the code it verified) ----

/// REV12: `/new` from an attached ephemeral session opens an ephemeral session.
#[test]
fn rev12_new_from_an_ephemeral_session_stays_ephemeral() {
    let mut app = new_app(Persistence::Persistent);
    app.start(Some("e1".into()));
    app.handle(Input::Attached { summary: eph_summary("e1"), transcript: Vec::new(), resync: false, attempt: app.attempt });
    typed(&mut app, "/new");
    let effects = app.handle(press(KeyCode::Enter));
    let spec = effects
        .iter()
        .find_map(|e| match e {
            Effect::Create { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(spec.persistence, Persistence::Ephemeral, "the new session is as private as the attached one");
    app.handle(Input::Created { attempt: app.attempt, result: Ok(eph_summary("e2")) });
    let effects = app.handle(Input::Attached { summary: eph_summary("e2"), transcript: Vec::new(), resync: false, attempt: app.attempt });
    assert!(saves(&effects).is_empty());
    typed(&mut app, "hi");
    assert!(saves(&app.handle(press(KeyCode::Enter))).is_empty());
}

fn config_to(effects: &[Effect], session: &str) -> Vec<SessionConfigParams> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::SetConfig(params) if params.session == session => Some(params.clone()),
            _ => None,
        })
        .collect()
}

/// REV12: `/model` and `/effort` typed during a switch apply to the session being opened.
#[test]
fn rev12_config_commands_during_a_switch_go_to_the_new_session() {
    for (command, model, effort) in [("/model x", Some("x"), None), ("/effort high", None, Some("high"))] {
        let mut app = attached();
        picker_to(&mut app, "s2");
        typed(&mut app, command);
        let effects = app.handle(press(KeyCode::Enter));
        assert!(config_to(&effects, "s1").is_empty(), "{command} did not go to the old session: {effects:?}");
        let effects = app.handle(Input::Attached {
            summary: summary("s2", SessionState::Idle),
            transcript: Vec::new(),
            resync: false,
            attempt: app.attempt,
        });
        let to_b = config_to(&effects, "s2");
        assert_eq!(to_b.len(), 1, "{command}: {effects:?}");
        assert_eq!((to_b[0].model.as_deref(), to_b[0].effort.as_deref()), (model, effort));
    }
}

/// REV12 (residual of REV10 #12): a finished call stays visible behind many running ones.
#[test]
fn rev12_a_finished_call_shows_behind_many_running_ones() {
    let mut app = running();
    for n in 0..12 {
        update(
            &mut app,
            SessionUpdate::ItemAdded {
                item: Item::ToolCall { call_id: format!("c{n}"), name: format!("slow{n}"), arguments: "{}".into(), native: None },
            },
        );
    }
    update(
        &mut app,
        SessionUpdate::ItemAdded {
            item: Item::ToolCall { call_id: "fin".into(), name: "quick".into(), arguments: "{}".into(), native: None },
        },
    );
    update(
        &mut app,
        SessionUpdate::ItemAdded { item: Item::ToolResult { call_id: "fin".into(), result: ToolResult::text("quick result") } },
    );
    let block = view::block(&app, 60, 30);
    let text: Vec<String> = block.rows.iter().map(ToString::to_string).collect();
    assert!(text.iter().any(|r| r.contains("quick result")), "{text:#?}");
}
