//! Streaming a game from a paired machine (design/REMOTE.md §5): the split Play button's
//! **Stream from <machine>**.
//!
//! 1. A launch splash goes up at once.
//! 2. This machine's caps come from the stream client (`quiver-stream caps --json`: its
//!    decoders, the display, pads); if that fails, the window's size stands in.
//! 3. `StartStream` on that machine's client: fresh keys every time, or the resume of a
//!    session kept there since the client left.
//! 4. `quiver-stream connect` runs full screen on this output with the ticket on its stdin.
//!    The splash comes down once its window is up; a failure (the machine's answer, or the
//!    client exiting with an error) is a toast.
//!
//! The library card shows **Resume on <machine>** while that machine keeps a session of the
//! game, and **Streaming** while one runs (the machine's `Streams`, refreshed on its
//! `StreamsChanged`).
//!
//! The client is `QUIVER_STREAM_CLIENT` (a command line), else `quiver-stream` beside this
//! binary, else on `PATH`.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gpui::{Context, FontWeight, IntoElement, ParentElement, Styled, div, px};
use quiver_api::Request;
use quiver_api::remote::{ClientCaps, StartStreamReply, StreamSession, StreamState, StreamTarget};

use crate::app::Quiver;
use crate::model::{GameRow, Gid};
use crate::theme::{Theme, rgba};
use crate::ui::{Icon, card, display, icon, soft_shadow};
use crate::{err_text, on_tokio};

/// A stream being started from here: what the splash says.
#[derive(Clone, Debug)]
pub struct StreamLaunch {
    pub game: String,
    pub machine: String,
    pub phase: String,
}

/// What the start-up reports back to the app.
enum Step {
    Phase(String),
    /// The client's window is up (or the client has been running a moment).
    Up,
    Done(Result<(), String>),
}

/// The stream client's command line.
pub fn client_command() -> Vec<String> {
    if let Ok(c) = std::env::var("QUIVER_STREAM_CLIENT")
        && !c.trim().is_empty()
    {
        return c.split_whitespace().map(str::to_owned).collect();
    }
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("quiver-stream"))).filter(|p| p.is_file());
    vec![beside.map(|p| p.display().to_string()).unwrap_or_else(|| "quiver-stream".into())]
}

/// `quiver-stream caps --json`, or why not.
pub fn probe_caps() -> Result<ClientCaps, String> {
    let c = client_command();
    let out = Command::new(&c[0]).args(&c[1..]).args(["caps", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(|e| format!("{}: {e}", c[0]))?;
    if !out.status.success() {
        return Err(format!("{} caps: {}", c[0], out.status));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("{} caps: {e}", c[0]))
}

/// The client's last words on stderr, for a toast: its `Error:` line, else its last line.
pub fn last_error(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.iter().find(|l| l.starts_with("Error:")).map(|l| l.trim_start_matches("Error:").trim().to_owned()).or_else(|| lines.last().map(|l| (*l).to_owned()))
}

impl Quiver {
    /// The stream of this game on a paired server, if one is running or kept: (server, the
    /// session).
    pub fn stream_session(&self, g: &GameRow) -> Option<(usize, &StreamSession)> {
        let game = g.id().0.to_string();
        self.mx.remotes.iter().enumerate().find_map(|(k, r)| r.streams.iter().find(|s| s.game.as_deref() == Some(game.as_str())).map(|s| (k, s)))
    }

    /// The card's word for a stream of this game: "Streaming", or "Resume on Tower".
    pub fn stream_tag(&self, g: &GameRow) -> Option<String> {
        let (k, s) = self.stream_session(g)?;
        Some(match s.state {
            StreamState::Kept => format!("Resume on {}", self.mx.remotes[k].name()),
            StreamState::Starting | StreamState::Streaming => "Streaming".into(),
        })
    }

    /// Stream a game here from paired server `k` (Play's **Stream from …**, or a resume).
    pub fn stream_from(&mut self, id: Gid, k: usize, cx: &mut Context<Self>) {
        let Some(r) = self.mx.remotes.get(k) else { return };
        let (machine, name) = (r.peer.id.clone(), r.peer.name.clone());
        let Some(source) = r.source.clone() else {
            return self.show_toast(format!("{name} isn't connected"), cx);
        };
        if self.mx.launch.is_some() {
            return self.show_toast("A stream is already starting", cx);
        }
        self.remember_play_on(id, &machine);
        let game = self.game_name(id);
        let resume = self.game(id).and_then(|g| self.stream_session(g)).is_some_and(|(_, s)| s.state == StreamState::Kept);
        self.mx.launch = Some(StreamLaunch { game: game.clone(), machine: name.clone(), phase: if resume { format!("Resuming on {name}…") } else { format!("Starting on {name}…") } });
        cx.notify();
        let target = StreamTarget::Game { game: id.0.to_string(), profile: None };
        let fallback = crate::machines::window_caps(self.viewport);
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Step>();
        // It reports through `tx`; its own result isn't needed.
        drop(on_tokio(async move {
            let caps = tokio::task::spawn_blocking(probe_caps).await.ok().and_then(Result::ok).unwrap_or(fallback);
            let req = Request::StartStream { target: target.clone(), prefs: Default::default(), caps };
            let reply: StartStreamReply = match source.call(req).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.unbounded_send(Step::Done(Err(err_text(&e))));
                    return;
                }
            };
            let _ = tx.unbounded_send(Step::Phase("Connecting…".into()));
            let t = tx.clone();
            let r = tokio::task::spawn_blocking(move || run_client(&reply, &target, |s| drop(t.unbounded_send(s)))).await;
            let _ = tx.unbounded_send(Step::Done(r.unwrap_or_else(|e| Err(e.to_string()))));
        }));
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(step) = rx.next().await {
                let ok = this
                    .update(cx, |this, cx| {
                        match step {
                            Step::Phase(p) => {
                                if let Some(l) = this.mx.launch.as_mut() {
                                    l.phase = p;
                                }
                            }
                            Step::Up => this.mx.launch = None,
                            Step::Done(r) => {
                                this.mx.launch = None;
                                if let Err(e) = r {
                                    this.toast_for(format!("Stream from {name}: {e}").into(), true, 8000, cx);
                                }
                                this.machine_streams(&machine, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    /// The launch splash, while a stream starts.
    pub fn render_stream_splash(&self, th: &Theme) -> Option<impl IntoElement + use<>> {
        let l = self.mx.launch.clone()?;
        Some(
            div().absolute().top_0().left_0().right_0().bottom_0().flex().items_center().justify_center().bg(rgba(0x05060a, 0.82)).child(
                card(th)
                    .w(px(420.))
                    .p(px(28.))
                    .gap(px(14.))
                    .items_center()
                    .shadow(vec![soft_shadow(rgba(0x000000, 0.5), 24., 70.)])
                    .child(icon(Icon::Cast, 28., th.acc_hi))
                    .child(display(22., FontWeight::BOLD, th.tx1).text_center().child(l.game))
                    .child(div().text_color(th.tx3).child(format!("Streaming from {}", l.machine)))
                    .child(div().flex().items_center().gap(px(8.)).text_color(th.tx2).child(icon(Icon::LoaderCircle, 14., th.tx3)).child(l.phase)),
            ),
        )
    }
}

/// Run `quiver-stream connect` with the ticket on its stdin until it exits. `Up` once its
/// window is up (its log says the stream's settings arrived) or it has run 4 s.
fn run_client(reply: &StartStreamReply, target: &StreamTarget, step: impl Fn(Step)) -> Result<(), String> {
    let c = client_command();
    let mut child = Command::new(&c[0])
        .args(&c[1..])
        .arg("connect")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run {}: {e}", c[0]))?;
    let line = serde_json::json!({"ticket": reply.ticket, "host_name": reply.host_name, "target": target});
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{line}");
    }
    let stderr = child.stderr.take();
    let (ltx, lrx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let Some(e) = stderr else { return };
        for l in std::io::BufReader::new(e).lines().map_while(Result::ok) {
            if ltx.send(l).is_err() {
                return;
            }
        }
    });
    let start = Instant::now();
    let mut up = false;
    let mut log = String::new();
    loop {
        while let Ok(l) = lrx.try_recv() {
            if l.contains("stream: ") || l.contains("swapchain") {
                if !up {
                    step(Step::Up);
                }
                up = true;
            }
            log.push_str(&l);
            log.push('\n');
        }
        if !up && start.elapsed() > Duration::from_secs(4) {
            step(Step::Up);
            up = true;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(l) = lrx.try_recv() {
                    log.push_str(&l);
                    log.push('\n');
                }
                return if status.success() { Ok(()) } else { Err(last_error(&log).unwrap_or_else(|| format!("the stream client exited ({status})"))) };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clients_error_line_makes_the_toast() {
        let log = "INFO connected\nError: couldn't reach the stream host at 10.0.0.2:47998: timed out\n\nCaused by: x\n";
        assert_eq!(last_error(log).as_deref(), Some("couldn't reach the stream host at 10.0.0.2:47998: timed out"));
        assert_eq!(last_error("a\nb\n").as_deref(), Some("b"));
        assert_eq!(last_error(""), None);
    }
}
