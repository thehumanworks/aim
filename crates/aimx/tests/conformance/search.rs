//! `search.grep` and `search.glob`: ripgrep semantics, `.gitignore`, globs, context, bounds.

use aim_proto::error::ErrorCode;
use aim_proto::harness::{CaseMode, Glob, GlobParams, Grep, GrepParams, GrepResult};
use aim_proto::ids::WorkspaceId;

use crate::common::{Client, Env, env, session};

fn tree(env: &Env) {
    let files = [
        (".gitignore", "target/\n*.log\n"),
        ("src/main.rs", "fn main() {\n    println!(\"Hello\");\n}\n"),
        ("src/lib.rs", "// hello from lib\npub fn hello() {}\n"),
        ("src/deep/mod.rs", "one\ntwo\nhello three\nfour\nfive\n"),
        ("target/debug/out.rs", "hello from build output\n"),
        ("run.log", "hello log\n"),
        ("notes.txt", "HELLO notes\n"),
        (".hidden/secret.rs", "hello hidden\n"),
        ("bin.dat", "hello\0binary"),
    ];
    for (path, body) in files {
        let path = env.path(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

fn grep(ws: &WorkspaceId, pattern: &str) -> GrepParams {
    GrepParams {
        workspace: ws.clone(),
        pattern: pattern.into(),
        path: None,
        globs: vec![],
        case: CaseMode::Smart,
        fixed_strings: false,
        context: 0,
        max_matches: None,
    }
}

async fn run(client: &Client, params: GrepParams) -> GrepResult {
    client.peer.call::<Grep>(params).await.unwrap()
}

fn paths(result: &GrepResult) -> Vec<&str> {
    let mut out: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
    out.dedup();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_respects_gitignore_hidden_and_binary() {
    let env = env().await;
    tree(&env);
    let (client, _, ws) = session(&env).await;
    let found = run(&client, grep(&ws, "hello")).await;
    // Smart case: lower-case pattern matches `Hello` and `HELLO` too. Ignored, hidden and binary
    // files are skipped; paths are relative to the root and sorted.
    assert_eq!(paths(&found), ["notes.txt", "src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);
    assert!(!found.truncated);
    let main = found.matches.iter().find(|m| m.path == "src/main.rs").unwrap();
    assert_eq!(main.line, 2);
    assert_eq!(main.text, "    println!(\"Hello\");");

    let mut sensitive = grep(&ws, "hello");
    sensitive.case = CaseMode::Sensitive;
    assert_eq!(paths(&run(&client, sensitive).await), ["src/deep/mod.rs", "src/lib.rs"]);
    let mut insensitive = grep(&ws, "HELLO");
    insensitive.case = CaseMode::Insensitive;
    assert_eq!(run(&client, insensitive).await.matches.len(), 5);
    // Smart case with an upper-case letter is sensitive.
    assert_eq!(paths(&run(&client, grep(&ws, "HELLO")).await), ["notes.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_globs_paths_context_and_limits() {
    let env = env().await;
    tree(&env);
    let (client, _, ws) = session(&env).await;

    let mut rs = grep(&ws, "hello");
    rs.globs = vec!["*.rs".into()];
    assert_eq!(paths(&run(&client, rs).await), ["src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);

    let mut sub = grep(&ws, "hello");
    sub.path = Some("src/deep".into());
    assert_eq!(paths(&run(&client, sub).await), ["src/deep/mod.rs"]);

    let mut ctx = grep(&ws, "three");
    ctx.context = 2;
    let found = run(&client, ctx).await;
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches[0].before, ["one", "two"]);
    assert_eq!(found.matches[0].after, ["four", "five"]);

    let mut limited = grep(&ws, "hello");
    limited.max_matches = Some(2);
    let found = run(&client, limited).await;
    assert_eq!(found.matches.len(), 2);
    assert!(found.truncated);

    let mut fixed = grep(&ws, "println!(");
    fixed.fixed_strings = true;
    assert_eq!(run(&client, fixed).await.matches.len(), 1);
    let err = client.peer.call::<Grep>(grep(&ws, "(unclosed")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    let mut missing = grep(&ws, "x");
    missing.path = Some("nope".into());
    assert_eq!(client.peer.call::<Grep>(missing).await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn glob_respects_gitignore_and_is_sorted() {
    let env = env().await;
    tree(&env);
    let (client, _, ws) = session(&env).await;
    let glob = |patterns: &[&str], path: Option<&str>, max: Option<u32>| GlobParams {
        workspace: ws.clone(),
        patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
        path: path.map(str::to_owned),
        max_results: max,
    };
    let found = client.peer.call::<Glob>(glob(&["**/*.rs"], None, None)).await.unwrap();
    assert_eq!(found.paths, ["src/deep/mod.rs", "src/lib.rs", "src/main.rs"]);
    assert!(!found.truncated);
    // `*` does not cross directories.
    let top = client.peer.call::<Glob>(glob(&["*"], None, None)).await.unwrap();
    assert_eq!(top.paths, ["bin.dat", "notes.txt"]);
    // Relative to the search path, reported relative to the root.
    let sub = client.peer.call::<Glob>(glob(&["*.rs"], Some("src"), None)).await.unwrap();
    assert_eq!(sub.paths, ["src/lib.rs", "src/main.rs"]);
    // Naming a dot-directory finds it.
    let hidden = client.peer.call::<Glob>(glob(&[".hidden/*"], None, None)).await.unwrap();
    assert_eq!(hidden.paths, [".hidden/secret.rs"]);
    let limited = client.peer.call::<Glob>(glob(&["**/*"], None, Some(2))).await.unwrap();
    assert_eq!(limited.paths.len(), 2);
    assert!(limited.truncated);
    let err = client.peer.call::<Glob>(glob(&["a[b"], None, None)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
}
