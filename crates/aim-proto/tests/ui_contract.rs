//! Contract tests of the UI protocol (ADR 0064, ADR 0017 Verification): envelopes and events
//! round-trip, the fold is shared, schemas exist, and the A2UI adapter preserves aim's messages.
#![expect(clippy::unwrap_used, clippy::indexing_slicing, reason = "fixture helpers read a known file")]

use aim_proto::daemon::{SessionAttachPagedResult, SessionAttachResult, SessionUpdate};
use aim_proto::event::EventBody;
use aim_proto::ui::catalog::TERMINAL;
use aim_proto::ui::model::{Change, Surfaces};
use aim_proto::ui::{Fallback, Placement, UiEnvelope, UiMessage, a2ui};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/ui_surface.json")).unwrap()
}

fn messages() -> Vec<UiEnvelope> {
    fixture()["messages"].as_array().unwrap().iter().map(|m| serde_json::from_value(m.clone()).unwrap()).collect()
}

#[test]
fn ui_surface_roundtrip() {
    let fixture = fixture();
    for (wire, envelope) in fixture["messages"].as_array().unwrap().iter().zip(messages()) {
        assert_eq!(&serde_json::to_value(&envelope).unwrap(), wire, "the envelope serializes as written");
        // As a session event and as a live update.
        let event = EventBody::Ui { message: envelope.clone() };
        let stored = serde_json::to_value(&event).unwrap();
        assert_eq!(stored["kind"], "ui");
        assert_eq!(serde_json::from_value::<EventBody>(stored).unwrap(), event);
        let update = SessionUpdate::Ui { message: envelope.clone() };
        let sent = serde_json::to_value(&update).unwrap();
        assert_eq!(sent["type"], "ui");
        assert_eq!(serde_json::from_value::<SessionUpdate>(sent).unwrap(), update);
        // Through A2UI and back.
        let exported = a2ui::export(&envelope);
        let ops: Vec<UiEnvelope> = exported.iter().map(|m| a2ui::import(m).unwrap()).collect();
        match &envelope.message {
            UiMessage::UpdateDataModel { .. } => assert_eq!(ops.len(), 1),
            _ => assert_eq!(ops, vec![envelope.clone()]),
        }
    }
    let mut surfaces = Surfaces::default();
    let changes: Vec<Change> = messages().iter().map(|m| surfaces.apply(&m.message, 3).unwrap()).collect();
    assert_eq!(changes, vec![Change::Created("release".into()), Change::Updated("release".into())]);
    let surface = surfaces.get("release").unwrap();
    assert_eq!((surface.anchor, surface.components.len(), surface.data["done"].clone()), (3, 12, json!(60)));
    assert_eq!(
        surface.component("later").and_then(|c| c.fallback.clone()),
        Some(Fallback::Text("trend: 1 3 2".into())),
        "an unknown component keeps its fallback"
    );
}

#[test]
fn attach_results_carry_surfaces_additively() {
    let mut surfaces = Surfaces::default();
    for message in messages() {
        surfaces.apply(&message.message, 0).unwrap();
    }
    let old = json!({
        "summary": {"meta": {"id": "s", "created_ms": 1, "workspace": "/w", "location": "local", "provider": "p", "model": "m"},
                    "state": "idle", "persistence": "persistent", "last_activity_ms": 1, "turns": 0},
        "transcript": []
    });
    let parsed: SessionAttachResult = serde_json::from_value(old.clone()).unwrap();
    assert!(parsed.surfaces.is_empty(), "a reply without surfaces reads as none");
    assert_eq!(serde_json::to_value(&parsed).unwrap(), old, "and serializes as before");
    let with = SessionAttachResult { surfaces: surfaces.list.clone(), ..parsed };
    let wire = serde_json::to_value(&with).unwrap();
    assert_eq!(serde_json::from_value::<SessionAttachResult>(wire).unwrap(), with);
    let paged = json!({"summary": old["summary"], "snapshot_id": "x", "total_bytes": 2, "first_chunk": "W10="});
    let paged: SessionAttachPagedResult = serde_json::from_value(paged).unwrap();
    assert!(paged.surfaces.is_empty());
}

#[test]
fn ui_schemas_exist_and_name_every_message_and_placement() {
    let schema = serde_json::to_string(&schemars::schema_for!(UiEnvelope)).unwrap();
    for name in ["create_surface", "update_components", "update_data_model", "delete_surface", "a2ui", "fallback"] {
        assert!(schema.contains(name), "the envelope schema names {name}");
    }
    let placement = serde_json::to_value(schemars::schema_for!(Placement)).unwrap();
    let description = placement["description"].as_str().unwrap();
    for name in Placement::NAMES {
        assert!(description.contains(name), "the placement schema names {name}");
    }
    assert!(placement["pattern"].as_str().unwrap().contains("tool\\("));
    let update = serde_json::to_string(&schemars::schema_for!(SessionUpdate)).unwrap();
    assert!(update.contains("\"ui\""), "session updates list the ui variant");
    // Every catalog component has a JSON Schema generated from the catalog data.
    for component in TERMINAL.components {
        let schema = component.json_schema();
        assert_eq!(schema["additionalProperties"], json!(false), "{}", component.name);
    }
}

#[test]
fn unknown_placements_and_versions_are_refused_by_the_types() {
    let bad = json!({"a2ui": "1.0", "type": "create_surface", "surface_id": "s", "placement": "sidebar"});
    assert!(serde_json::from_value::<UiEnvelope>(bad).is_err());
    let defaulted: UiEnvelope = serde_json::from_value(json!({"a2ui": "1.0", "type": "create_surface", "surface_id": "s"})).unwrap();
    let UiMessage::CreateSurface { placement, catalog_id, .. } = defaulted.message else { panic!("create") };
    assert_eq!((placement, catalog_id.as_str()), (Placement::Transcript, "aim/terminal@1"));
}

/// Types exactly as a build without the UI protocol (origin/main before ADR 0064) declares them,
/// minus doc comments: what an old client or an old log reader parses new data with.
mod pre_ui {
    use aim_proto::conversation::{Item, Part, RateLimits, StopReason, Usage};
    use aim_proto::daemon::{SessionState, SessionSummary};
    use aim_proto::event::{DecisionRecord, EffortSource};
    use aim_proto::tool::ToolResult;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum SessionUpdate {
        StateChanged {
            state: SessionState,
        },
        TurnStarted {
            turn: u64,
        },
        RequestStarted {
            index: u32,
        },
        TextDelta {
            delta: String,
        },
        ReasoningDelta {
            delta: String,
        },
        ItemAdded {
            item: Item,
        },
        ToolStarted {
            call_id: String,
            name: String,
            arguments: String,
        },
        ToolFinished {
            call_id: String,
            name: String,
            result: ToolResult,
        },
        SteerQueued,
        SteerDelivered {
            count: usize,
        },
        SteersReturned {
            steers: Vec<Vec<Part>>,
        },
        Usage {
            usage: Usage,
        },
        RateLimits {
            limits: RateLimits,
        },
        Decision {
            decision: DecisionRecord,
        },
        ConfigChanged {
            model: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            effort: Option<String>,
            #[serde(default, skip_serializing_if = "EffortSource::is_explicit")]
            effort_source: EffortSource,
        },
        ConfigRejected {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            model: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            effort: Option<String>,
            message: String,
        },
        Compacted {
            replaced: u32,
            items: Vec<Item>,
            method: String,
            tokens_before: u64,
            tokens_after: u64,
        },
        TurnEnded {
            stop: StopReason,
        },
        TurnFailed {
            message: String,
        },
    }

    #[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
    pub struct SessionUpdateParams {
        pub session: String,
        pub update: SessionUpdate,
    }

    #[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
    pub struct SessionAttachResult {
        pub summary: SessionSummary,
        pub transcript: Vec<Item>,
    }

    #[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum EventBody {
        TurnStarted,
        Item {
            item: Item,
        },
        Usage {
            usage: Usage,
            model: String,
        },
        RateLimits {
            limits: RateLimits,
        },
        Decision {
            decision: DecisionRecord,
        },
        TurnEnded {
            stop: StopReason,
        },
        TurnFailed {
            message: String,
        },
        ConfigChanged {
            model: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            effort: Option<String>,
            #[serde(default, skip_serializing_if = "EffortSource::is_explicit")]
            effort_source: EffortSource,
        },
        Compacted {
            replaced: u32,
            items: Vec<Item>,
        },
        #[serde(untagged)]
        Unknown(Value),
    }
}

/// REV19: an old client against a new daemon. Its notification parser
/// (`daemon/client.rs`: `if let Ok(SessionUpdateParams { .. }) = serde_json::from_value(params)`)
/// refuses a `ui` update, so the update is dropped and the stream goes on: the notifications
/// around it still parse. Old attach replies ignore `surfaces`, and an old log reader keeps a `ui`
/// event verbatim as unknown.
#[test]
fn an_old_client_skips_ui_updates_and_keeps_everything_else() {
    use aim_proto::daemon::{SessionState, SessionUpdateParams};
    let ui = SessionUpdateParams { session: "s".into(), update: SessionUpdate::Ui { message: messages()[0].clone() } };
    let wire = serde_json::to_value(&ui).unwrap();
    assert!(serde_json::from_value::<pre_ui::SessionUpdateParams>(wire).is_err(), "an old client drops the ui update");
    let before = SessionUpdateParams { session: "s".into(), update: SessionUpdate::StateChanged { state: SessionState::Running } };
    let old: pre_ui::SessionUpdateParams = serde_json::from_value(serde_json::to_value(&before).unwrap()).unwrap();
    assert_eq!(old.update, pre_ui::SessionUpdate::StateChanged { state: SessionState::Running }, "other updates still parse");

    let mut surfaces = Surfaces::default();
    for message in messages() {
        surfaces.apply(&message.message, 0).unwrap();
    }
    let summary = json!({"meta": {"id": "s", "created_ms": 1, "workspace": "/w", "location": "local", "provider": "p", "model": "m"},
                         "state": "idle", "persistence": "persistent", "last_activity_ms": 1, "turns": 0});
    let new_reply = SessionAttachResult {
        summary: serde_json::from_value(summary).unwrap(),
        transcript: Vec::new(),
        surfaces: surfaces.list,
        options: None,
    };
    let old: pre_ui::SessionAttachResult = serde_json::from_value(serde_json::to_value(&new_reply).unwrap()).unwrap();
    assert_eq!((old.summary, old.transcript), (new_reply.summary, new_reply.transcript), "surfaces are ignored");

    let event = serde_json::to_value(EventBody::Ui { message: messages()[1].clone() }).unwrap();
    let old: pre_ui::EventBody = serde_json::from_value(event.clone()).unwrap();
    assert_eq!(old, pre_ui::EventBody::Unknown(event.clone()), "an old reader keeps the ui event as unknown");
    assert_eq!(serde_json::to_value(&old).unwrap(), event, "and writes it back verbatim");
}
