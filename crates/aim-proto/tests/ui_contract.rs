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
