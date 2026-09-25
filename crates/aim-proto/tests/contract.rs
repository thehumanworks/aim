//! Contract tests for the wire types. The error-code table is checked exhaustively (it is a finite
//! domain, so this is a proof by enumeration); envelopes and content are checked by round trip.

use aim_proto::content::{Base64Bytes, Content};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{self, ExactEdit, FsEditParams, Precondition};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use aim_proto::rpc::{Envelope, ErrorObject, Message, Method, Notification, RequestId};
use serde_json::json;

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
