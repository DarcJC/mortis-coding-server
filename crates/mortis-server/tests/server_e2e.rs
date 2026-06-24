//! End-to-end tests driving the full assembled app (auth + REST handlers +
//! services + real gix/grep/session backends) via `tower::oneshot`, plus a
//! light MCP-endpoint auth check.

use std::process::Command;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use tower::ServiceExt;

use mortis_core::config::{RepoConfig, VcsKind};
use mortis_core::RepoId;
use mortis_server::config::{AuthConfig, Config, ServerConfig, SessionConfig, TokenEntry};
use mortis_server::{build_app, build_services};

fn u(p: &std::path::Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
}

fn git(dir: &Utf8Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {args:?} failed");
}

/// A tiny client over the assembled `Router`.
struct Client {
    app: Router,
}

impl Client {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Body,
        content_type: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        let req = builder.body(body).unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    async fn get_json(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        let (s, b) = self.send("GET", uri, Some(token), Body::empty(), None).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn post_json(&self, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
        let (s, b) = self
            .send("POST", uri, Some(token), Body::from(body.to_string()), Some("application/json"))
            .await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// Send a JSON-RPC message to the MCP endpoint (stateless JSON mode).
    async fn mcp(&self, token: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1") // rmcp validates the Host header
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// Create a two-commit fixture git repo under `remote`.
fn make_fixture(remote: &Utf8Path) {
    std::fs::create_dir_all(remote.join("src")).unwrap();
    git(remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(remote.join("README.md"), "# proj\n").unwrap();
    git(remote, &["add", "."]);
    git(remote, &["commit", "-qm", "c1"]);
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    git(remote, &["add", "."]);
    git(remote, &["commit", "-qm", "c2"]);
}

fn config_for(root: &Utf8Path, remote: &Utf8Path) -> Config {
    let url = format!("file:///{}", remote.as_str().replace('\\', "/"));
    Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: root.join("data"),
            svn_bin: None,
        },
        auth: AuthConfig {
            tokens: vec![
                TokenEntry { token: "alice-tok".into(), principal: "alice".into() },
                TokenEntry { token: "bob-tok".into(), principal: "bob".into() },
            ],
        },
        repos: vec![RepoConfig {
            id: RepoId::from("proj"),
            kind: VcsKind::Git,
            url,
            rev: Some("main".into()),
            schedule: None,
            include: vec!["src/**".into(), "*.md".into()],
            exclude: vec![],
            username: None,
            password: None,
        }],
        session: SessionConfig {
            persist: true,
            ttl: "24h".into(),
            reap_interval: "10m".into(),
        },
    }
}

#[tokio::test]
async fn rest_end_to_end_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let root = u(tmp.path());

    // fixture repo with two commits
    let remote = root.join("remote");
    std::fs::create_dir_all(remote.join("src")).unwrap();
    git(&remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(remote.join("README.md"), "# proj\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "c1"]);
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "c2"]);

    let (state, _services) = build_services(config_for(&root, &remote)).unwrap();
    let client = Client { app: build_app(state) };

    // ---- health is public ----
    let (s, body) = client.send("GET", "/health", None, Body::empty(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, b"ok");

    // ---- auth required ----
    let (s, _) = client.send("GET", "/api/v1/repos", None, Body::empty(), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = client.send("GET", "/api/v1/repos", Some("wrong"), Body::empty(), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // ---- sync, then list shows a head ----
    let (s, _) = client.post_json("/api/v1/repos/proj/sync", "alice-tok", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, repos) = client.get_json("/api/v1/repos", "alice-tok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(repos[0]["id"], "proj");
    assert!(repos[0]["head"].is_string());

    // ---- search ----
    let (s, hits) = client
        .post_json("/api/v1/search", "alice-tok", json!({"pattern": "fn c", "repo": "proj"}))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["path"], "src/a.rs");

    // ---- read with a line range ----
    let (s, fc) = client
        .get_json("/api/v1/repos/proj/file?path=src/a.rs&start=1&end=1", "alice-tok")
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(fc["text"], "fn a() {}");
    assert_eq!(fc["total_lines"], 3);

    // ---- blame + history ----
    let (s, blame) = client.get_json("/api/v1/repos/proj/blame?path=src/a.rs", "alice-tok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blame.as_array().unwrap().len(), 3);
    let (s, hist) = client.get_json("/api/v1/repos/proj/history", "alice-tok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hist.as_array().unwrap().len(), 2);
    assert_eq!(hist[0]["summary"], "c2");

    // ---- session: create, write, status, diff, patch ----
    let (s, sess) = client.post_json("/api/v1/sessions", "alice-tok", json!({"repo": "proj"})).await;
    assert_eq!(s, StatusCode::OK);
    let sid = sess["id"].as_str().unwrap().to_string();

    let (s, _) = client
        .send(
            "PUT",
            &format!("/api/v1/sessions/{sid}/file?path=src/a.rs"),
            Some("alice-tok"),
            Body::from("fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n"),
            Some("application/octet-stream"),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    let (s, status) = client.get_json(&format!("/api/v1/sessions/{sid}/status"), "alice-tok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(status[0]["path"], "src/a.rs");
    assert_eq!(status[0]["change"], "modified");

    // session read reflects the overlay write, base is untouched
    let (s, fc) = client
        .get_json(&format!("/api/v1/sessions/{sid}/file?path=src/a.rs"), "alice-tok")
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(fc["total_lines"], 4);

    let (s, diff_bytes) = client
        .send("GET", &format!("/api/v1/sessions/{sid}/diff"), Some("alice-tok"), Body::empty(), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    let diff = String::from_utf8(diff_bytes).unwrap();
    assert!(diff.contains("diff --git"), "diff was: {diff}");
    assert!(diff.contains("fn d()"));

    let (s, patch_bytes) = client
        .send("GET", &format!("/api/v1/sessions/{sid}/patch"), Some("alice-tok"), Body::empty(), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(String::from_utf8(patch_bytes).unwrap().contains("diff --git"));

    // ---- owner isolation: bob cannot see or touch alice's session ----
    let (s, bob_sessions) = client.get_json("/api/v1/sessions", "bob-tok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bob_sessions.as_array().unwrap().len(), 0);
    let (s, _) = client.get_json(&format!("/api/v1/sessions/{sid}"), "bob-tok").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mcp_endpoint_requires_auth() {
    let tmp = tempfile::tempdir().unwrap();
    let root = u(tmp.path());
    let remote = root.join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("README.md"), "# x\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "c1"]);

    let (state, _services) = build_services(config_for(&root, &remote)).unwrap();
    let client = Client { app: build_app(state) };

    // An MCP initialize without a bearer token must be rejected by the auth layer.
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.0.0" }
        }
    });
    let (s, _) = client
        .send("POST", "/mcp", None, Body::from(init.to_string()), Some("application/json"))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mcp_tools_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = u(tmp.path());
    let remote = root.join("remote");
    make_fixture(&remote);

    let (state, services) = build_services(config_for(&root, &remote)).unwrap();
    services.sync_repo(&RepoId::from("proj")).await.unwrap();
    let client = Client { app: build_app(state) };

    // initialize advertises our server and the tools capability
    let (s, init) = client
        .mcp(
            "alice-tok",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                   "params":{"protocolVersion":"2025-06-18","capabilities":{},
                             "clientInfo":{"name":"t","version":"0"}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(init["result"]["serverInfo"]["name"], "mortis-code-server");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // tools/list returns the full tool set
    let (s, list) = client
        .mcp("alice-tok", json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await;
    assert_eq!(s, StatusCode::OK);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"search_code"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"create_session"));
    assert!(names.len() >= 12, "expected the full tool set, got {names:?}");

    // tools/call search_code returns JSON text content with the match
    let (s, call) = client
        .mcp(
            "alice-tok",
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                   "params":{"name":"search_code","arguments":{"pattern":"fn c","repo":"proj"}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    let hits: Value = serde_json::from_str(text).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["path"], "src/a.rs");

    // create_session via MCP picks up the principal from the bearer token
    let (s, sess) = client
        .mcp(
            "alice-tok",
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
                   "params":{"name":"create_session","arguments":{"repo":"proj"}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let sess_text = sess["result"]["content"][0]["text"].as_str().unwrap();
    let sess_obj: Value = serde_json::from_str(sess_text).unwrap();
    assert_eq!(sess_obj["owner"], "alice");
}
