//! `quiver-app --page downloads` (or `library`, `activity`): open Quiver on that page, or turn
//! the Quiver that is already open to it. The status-bar widget (contrib/omarchy) uses it for
//! "Open Quiver".
//!
//! A running app (not demo mode) listens on `$XDG_RUNTIME_DIR/quiver/app.sock`. A launch
//! with `--page` first offers the page there: one line, `page <name>`, answered `ok`; when
//! that is delivered it exits, and otherwise it starts as usual on that page. Raising the
//! window is the caller's job (Wayland compositors don't let a window take focus by
//! itself); the widget asks Hyprland to focus it.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AsyncWindowContext, WeakEntity};

use crate::app::Quiver;
use crate::nav::Section;

/// The page `--page <name>` (or `--page=<name>`) asks for, if any.
pub fn requested_page(args: impl IntoIterator<Item = String>) -> Option<Section> {
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let value = match a.strip_prefix("--page=") {
            Some(v) => v.to_owned(),
            None if a == "--page" => args.next()?,
            None => continue,
        };
        return parse_page(&value);
    }
    None
}

fn parse_page(name: &str) -> Option<Section> {
    match name.trim().to_ascii_lowercase().as_str() {
        "library" => Some(Section::Library),
        "downloads" => Some(Section::Downloads),
        "activity" => Some(Section::Activity),
        _ => None,
    }
}

fn page_name(s: Section) -> &'static str {
    match s {
        Section::Library => "library",
        Section::Downloads => "downloads",
        Section::Activity => "activity",
    }
}

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("quiver").join("app.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("quiver-{user}")).join("app.sock")
        }
    }
}

/// Offer `page` to a running app; true when it took it.
pub fn forward(page: Section) -> bool {
    forward_at(&socket_path(), page)
}

fn forward_at(path: &Path, page: Section) -> bool {
    let attempt = || -> std::io::Result<bool> {
        let mut s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.set_write_timeout(Some(Duration::from_secs(2)))?;
        writeln!(s, "page {}", page_name(page))?;
        let mut answer = String::new();
        BufReader::new(s).read_line(&mut answer)?;
        Ok(answer.trim() == "ok")
    };
    attempt().unwrap_or(false)
}

/// The listening socket: bound when no other app answers on it (a stale one is replaced).
/// `take_over`: a relaunch onto a new build, whose predecessor is on its way out.
fn bind(path: &Path, take_over: bool) -> Option<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    if path.exists() {
        if !take_over && UnixStream::connect(path).is_ok() {
            tracing::info!("another Quiver window answers on {}; not listening for pages", path.display());
            return None;
        }
        let _ = std::fs::remove_file(path);
    }
    match UnixListener::bind(path) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!("can't listen on {}: {e}", path.display());
            None
        }
    }
}

/// Listen for pages and open each one asked for, for as long as the window lives.
pub async fn listen(take_over: bool, this: WeakEntity<Quiver>, cx: &mut AsyncWindowContext) {
    let Some(listener) = bind(&socket_path(), take_over) else { return };
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Section>();
    std::thread::Builder::new().name("quiver-app-handoff".into()).spawn(move || serve(listener, tx)).ok();
    use futures::StreamExt;
    while let Some(page) = rx.next().await {
        let alive = this.update_in(cx, |this, window, cx| {
            this.nav.open_section(page);
            // Asks the compositor; Hyprland marks the window urgent rather than raising it.
            window.activate_window();
            cx.notify();
        });
        if alive.is_err() {
            break;
        }
    }
}

/// Answer connections until the app stops taking pages.
fn serve(listener: UnixListener, tx: futures::channel::mpsc::UnboundedSender<Section>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        // One short line; anything longer isn't ours.
        let read = BufReader::new((&stream).take(64)).read_line(&mut line);
        let page = read.ok().and_then(|_| line.trim().strip_prefix("page ").and_then(parse_page));
        let answer = match page {
            Some(p) if tx.unbounded_send(p).is_ok() => "ok",
            Some(_) => return,
            None => "unknown",
        };
        let _ = writeln!(stream, "{answer}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_the_page_argument() {
        assert_eq!(requested_page(args(&["--page", "downloads"])), Some(Section::Downloads));
        assert_eq!(requested_page(args(&["--page=Activity"])), Some(Section::Activity));
        assert_eq!(requested_page(args(&["--page", "nope"])), None);
        assert_eq!(requested_page(args(&["--page"])), None);
        assert_eq!(requested_page(args(&[])), None);
    }

    #[test]
    fn hands_a_page_to_the_running_app() {
        let dir = std::env::temp_dir().join(format!("quiver-handoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("app.sock");
        assert!(!forward_at(&path, Section::Downloads), "nobody listening");
        let listener = bind(&path, false).expect("binds");
        assert!(bind(&path, false).is_none(), "a live app keeps the socket");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || serve(listener, tx));
        assert!(forward_at(&path, Section::Downloads));
        assert_eq!(rx.try_next().unwrap(), Some(Section::Downloads));
        // Junk gets "unknown", and the app keeps serving.
        let mut s = UnixStream::connect(&path).unwrap();
        writeln!(s, "rm -rf /").unwrap();
        let mut answer = String::new();
        BufReader::new(s).read_line(&mut answer).unwrap();
        assert_eq!(answer.trim(), "unknown");
        assert!(forward_at(&path, Section::Activity));
        assert_eq!(rx.try_next().unwrap(), Some(Section::Activity));
        // A relaunch takes the socket over.
        let again = bind(&path, true).expect("taken over");
        drop(again);
        // A stale socket (nobody answering) is replaced.
        assert!(bind(&path, false).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
