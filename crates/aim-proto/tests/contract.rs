//! Contract tests for the wire types. The error-code table is checked exhaustively (it is a finite
//! domain, so this is a proof by enumeration); envelopes and content are checked by round trip.

use aim_proto::content::{Base64Bytes, Content};
use aim_proto::daemon::Location;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{self, ExactEdit, FsEditParams, Precondition};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use aim_proto::rpc::{Envelope, ErrorObject, Message, Method, Notification, RequestId};
use serde_json::json;

#[test]
fn remote_location_is_additive_and_contains_no_credential() {
    let remote = Location::Remote { url: "wss://example.test/rpc".to_owned() };
    let wire = serde_json::to_value(&remote).unwrap();
    assert_eq!(wire, json!({"kind": "remote", "url": "wss://example.test/rpc"}));
    assert_eq!(serde_json::from_value::<Location>(wire).unwrap(), remote);
    assert_eq!(serde_json::from_value::<Location>(json!({"kind": "local"})).unwrap(), Location::Local);
    assert_eq!(
        serde_json::from_value::<Location>(json!({"kind": "ssh", "destination": "box"})).unwrap(),
        Location::Ssh { destination: "box".to_owned() }
    );
}

#[test]
fn error_codes_are_a_bijection_between_numbers_and_names() {
    for code in ErrorCode::ALL {
        assert_eq!(ErrorCode::from_number(code.number()), code, "number of {code}");
        assert_eq!(ErrorCode::ALL.iter().filter(|c| c.number() == code.number()).count(), 1);
        assert_eq!(ErrorCode::ALL.iter().filter(|c| c.name() == code.name()).count(), 1);
        let wire = serde_json::to_value(code).unwrap();
        assert_eq!(wire, json!(code.name()), "serde name of {code}");
        let obj = ErrorObject::from(ProtoError::new(code, "m").with_detail(json!({"x": 1})));
        let back = ProtoError::from(obj);
        assert_eq!(back, ProtoError::new(code, "m").with_detail(json!({"x": 1})));
    }
}

#[test]
fn foreign_error_objects_map_by_number() {
    let obj = ErrorObject { code: -32601, message: "no".into(), data: None };
    assert_eq!(ProtoError::from(obj).code, ErrorCode::MethodNotFound);
    let unknown = ErrorObject { code: 7, message: "?".into(), data: Some(json!("free text")) };
    assert_eq!(ProtoError::from(unknown).code, ErrorCode::Internal);
}

#[test]
fn envelopes_classify_and_round_trip() {
    let request = Message::Request { id: RequestId::Number(1), method: "fs.read".into(), params: json!({"a": 1}) };
    let notification = Message::Notification { method: "exec.output".into(), params: json!(null) };
    let ok = Message::Response { id: RequestId::String("x".into()), outcome: Ok(json!(null)) };
    let err = Message::Response {
        id: RequestId::Number(2),
        outcome: Err(ErrorObject::from(ProtoError::new(ErrorCode::Denied, "protected path"))),
    };
    for message in [request, notification, ok, err] {
        let wire = serde_json::to_string(&message.clone().into_envelope()).unwrap();
        let env: Envelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(Message::from_envelope(env).unwrap(), message, "{wire}");
    }
}

#[test]
fn null_result_is_a_success_not_an_error() {
    let env: Envelope = serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":null}"#).unwrap();
    assert_eq!(Message::from_envelope(env).unwrap(), Message::Response { id: RequestId::Number(3), outcome: Ok(json!(null)) });
}

#[test]
fn malformed_envelopes_are_invalid_requests() {
    for wire in [r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#, r#"{"jsonrpc":"2.0"}"#] {
        let env: Envelope = serde_json::from_str(wire).unwrap();
        assert_eq!(Message::from_envelope(env).unwrap_err().code, ErrorCode::InvalidRequest, "{wire}");
    }
}

#[test]
fn content_uses_text_when_possible_and_round_trips_bytes() {
    assert_eq!(Content::from_bytes(b"hello".to_vec()), Content::Utf8 { text: "hello".into() });
    let binary = vec![0xff, 0x00, 0xfe];
    let content = Content::from_bytes(binary.clone());
    assert_eq!(content, Content::Base64 { data: Base64Bytes(binary.clone()) });
    let wire = serde_json::to_value(&content).unwrap();
    assert_eq!(wire, json!({"encoding": "base64", "data": "/wD+"}));
    let back: Content = serde_json::from_value(wire).unwrap();
    assert_eq!(back.into_bytes(), binary);
}

#[test]
fn typed_method_params_serialize_to_the_documented_shape() {
    assert_eq!(harness::FsEdit::NAME, "fs.edit");
    let params = FsEditParams {
        workspace: WorkspaceId::new("w1"),
        path: "src/lib.rs".into(),
        edits: vec![ExactEdit { old: "a".into(), new: "b".into(), replace_all: false }],
        precondition: Precondition::IfAbsent,
        idempotency_key: IdempotencyKey::new("k1"),
        scope: None,
    };
    let wire = serde_json::to_value(&params).unwrap();
    assert_eq!(
        wire,
        json!({
            "workspace": "w1",
            "path": "src/lib.rs",
            "edits": [{"old": "a", "new": "b", "replace_all": false}],
            "precondition": {"kind": "if_absent"},
            "idempotency_key": "k1"
        })
    );
    let back: FsEditParams = serde_json::from_value(wire).unwrap();
    assert_eq!(back, params);
}

#[test]
fn every_harness_method_has_a_json_schema() {
    fn schema<M: Method>() -> (String, serde_json::Value, serde_json::Value) {
        (
            M::NAME.to_owned(),
            serde_json::to_value(schemars::schema_for!(M::Params)).unwrap(),
            serde_json::to_value(schemars::schema_for!(M::Result)).unwrap(),
        )
    }
    let all = [
        schema::<harness::Initialize>(),
        schema::<harness::WorkspaceOpen>(),
        schema::<harness::FsStat>(),
        schema::<harness::FsRead>(),
        schema::<harness::FsWrite>(),
        schema::<harness::FsEdit>(),
        schema::<harness::FsList>(),
        schema::<harness::FsMkdir>(),
        schema::<harness::FsRemove>(),
        schema::<harness::FsRename>(),
        schema::<harness::ExecSpawn>(),
        schema::<harness::ExecRead>(),
        schema::<harness::ExecWriteStdin>(),
        schema::<harness::ExecResize>(),
        schema::<harness::ExecSignal>(),
        schema::<harness::ExecRelease>(),
        schema::<harness::ExecWait>(),
        schema::<harness::FsReadMany>(),
        schema::<harness::FsCopy>(),
        schema::<harness::WatchStart>(),
        schema::<harness::WatchStop>(),
        schema::<harness::Grep>(),
        schema::<harness::Glob>(),
        schema::<harness::ToolsList>(),
        schema::<harness::ToolsCall>(),
    ];
    let mut names: Vec<&str> = all.iter().map(|(n, _, _)| n.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len(), "method names must be unique");
    for (name, params, _) in &all {
        assert!(params.is_object(), "{name} params schema");
    }
}

#[test]
fn every_daemon_method_has_a_json_schema_and_updates_are_tagged() {
    use aim_proto::daemon;
    fn schema<M: Method>() -> (String, serde_json::Value) {
        (M::NAME.to_owned(), serde_json::to_value(schemars::schema_for!(M::Params)).unwrap())
    }
    let all = [
        schema::<daemon::DaemonInitialize>(),
        schema::<daemon::SessionCreate>(),
        schema::<daemon::SessionList>(),
        schema::<daemon::SessionAttach>(),
        schema::<daemon::SessionDetach>(),
        schema::<daemon::SessionPrompt>(),
        schema::<daemon::SessionCancel>(),
        schema::<daemon::SessionSetConfig>(),
        schema::<daemon::SessionClose>(),
        schema::<daemon::MediaTranscribe>(),
    ];
    let mut names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len(), "method names must be unique");
    let update = daemon::SessionUpdateParams {
        session: "s".into(),
        update: daemon::SessionUpdate::StateChanged { state: daemon::SessionState::Running },
    };
    assert_eq!(serde_json::to_value(&update).unwrap(), json!({"session": "s", "update": {"type": "state_changed", "state": "running"}}));
    let detached = daemon::SessionDetachedParams { session: "s".into(), reason: daemon::DetachReason::Lagged };
    assert_eq!(<daemon::SessionDetachedNotification as Notification>::NAME, "session.detached");
    assert_eq!(serde_json::to_value(&detached).unwrap(), json!({"session": "s", "reason": "lagged"}));
    assert_eq!(
        serde_json::from_value::<daemon::SessionDetachedParams>(json!({"session": "s", "reason": "replaced"})).unwrap().reason,
        daemon::DetachReason::Replaced
    );
}

#[test]
fn unknown_session_events_are_preserved_not_rejected() {
    use aim_proto::event::{EventBody, SessionEvent};
    let wire = json!({"schema": 9, "seq": 4, "turn": 2, "ts_ms": 1, "body": {"kind": "from_the_future", "x": [1, 2]}});
    let event: SessionEvent = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(event.body, EventBody::Unknown(json!({"kind": "from_the_future", "x": [1, 2]})));
    assert_eq!(serde_json::to_value(&event).unwrap(), wire, "round-trips verbatim");
    let known = json!({"schema": 1, "seq": 1, "turn": 1, "ts_ms": 1, "body": {"kind": "turn_started"}});
    let event: SessionEvent = serde_json::from_value(known).unwrap();
    assert_eq!(event.body, EventBody::TurnStarted);
}

#[test]
fn adr_0038_fields_are_additive_and_old_records_read_as_explicit() {
    use aim_proto::daemon::SessionUpdate;
    use aim_proto::event::{EffortSource, EventBody, SessionAgent, SessionMeta};
    // A log written before ADR 0038: no effort source (explicit), no agent.
    let old: EventBody = serde_json::from_value(json!({"kind": "config_changed", "model": "m", "effort": "low"})).unwrap();
    assert_eq!(old, EventBody::ConfigChanged { model: "m".into(), effort: Some("low".into()), effort_source: EffortSource::Explicit });
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        json!({"kind": "config_changed", "model": "m", "effort": "low"}),
        "explicit is left out"
    );
    let auto = EventBody::ConfigChanged { model: "m".into(), effort: None, effort_source: EffortSource::Auto };
    assert_eq!(serde_json::to_value(&auto).unwrap(), json!({"kind": "config_changed", "model": "m", "effort_source": "auto"}));
    let meta = json!({"id": "s", "created_ms": 1, "workspace": "/w", "location": "local", "provider": "p", "model": "m"});
    let parsed: SessionMeta = serde_json::from_value(meta.clone()).unwrap();
    assert_eq!(parsed.agent, None);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), meta, "a session without an agent serializes as before");
    let with_agent =
        SessionMeta { agent: Some(SessionAgent { name: "reader".into(), allow: Some(vec!["Read".into()]), deny: Vec::new() }), ..parsed };
    let wire = serde_json::to_value(&with_agent).unwrap();
    assert_eq!(wire["agent"], json!({"name": "reader", "allow": ["Read"]}));
    assert_eq!(serde_json::from_value::<SessionMeta>(wire).unwrap(), with_agent);
    let rejected = SessionUpdate::ConfigRejected { model: None, effort: Some("ultra".into()), message: "not offered".into() };
    assert_eq!(serde_json::to_value(&rejected).unwrap(), json!({"type": "config_rejected", "effort": "ultra", "message": "not offered"}));
    assert_eq!(aim_proto::daemon::AUTO_EFFORT, "auto");
}

/// A reader built before ADR 0066: `EventBody` as it was, without the nested tool events.
mod before_adr_0066 {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum EventBody {
        TurnStarted,
        Item {
            item: Value,
        },
        TurnEnded {
            stop: Value,
        },
        #[serde(untagged)]
        Unknown(Value),
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum SessionUpdate {
        ToolStarted { call_id: String, name: String, arguments: String },
        ToolFinished { call_id: String, name: String, result: Value },
    }
}

#[test]
fn adr_0066_nested_tool_records_round_trip_and_old_readers_keep_them_as_unknown() {
    use aim_proto::daemon::SessionUpdate;
    use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent};
    use aim_proto::tool::ToolResult;

    let started = EventBody::NestedToolStarted {
        parent: "call_run_code".into(),
        call_id: "0199a0c0-0000-7000-8000-000000000001:0".into(),
        name: "Bash".into(),
        arguments: r#"{"command":"ls"}"#.into(),
    };
    let finished = EventBody::NestedToolFinished {
        parent: "call_run_code".into(),
        call_id: "0199a0c0-0000-7000-8000-000000000001:0".into(),
        name: "Bash".into(),
        result: ToolResult::text("src"),
    };
    for body in [started, finished] {
        let event = SessionEvent { schema: EVENT_SCHEMA, seq: 3, turn: 1, ts_ms: 1, body };
        let wire = serde_json::to_value(&event).unwrap();
        // This build reads its own records back.
        assert_eq!(serde_json::from_value::<SessionEvent>(wire.clone()).unwrap(), event);
        // An older build keeps them verbatim as unknown and writes them back unchanged.
        let old: before_adr_0066::EventBody = serde_json::from_value(wire["body"].clone()).unwrap();
        assert!(matches!(old, before_adr_0066::EventBody::Unknown(_)), "{old:?}");
        assert_eq!(serde_json::to_value(&old).unwrap(), wire["body"]);
    }
    // The pre-0066 kinds still read as before.
    let old: before_adr_0066::EventBody = serde_json::from_value(json!({"kind": "turn_started"})).unwrap();
    assert_eq!(old, before_adr_0066::EventBody::TurnStarted);

    // `parent` is additive on the wire: absent for the model's own calls, ignored by old clients.
    let own = SessionUpdate::ToolStarted { call_id: "c".into(), name: "Read".into(), arguments: "{}".into(), parent: None };
    assert_eq!(serde_json::to_value(&own).unwrap(), json!({"type": "tool_started", "call_id": "c", "name": "Read", "arguments": "{}"}));
    let nested =
        SessionUpdate::ToolFinished { call_id: "n".into(), name: "Read".into(), result: ToolResult::text("x"), parent: Some("c".into()) };
    let wire = serde_json::to_value(&nested).unwrap();
    assert_eq!(wire["parent"], "c");
    assert_eq!(serde_json::from_value::<SessionUpdate>(wire.clone()).unwrap(), nested);
    let old: before_adr_0066::SessionUpdate = serde_json::from_value(wire).unwrap();
    assert!(matches!(old, before_adr_0066::SessionUpdate::ToolFinished { call_id, .. } if call_id == "n"));
}
