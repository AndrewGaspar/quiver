//! `quiver_api::epic::webview::run` against stand-in helpers (shell scripts answering as
//! `quiver-webview` does). The real helper is tested in crates/quiver-webview/tests.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quiver_api::epic::webview::{Outcome, run};

fn fake(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test]
async fn exit_codes_map_to_outcomes() {
    let d = tempfile::tempdir().unwrap();
    let t = Duration::from_secs(10);
    let code = run(&fake(d.path(), "ok", "echo 0123456789abcdef0123456789abcdef"), &[], t).await;
    match &code {
        Outcome::Code(c) => assert_eq!(c.expose(), "0123456789abcdef0123456789abcdef"),
        other => panic!("{other:?}"),
    }
    assert!(!format!("{code:?}").contains("0123"));
    assert_eq!(run(&fake(d.path(), "closed", "echo 'quiver-webview: the sign-in window was closed' >&2; exit 2"), &[], t).await, Outcome::Cancelled);
    assert_eq!(run(&fake(d.path(), "late", "exit 3"), &[], t).await, Outcome::TimedOut);
    assert_eq!(
        run(&fake(d.path(), "nolib", "echo 'Gtk-WARNING: x' >&2; echo 'quiver-webview: WebKitGTK isn'\\''t available: no lib' >&2; exit 4"), &[], t).await,
        Outcome::Unavailable("WebKitGTK isn't available: no lib".into())
    );
    assert_eq!(run(&fake(d.path(), "bad", "echo 'quiver-webview: bad' >&2; exit 1"), &[], t).await, Outcome::Failed("bad".into()));
    assert!(matches!(run(&fake(d.path(), "empty", "exit 0"), &[], t).await, Outcome::Failed(_)));
    assert!(matches!(run(&d.path().join("missing"), &[], t).await, Outcome::Unavailable(_)));
}

#[tokio::test]
async fn a_hung_helper_is_stopped() {
    let d = tempfile::tempdir().unwrap();
    let started = Instant::now();
    // The helper's own timeout is the first line of defence; this is the second.
    let o = tokio::time::timeout(Duration::from_secs(30), run(&fake(d.path(), "hung", "exec sleep 600"), &[], Duration::from_millis(300))).await;
    assert_eq!(o.unwrap(), Outcome::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
}
