//! Unit tests of the app state machine: update and key sequences in, view model and effects out.

use std::time::{Duration, Instant};

use aim_proto::conversation::Usage;
use aim_proto::daemon::Location;
use aim_proto::event::SessionMeta;
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
        },
        state,
        persistence: Persistence::Ephemeral,
        last_activity_ms: 0,
        turns: 0,
    }
}

fn new_app(persistence: Persistence) -> App {
    let config = AppConfig { spec: spec(persistence), hyperlinks: false, home: None };
    App::new(Theme::plain(), config, Vec::new(), false)
}

/// An app attached to session `s1`, idle, with nothing in it.
fn attached() -> App {
    let mut app = new_app(Persistence::Ephemeral);
    assert_eq!(app.start(None), [Effect::Create(spec(Persistence::Ephemeral))]);
    let effects = app.handle(Input::Created(Ok(summary("s1", SessionState::Idle))));
    assert_eq!(effects, [Effect::Attach { session: "s1".into(), resync: false }]);
    let effects = app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript: Vec::new(), resync: false });
    assert!(effects.is_empty());
    app
}

fn update(app: &mut App, update: SessionUpdate) -> Vec<Effect> {
    app.handle(Input::Update { session: "s1".into(), update })
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
    assert_eq!(states(&app), [SteerState::Delivered]);
    update(&mut app, SessionUpdate::ItemAdded { item: user("also run clippy") });
    update(&mut app, SessionUpdate::StateChanged { state: SessionState::Idle });
    assert!(app.steers.is_empty());
    assert!(app.composer.is_empty(), "delivered steering is not refilled");
}

#[test]
fn delivery_before_the_prompt_answer_neither_duplicates_nor_refills() {
    let mut app = running();
    let id = steer(&mut app, "check the docs");
    update(&mut app, SessionUpdate::SteerDelivered { count: 1 });
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
    assert!(app.transcript.pending().next().is_none());
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
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript: Vec::new(), resync: false });
    typed(&mut app, "/model gpt-x");
    let effects = app.handle(press(KeyCode::Enter));
    assert!(effects.contains(&Effect::SaveHistory("/model gpt-x".into())));
    assert!(effects.contains(&Effect::SetConfig(SessionConfigParams { session: "s1".into(), model: Some("gpt-x".into()), effort: None })));
    update(&mut app, SessionUpdate::ConfigChanged { model: "gpt-x".into(), effort: Some("high".into()) });
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
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript, resync: false });
    let users: Vec<&Entry> = app.transcript.entries().iter().filter(|e| matches!(e, Entry::User { .. })).collect();
    assert_eq!(users, [&Entry::User { text: "first".into() }, &Entry::User { text: "from aim run".into() }]);
    let printed: Vec<String> = app.take_history(60).iter().map(Row::text).collect();
    assert!(!printed.iter().any(|r| r.contains("environment") || r.contains("workspace: /w")), "{printed:?}");
}

#[test]
fn a_dropped_stream_reattaches_and_applies_only_new_items() {
    let mut app = attached();
    update(&mut app, SessionUpdate::ItemAdded { item: user("a") });
    let effects = app.handle(Input::StreamEnded { session: "s1".into() });
    assert_eq!(effects, [Effect::Attach { session: "s1".into(), resync: true }]);
    let transcript = vec![user("a"), user("b")];
    app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript, resync: true });
    let users = app.transcript.entries().iter().filter(|e| matches!(e, Entry::User { .. })).count();
    assert_eq!(users, 2, "`a` once, then `b`");
    assert!(notices(&app).iter().any(|n| n.starts_with("reconnected")));
}

#[test]
fn updates_of_another_session_are_ignored() {
    let mut app = attached();
    app.handle(Input::Update { session: "other".into(), update: SessionUpdate::TextDelta { delta: "x".into() } });
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
    let effects = app.handle(Input::Attached { summary: summary("s1", SessionState::Idle), transcript: Vec::new(), resync: false });
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
    assert_eq!(app.handle(press(KeyCode::Enter)), [Effect::Attach { session: "s2".into(), resync: false }]);
    assert!(app.picker.is_none());
    let mut s2 = summary("s2", SessionState::Idle);
    s2.meta.workspace = "/elsewhere".into();
    app.handle(Input::Attached { summary: s2, transcript: vec![user("old prompt")], resync: false });
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
