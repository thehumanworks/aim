//! Component boundary tests using the checked-in SDK example components.
#![expect(clippy::unwrap_used, reason = "test failures should stop at the failed fixture or assertion")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aim_plugin::{BoxFuture, PluginDelegate, PluginSource, PluginToolHost, TrustStore};
use aim_proto::error::ProtoError;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use serde_json::{Value, json};

#[derive(Default)]
struct Delegate {
    calls: Mutex<Vec<(String, IdempotencyKey)>>,
}

impl PluginDelegate for Delegate {
    fn call(&self, name: String, _arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.calls.lock().unwrap().push((name, key));
        Box::pin(async { Ok(ToolResult::text("delegated")) })
    }
}

fn examples() -> PathBuf {
    std::env::var_os("AIM_PLUGIN_EXAMPLES")
        .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples"), PathBuf::from)
}

fn source(name: &str, project: bool) -> PluginSource {
    let base = examples();
    let manifest_text = std::fs::read_to_string(base.join(name).join("aim-plugin.toml")).unwrap();
    let path = match name {
        "kv_counter" => "aim_example_kv_counter.wasm",
        _ => "aim_example_delegate_read.wasm",
    };
    let component = std::fs::read(base.join("build").join(path)).unwrap();
    PluginSource { manifest_text, component, project }
}

fn key() -> IdempotencyKey {
    IdempotencyKey::new("outer-key")
}

#[tokio::test]
async fn project_source_requires_exact_hash_and_edited_bytes_lose_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut trust = TrustStore::load(dir.path()).unwrap();
    let src = source("kv_counter", true);
    let delegate = Arc::new(Delegate::default());
    let host = PluginToolHost::load(&trust, vec![src.clone()], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::new()).unwrap();
    assert!(host.specs().is_empty());
    trust.grant(&src.hash(), ["tools.provide".into(), "kv".into()]).unwrap();
    let host = PluginToolHost::load(&trust, vec![src.clone()], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::new()).unwrap();
    assert_eq!(host.specs().len(), 1);
    let mut edited = src;
    edited.component.push(0);
    let host = PluginToolHost::load(&trust, vec![edited], delegate, HashSet::new()).unwrap();
    assert!(host.specs().is_empty());
}

#[tokio::test]
async fn kv_guest_persists_across_session_host_recreation() {
    let dir = tempfile::tempdir().unwrap();
    let mut trust = TrustStore::load(dir.path()).unwrap();
    let src = source("kv_counter", false);
    trust.grant(&src.hash(), ["tools.provide".into(), "kv".into()]).unwrap();
    let delegate = Arc::new(Delegate::default());
    let name = "plugin__kv_counter__increment".to_owned();
    let host = PluginToolHost::load(&trust, vec![src.clone()], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::new()).unwrap();
    let first = host.call(name.clone(), json!({"key":"a"}), key()).await.unwrap();
    assert_eq!(first, ToolResult::text("a: 1"));
    let host = PluginToolHost::load(&trust, vec![src], delegate, HashSet::new()).unwrap();
    let second = host.call(name, json!({"key":"a"}), key()).await.unwrap();
    assert_eq!(second, ToolResult::text("a: 2"));
}

#[tokio::test]
async fn delegated_tool_needs_both_hash_grant_and_agent_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let mut trust = TrustStore::load(dir.path()).unwrap();
    let src = source("delegate_read", false);
    trust.grant(&src.hash(), ["tools.provide".into()]).unwrap();
    let delegate = Arc::new(Delegate::default());
    let name = "plugin__delegate_read__delegate_read".to_owned();
    let args = json!({"file_path":"note.txt"});
    let host =
        PluginToolHost::load(&trust, vec![src.clone()], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::from(["read".into()]))
            .unwrap();
    assert!(host.call(name.clone(), args.clone(), key()).await.is_err());
    assert!(delegate.calls.lock().unwrap().is_empty());
    trust.grant(&src.hash(), ["tools.provide".into(), "tools.call:read".into()]).unwrap();
    let host = PluginToolHost::load(&trust, vec![src.clone()], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::new()).unwrap();
    assert!(host.call(name.clone(), args.clone(), key()).await.is_err());
    assert!(delegate.calls.lock().unwrap().is_empty());
    let host =
        PluginToolHost::load(&trust, vec![src], Arc::clone(&delegate) as Arc<dyn PluginDelegate>, HashSet::from(["read".into()])).unwrap();
    assert_eq!(host.call(name, args, key()).await.unwrap(), ToolResult::text("delegated"));
    let calls = delegate.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "read");
    assert_eq!(calls[0].1.as_str(), "outer-key:plugin:0");
}
