use std::collections::BTreeSet;
use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};

use super::{
    ConfiguredRemote, GrantSnapshot, ProgramError, ProgramLanguage, ProgramManifest, ProgramProvenance, ProgramScope, ProgramStore,
    SavedProgram,
};
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn manifest() -> ProgramManifest {
    ProgramManifest {
        id: "01K00000000000000000000000".into(),
        name: "Find files".into(),
        description: "Find relevant files".into(),
        language: ProgramLanguage::TypeScript,
        runtime_version: "1".into(),
        params: json!({"type":"object","properties":{"pattern":{"type":"string"}}}),
        returns: json!({"type":"array","items":{"type":"string"}}),
        tools: names(&["search", "read", "write"]),
        grants: GrantSnapshot { tools: names(&["search", "read"]) },
        provenance: ProgramProvenance { session_id: "session".into(), turn: 2 },
        version: "1.0.0".into(),
        tags: vec!["files".into()],
    }
}

#[test]
fn manifest_accepts_natural_json_schemas_from_tool_arguments() {
    let mut input = serde_json::to_value(manifest()).unwrap();
    input["params"] = json!({"type":"object"});
    input["returns"] = json!({"type":"null"});
    let parsed: ProgramManifest = serde_json::from_value(input).unwrap();
    assert_eq!(parsed.params, json!({"type":"object"}));
    assert_eq!(parsed.returns, json!({"type":"null"}));
}

fn store() -> (tempfile::TempDir, ProgramStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = ProgramStore::new(directory.path().join("user/programs"));
    (directory, store)
}

#[test]
fn git_save_load_and_content_trust() {
    let (_directory, store) = store();
    let first =
        store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main(args) { return args; }").unwrap();
    assert!(first.trusted);
    assert_eq!(store.load(ProgramScope::User, "find-files").unwrap().sha256, first.sha256);
    assert!(first.readme.contains("name: \"find-files\""));
    let root = store.root(ProgramScope::User).unwrap();
    assert_eq!(super::git_output(root, &["rev-list", "--count", "HEAD"], "rev-list").unwrap(), "1");

    let source = root.join("find-files/main.ts");
    std::fs::write(source, "export default async function main() { return 1; }").unwrap();
    let changed = store.load(ProgramScope::User, "find-files").unwrap();
    assert!(!changed.trusted);
    assert!(changed.effective_tools(&names(&["search", "read"])).is_empty());
    assert!(store.trust(ProgramScope::User, "find-files").unwrap().trusted);

    std::fs::write(root.join("find-files/README.md"), "changed retrieval text").unwrap();
    assert!(!store.load(ProgramScope::User, "find-files").unwrap().trusted);
}

#[test]
fn second_save_is_one_more_commit_and_project_programs_never_touch_the_daemons_disk() {
    let (directory, store) = store();
    store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 1; }").unwrap();
    store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 2; }").unwrap();
    // Project programs are read and written through the workspace (ADR 0066, REV13a L5).
    let project = store.save(ProgramScope::Project, "find-files", &manifest(), "export default async function main() { return 3; }");
    assert!(matches!(project, Err(ProgramError::Invalid(_))));
    assert!(!directory.path().join("project").exists());
    let user = store.root(ProgramScope::User).unwrap();
    assert_eq!(super::git_output(user, &["rev-list", "--count", "HEAD"], "rev-list").unwrap(), "2");
    let all = store.list().unwrap().programs;
    assert_eq!(all.len(), 1);
    assert_eq!(all.first().unwrap().scope, ProgramScope::User);
}

#[test]
fn one_malformed_program_does_not_hide_the_others() {
    let (_directory, store) = store();
    store.save(ProgramScope::User, "good", &manifest(), "export default async function main() { return 1; }").unwrap();
    store.save(ProgramScope::User, "broken", &manifest(), "export default async function main() { return 2; }").unwrap();
    let root = store.root(ProgramScope::User).unwrap();
    std::fs::write(root.join("broken/program.toml"), "not = [valid").unwrap();
    let listing = store.list().unwrap();
    assert_eq!(listing.programs.iter().map(|program| program.slug.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(listing.problems.len(), 1);
    assert!(listing.problems.first().unwrap().contains("broken"), "{:?}", listing.problems);
}

#[test]
fn repeated_identical_save_still_records_a_save_commit() {
    let (_directory, store) = store();
    let entry = manifest();
    let source = "export default async function main() { return 1; }";
    store.save(ProgramScope::User, "find-files", &entry, source).unwrap();
    store.save(ProgramScope::User, "find-files", &entry, source).unwrap();
    let root = store.root(ProgramScope::User).unwrap();
    assert_eq!(super::git_output(root, &["rev-list", "--count", "HEAD"], "rev-list").unwrap(), "2");
}

#[test]
fn save_does_not_commit_unrelated_staged_files() {
    let (_directory, store) = store();
    store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 1; }").unwrap();
    let root = store.root(ProgramScope::User).unwrap();
    std::fs::write(root.join("unrelated.txt"), "keep staged").unwrap();
    super::git(root, &["add", "--", "unrelated.txt"], "add").unwrap();
    store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 2; }").unwrap();
    let committed = super::git_output(root, &["show", "--pretty=format:", "--name-only", "HEAD"], "show").unwrap();
    assert!(!committed.contains("unrelated.txt"));
    let staged = super::git_output(root, &["diff", "--cached", "--name-only"], "diff").unwrap();
    assert_eq!(staged, "unrelated.txt");
}

#[test]
fn concurrent_saves_keep_all_trust_hashes_and_commits() {
    let (_directory, store) = store();
    let store = Arc::new(store);
    let handles: Vec<_> = (0..4)
        .map(|number| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let slug = format!("program-{number}");
                let source = format!("export default async function main() {{ return {number}; }}");
                let mut entry = manifest();
                entry.id = format!("id-{number}");
                store.save(ProgramScope::User, &slug, &entry, &source)
            })
        })
        .collect();
    for handle in handles {
        assert!(handle.join().unwrap().unwrap().trusted);
    }
    let programs = store.list().unwrap().programs;
    assert_eq!(programs.len(), 4);
    assert!(programs.iter().all(|program| program.trusted));
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_in_program_and_repository_paths() {
    use std::os::unix::fs::symlink;

    let (directory, store) = store();
    let user_root = store.root(ProgramScope::User).unwrap();
    std::fs::create_dir_all(user_root).unwrap();
    symlink(directory.path(), user_root.join("find-files")).unwrap();
    assert!(matches!(store.save(ProgramScope::User, "find-files", &manifest(), "source"), Err(ProgramError::Invalid(_))));
    std::fs::remove_file(user_root.join("find-files")).unwrap();

    std::fs::create_dir_all(user_root.join("find-files")).unwrap();
    symlink(directory.path().join("outside"), user_root.join("find-files/main.ts")).unwrap();
    assert!(matches!(store.save(ProgramScope::User, "find-files", &manifest(), "source"), Err(ProgramError::Invalid(_))));
}

#[test]
fn grant_intersection_never_adds_tools() {
    let (_directory, store) = store();
    let saved = store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 1; }").unwrap();
    assert_eq!(saved.effective_tools(&names(&["search", "write", "extra"])), names(&["search"]));
    let mut without_reference = saved.clone();
    without_reference.manifest.tools.remove("search");
    assert!(without_reference.effective_tools(&names(&["search", "write", "extra"])).is_empty());
}

#[test]
fn rejects_path_escape_and_unconfigured_remote() {
    let (_directory, store) = store();
    assert!(matches!(store.save(ProgramScope::User, "../escape", &manifest(), ""), Err(ProgramError::Invalid(_))));
    assert!(matches!(
        store.sync_user(&ConfiguredRemote { url: "https://someone:token@github.com/x/y.git".into(), branch: "main".into() }),
        Err(ProgramError::Invalid(_))
    ));
    assert!(!store.root(ProgramScope::User).unwrap().exists());
}

struct FakeHost;

impl ToolHost for FakeHost {
    fn specs(&self) -> Vec<ToolSpec> {
        ["search", "read", "write", "extra"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: name.into(),
                input_schema: json!({"type":"object"}),
                input: ToolInput::Json,
                annotations: ToolAnnotations::default(),
            })
            .collect()
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async { Ok(ToolResult::default()) })
    }
}

#[tokio::test]
async fn host_rejects_unadvertised_and_ungranted_calls() {
    let (_directory, store) = store();
    let saved: SavedProgram =
        store.save(ProgramScope::User, "find-files", &manifest(), "export default async function main() { return 1; }").unwrap();
    let host = saved.narrow_host(Arc::new(FakeHost));
    assert_eq!(host.specs().into_iter().map(|spec| spec.name).collect::<Vec<_>>(), vec!["search", "read"]);
    assert!(host.call("search".into(), json!({}), IdempotencyKey::new("one")).await.is_ok());
    let denied = host.call("write".into(), json!({}), IdempotencyKey::new("two")).await.unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied);
    let denied = host.call("extra".into(), json!({}), IdempotencyKey::new("three")).await.unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied);
}
