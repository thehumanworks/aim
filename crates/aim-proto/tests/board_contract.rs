//! Board wire names, schemas, and stable tagged payloads.
use aim_proto::board::{self, BoardEvent, JobSpec};
use aim_proto::rpc::{Method, Notification};
use serde_json::json;

#[test]
fn every_board_method_has_unique_name_and_both_schemas() {
    fn schema<M: Method>() -> (&'static str, serde_json::Value, serde_json::Value) {
        (
            M::NAME,
            serde_json::to_value(schemars::schema_for!(M::Params)).unwrap(),
            serde_json::to_value(schemars::schema_for!(M::Result)).unwrap(),
        )
    }
    let all = [
        schema::<board::BoardPost>(),
        schema::<board::BoardList>(),
        schema::<board::BoardShow>(),
        schema::<board::BoardAssign>(),
        schema::<board::BoardRegisterWorker>(),
        schema::<board::BoardClaim>(),
        schema::<board::BoardHeartbeat>(),
        schema::<board::BoardMessage>(),
        schema::<board::BoardComplete>(),
        schema::<board::BoardFail>(),
        schema::<board::BoardConfirmCleanup>(),
        schema::<board::BoardExpire>(),
        schema::<board::BoardCancel>(),
        schema::<board::BoardRetry>(),
        schema::<board::BoardReview>(),
        schema::<board::BoardPoll>(),
        schema::<board::BoardWatch>(),
    ];
    let mut names: Vec<_> = all.iter().map(|(name, _, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len());
    for (name, params, result) in all {
        assert!(name.starts_with("board."));
        assert!(params.is_object(), "{name} params");
        assert!(result.is_object(), "{name} result");
    }
    assert_eq!(<board::BoardEventNotification as Notification>::NAME, "board.event");
    assert!(serde_json::to_value(schemars::schema_for!(board::BoardEventParams)).unwrap().is_object());
}

#[test]
fn post_contract_and_event_round_trip() {
    let spec = JobSpec {
        title: "Review parser".into(),
        deliverable: "Find reproducible bugs".into(),
        acceptance: vec!["Every finding cites a source line".into()],
        depends_on: vec!["job-a".into()],
        max_retries: 2,
        workspace: None,
        work: None,
    };
    let wire = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        wire,
        json!({
            "title": "Review parser",
            "deliverable": "Find reproducible bugs",
            "acceptance": ["Every finding cites a source line"],
            "depends_on": ["job-a"],
            "max_retries": 2
        })
    );
    assert_eq!(serde_json::from_value::<JobSpec>(wire).unwrap(), spec);

    let event = BoardEvent {
        id: "event-1".into(),
        run_id: "run-1".into(),
        job_id: "job-a".into(),
        seq: 3,
        version: 2,
        kind: "job.claimed".into(),
        ts_ms: 5,
    };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(serde_json::from_value::<BoardEvent>(wire.clone()).unwrap(), event);
    assert!(wire.get("claim_token").is_none(), "event hints must not carry claim secrets");
}
