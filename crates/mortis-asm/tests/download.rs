//! End-to-end tests for the download → validate → query lifecycle, driven
//! against a local HTTP server.

use std::time::Duration;

use camino::Utf8PathBuf;
use mortis_asm::MemAssemblyStore;
use mortis_core::CoreError;
use mortis_core::asm::{AsmDownloadPolicy, AsmSessionId, AsmStatus, AssemblyStore, BinaryOs};
use mortis_core::model::Principal;
use object::write::{Object, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

const X64_CODE: [u8; 5] = [0x55, 0x48, 0x89, 0xe5, 0xc3];

/// A valid little ELF (relocatable, with a `.text` section + function symbol).
fn elf_fixture() -> Vec<u8> {
    let mut obj = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    obj.append_section_data(text, &X64_CODE, 1);
    obj.add_symbol(Symbol {
        name: b"f".to_vec(),
        value: 0,
        size: X64_CODE.len() as u64,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    obj.write().unwrap()
}

/// Spawn a local test server, returning its base URL (`http://127.0.0.1:<port>`).
async fn spawn_server() -> String {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    let elf_bin = elf_fixture();
    let elf_slow = elf_bin.clone();

    let app = Router::new()
        .route(
            "/bin",
            get(move || {
                let b = elf_bin.clone();
                async move { b }
            }),
        )
        .route(
            "/slow",
            get(move || {
                let b = elf_slow.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    b
                }
            }),
        )
        .route("/big", get(|| async { vec![0u8; 20_000] }))
        .route("/text", get(|| async { "definitely not a binary".to_string() }))
        .route("/notfound", get(|| async { StatusCode::NOT_FOUND }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn make_store(dir: Utf8PathBuf, hosts: &[&str], max_bytes: u64) -> MemAssemblyStore {
    MemAssemblyStore::new(
        dir,
        AsmDownloadPolicy {
            allowed_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            max_download_bytes: max_bytes,
            timeout: Duration::from_secs(10),
            max_sessions: 16,
        },
    )
    .unwrap()
}

fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = Utf8PathBuf::from_path_buf(t.path().join("asm")).unwrap();
    (t, p)
}

async fn wait_terminal(store: &MemAssemblyStore, id: &AsmSessionId) -> AsmStatus {
    for _ in 0..200 {
        let s = store.get(id).await.unwrap();
        if matches!(s.status, AsmStatus::Ready { .. } | AsmStatus::Failed { .. }) {
            return s.status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session never reached a terminal state");
}

#[tokio::test]
async fn download_then_ready_and_query() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    let p = Principal::from("alice");

    let s = store.create(&p, &format!("{base}/bin")).await.unwrap();
    assert!(matches!(s.status, AsmStatus::Downloading { .. }));

    let info = match wait_terminal(&store, &s.id).await {
        AsmStatus::Ready { info } => info,
        other => panic!("expected ready, got {other:?}"),
    };
    assert_eq!(info.os, BinaryOs::Linux);
    assert_eq!(info.arch, "x86_64");

    // Full query path through the store.
    let dis = store.disassemble(&s.id, 0, 5).await.unwrap();
    assert_eq!(dis.instructions[0].mnemonic, "push");
    let r = store.resolve_function(&s.id, 0).await.unwrap();
    assert_eq!(r.name.as_deref(), Some("f"));
    let meta = store.metadata(&s.id).await.unwrap();
    assert_eq!(meta.arch, "x86_64");
}

#[tokio::test]
async fn empty_allowlist_denies() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &[], 1 << 20);
    let err = store
        .create(&Principal::from("a"), &format!("{base}/bin"))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
async fn host_not_allowed_denies() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["example.com"], 1 << 20);
    let err = store
        .create(&Principal::from("a"), &format!("{base}/bin"))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Forbidden(_)), "got {err:?}");
}

#[tokio::test]
async fn non_http_scheme_is_invalid() {
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    let err = store
        .create(&Principal::from("a"), "file:///etc/passwd")
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidInput(_)), "got {err:?}");
}

#[tokio::test]
async fn oversize_download_fails() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    // 1 KiB cap; `/big` serves 20 KB.
    let store = make_store(dir, &["127.0.0.1"], 1024);
    let s = store
        .create(&Principal::from("a"), &format!("{base}/big"))
        .await
        .unwrap();
    match wait_terminal(&store, &s.id).await {
        AsmStatus::Failed { code, .. } => assert_eq!(code, "too_large"),
        other => panic!("expected failed, got {other:?}"),
    }
}

#[tokio::test]
async fn validation_failure_reports_invalid_binary() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    let s = store
        .create(&Principal::from("a"), &format!("{base}/text"))
        .await
        .unwrap();
    match wait_terminal(&store, &s.id).await {
        AsmStatus::Failed { code, .. } => assert_eq!(code, "invalid_binary"),
        other => panic!("expected failed, got {other:?}"),
    }
}

#[tokio::test]
async fn server_error_reports_failed() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    let s = store
        .create(&Principal::from("a"), &format!("{base}/notfound"))
        .await
        .unwrap();
    assert!(matches!(
        wait_terminal(&store, &s.id).await,
        AsmStatus::Failed { .. }
    ));
}

#[tokio::test]
async fn query_before_ready_is_conflict() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    // `/slow` delays its response, so the session is still downloading here.
    let s = store
        .create(&Principal::from("a"), &format!("{base}/slow"))
        .await
        .unwrap();
    let err = store.disassemble(&s.id, 0, 4).await.unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn reap_expired_removes_sessions() {
    let base = spawn_server().await;
    let (_t, dir) = tmp();
    let store = make_store(dir, &["127.0.0.1"], 1 << 20);
    let s = store
        .create(&Principal::from("a"), &format!("{base}/bin"))
        .await
        .unwrap();
    let _ = wait_terminal(&store, &s.id).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let n = store.reap_expired(Duration::ZERO).await.unwrap();
    assert_eq!(n, 1);
    assert!(matches!(store.get(&s.id).await, Err(CoreError::NotFound(_))));
}
