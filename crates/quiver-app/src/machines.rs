//! Several machines in one app (design/REMOTE.md §5): this PC and the servers it is paired
//! with (`quiver remote add`, Settings → Machines).
//!
//! - **Each paired server gets its own `Client`** ([`Source::machine`]: TLS to its quiverd,
//!   `quiver-api`'s reconnecting client), with its own event stream, library and download
//!   queue. Paired servers come from this machine's quiverd (`RemotePeers`), which also
//!   checks whether each is up (`PeerOnline`/`PeerOffline`).
//! - **The view** is what the top bar's machine switcher picks. [`View::All`] (the
//!   default) is this PC's app with the servers merged in: each title once, with the
//!   machines it is installed on (`GameRow::remote`), titles only a server has (rows with a
//!   `home`), each machine's downloads grouped on the Downloads page, Play split between
//!   **Play on This PC** and **Stream from …**, and **Install on …**. [`View::Here`] is this
//!   PC alone. [`View::Machine`] is the whole app on one server: its library, downloads,
//!   settings, profiles, mods and tweaks, through its client (`Quiver::source` is that
//!   machine's while it is shown).
//! - **Where Play goes** is remembered per game in the app's own preferences
//!   (`app.json`, `play_on`).
//!
//! Art for another machine's games comes from the stores' CDNs (shared by URL); its local
//! files (Steam's library cache there) are keyed by machine (`ArtSource::Machine`) and
//! aren't fetched over the tunnel yet.

use futures::StreamExt;
use gpui::Context;
use quiver_api::downloads::{QueueEntry, QueueState};
use quiver_api::remote::{PeerInfo, PeerState, RemoteStatus, Role, StartStreamReply};
use quiver_api::{Event, LibraryEntry, Request, Status};

use crate::app::{Cmd, Command, Quiver};
use crate::model::{Gid, GameRow, RemoteCopy, clean_name};
use crate::nav::{Page, Section, SettingsSection};
use crate::source::Source;
use crate::ui::Icon;
use crate::{err_text, on_tokio};

/// Which machines the app shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// This PC with the paired servers merged in.
    #[default]
    All,
    /// This PC only.
    Here,
    /// One paired server (its id): the whole app is that machine's.
    Machine(String),
}

/// A paired server's link, as its client reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Connecting,
    Up,
    /// Not connected (it can't be reached, or it refused): why.
    Down(String),
}

/// A paired server and what the app knows of it.
pub struct Remote {
    pub peer: PeerInfo,
    pub source: Option<Source>,
    pub link: Link,
    pub status: Option<Status>,
    pub library: Vec<LibraryEntry>,
    pub queue: Vec<QueueEntry>,
    /// A connection attempt is under way or done (don't start another).
    started: bool,
}

impl Remote {
    fn new(peer: PeerInfo) -> Remote {
        Remote { peer, source: None, link: Link::Connecting, status: None, library: Vec::new(), queue: Vec::new(), started: false }
    }

    pub fn id(&self) -> &str {
        &self.peer.id
    }

    pub fn name(&self) -> &str {
        &self.peer.name
    }

    /// Its status dot: online (connected), asleep (can be woken) or offline.
    pub fn state(&self) -> PeerState {
        match &self.link {
            Link::Up => PeerState::Online,
            _ if self.peer.mac.is_some() => PeerState::Asleep,
            _ => PeerState::Offline,
        }
    }

    pub fn up(&self) -> bool {
        self.link == Link::Up
    }

    /// Its downloads that are running or waiting.
    pub fn downloads(&self) -> impl Iterator<Item = &QueueEntry> {
        self.queue.iter().filter(|e| !matches!(e.state, QueueState::Done))
    }

    /// The running download's progress, 0..1.
    pub fn progress(&self) -> Option<f32> {
        let e = self.queue.iter().find(|e| e.state.is_active())?;
        (e.total_bytes > 0).then(|| (e.done_bytes as f64 / e.total_bytes as f64) as f32)
    }

    /// "2 downloading · 38%", or "" when nothing is.
    pub fn activity(&self) -> String {
        let n = self.downloads().filter(|e| e.state != QueueState::Failed).count();
        match (n, self.progress()) {
            (0, _) => String::new(),
            (n, Some(f)) => format!("{n} downloading · {:.0}%", (f * 100.).floor()),
            (n, None) => format!("{n} queued"),
        }
    }
}

/// The Settings page's Add form and the switcher's state.
#[derive(Default)]
pub struct MachinesUi {
    pub view: View,
    /// This machine's quiverd (kept while another machine is shown).
    pub local: Option<Source>,
    /// This machine's remote state (`RemoteStatus`).
    pub here: Option<Result<RemoteStatus, String>>,
    pub remotes: Vec<Remote>,
    /// A game to open once the machine it lives on is shown.
    pending_open: Option<Gid>,
    /// Settings → Machines (`ui/settings/machines.rs`).
    pub settings: crate::ui::settings::machines::MachinesSettings,
}

impl MachinesUi {
    pub fn remote(&self, id: &str) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.id() == id)
    }

    fn remote_mut(&mut self, id: &str) -> Option<&mut Remote> {
        self.remotes.iter_mut().find(|r| r.id() == id)
    }

    /// The machine shown, when it isn't this one.
    pub fn shown(&self) -> Option<&Remote> {
        match &self.view {
            View::Machine(id) => self.remote(id),
            _ => None,
        }
    }

    /// Merging the servers into this PC's library.
    pub fn merged(&self) -> bool {
        self.view == View::All && !self.remotes.is_empty()
    }

    /// This machine's name ("This PC" until quiverd says).
    pub fn here_name(&self) -> String {
        match &self.here {
            Some(Ok(s)) => s.name.clone(),
            _ => "This PC".into(),
        }
    }
}

/// What a switcher, game-menu or Downloads entry does (`Cmd::Machine`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineCmd {
    /// Show every machine (0), this PC (1), or paired server `k` (`k + 2`).
    View(usize),
    Wake(usize),
    Settings,
    /// Play this game here (and remember it).
    PlayHere(Gid),
    /// Stream this game from paired server `k` (and remember it).
    Stream(Gid, usize),
    /// Install this game here (`None`) or on paired server `k`.
    InstallOn(Gid, Option<usize>),
}

/// A stand-in for the client's capabilities until the stream client (`quiver-stream`)
/// reports its own: the window's size, 60 Hz, stereo audio, no hardware decoder listed.
fn caps(viewport: (f32, f32)) -> quiver_api::remote::ClientCaps {
    quiver_api::remote::ClientCaps {
        decoders: Vec::new(),
        display: quiver_stream_proto::DisplayCaps {
            width: viewport.0.max(1.) as u32,
            height: viewport.1.max(1.) as u32,
            refresh_mhz: 60_000,
            hdr: false,
            vrr: false,
            stereo_sbs: false,
            scale: 1.0,
        },
        audio_channels: 2,
        pads: Vec::new(),
        microphone: false,
    }
}

impl Quiver {
    // -----------------------------------------------------------------------------------------
    // Machines and their clients

    /// Connected to this machine's quiverd: remember it, and read the paired machines.
    pub fn machines_start(&mut self, cx: &mut Context<Self>) {
        if self.mx.local.is_none() {
            self.mx.local = self.source.clone();
            // Machines that couldn't be reached are tried again now and then.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                    if this.update(cx, |this, cx| this.machines_tick(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        self.machines_reload(cx);
    }

    /// Ask this machine's quiverd for its remote state and paired machines.
    pub fn machines_reload(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.mx.local.clone() else { return };
        self.call_on::<RemoteStatus, _>(local, Request::RemoteStatus, cx, |this, r, cx| {
            let peers = r.as_ref().map(|s| s.peers.clone()).unwrap_or_default();
            this.mx.here = Some(r.map_err(|e| err_text(&e)));
            this.machines_set_peers(peers, cx);
        });
        self.moonlight_reload(cx);
    }

    fn machines_set_peers(&mut self, peers: Vec<PeerInfo>, cx: &mut Context<Self>) {
        let servers: Vec<PeerInfo> = peers.into_iter().filter(|p| p.role == Role::Server).collect();
        self.mx.remotes.retain(|r| servers.iter().any(|p| p.id == r.peer.id));
        for p in servers {
            match self.mx.remote_mut(&p.id) {
                Some(r) => r.peer = p,
                None => self.mx.remotes.push(Remote::new(p)),
            }
        }
        // A machine shown that was revoked: back to this one.
        if let View::Machine(id) = &self.mx.view
            && self.mx.remote(id).is_none()
        {
            self.set_view(View::All, cx);
        }
        let ids: Vec<String> = self.mx.remotes.iter().filter(|r| !r.started).map(|r| r.peer.id.clone()).collect();
        for id in ids {
            self.machine_connect(&id, cx);
        }
        self.machines_merge(cx);
    }

    /// Connect to a paired server with its own client.
    fn machine_connect(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some(local), Some(r)) = (self.mx.local.clone(), self.mx.remote_mut(id)) else { return };
        r.started = true;
        r.link = Link::Connecting;
        let peer = r.peer.clone();
        let id = id.to_owned();
        let rx = on_tokio(async move {
            let (source, status, mut events) = local.machine(&peer).await?;
            let (tx, rx) = futures::channel::mpsc::unbounded();
            tokio::spawn(async move {
                while let Some(e) = events.recv().await {
                    if tx.unbounded_send(e).is_err() {
                        break;
                    }
                }
            });
            anyhow::Ok((source, status, rx))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled")));
            let events = this
                .update(cx, |this, cx| {
                    let r = this.mx.remote_mut(&id)?;
                    let out = match result {
                        Ok((source, status, events)) => {
                            r.source = Some(source);
                            r.status = Some(status);
                            r.link = Link::Up;
                            Some(events)
                        }
                        Err(e) => {
                            r.link = Link::Down(err_text(&e));
                            None
                        }
                    };
                    if out.is_some() {
                        this.machine_load(&id, cx);
                    }
                    cx.notify();
                    out
                })
                .ok()
                .flatten();
            let Some(mut events) = events else { return };
            while let Some(e) = events.next().await {
                if this.update(cx, |this, cx| this.on_machine_event(&id, e, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Read a paired server's library and queue.
    fn machine_load(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(source) = self.mx.remote(id).and_then(|r| r.source.clone()) else { return };
        let i = id.to_owned();
        self.call_on::<Vec<LibraryEntry>, _>(source.clone(), Request::Library { refresh: false, all_stores: true }, cx, move |this, r, cx| {
            if let (Ok(lib), Some(m)) = (r, this.mx.remote_mut(&i)) {
                m.library = lib;
                this.machines_merge(cx);
            }
        });
        self.machine_queue(id, cx);
    }

    /// Read a paired machine's queue again (after a change made from here).
    pub fn machines_refresh_queue(&mut self, id: &str, cx: &mut Context<Self>) {
        self.machine_queue(id, cx);
    }

    fn machine_queue(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(source) = self.mx.remote(id).and_then(|r| r.source.clone()) else { return };
        let i = id.to_owned();
        self.call_on::<Vec<QueueEntry>, _>(source, Request::Queue, cx, move |this, r, cx| {
            if let (Ok(q), Some(m)) = (r, this.mx.remote_mut(&i)) {
                m.queue = q;
                this.machines_merge(cx);
            }
        });
    }

    /// Every few seconds: the servers' queues (their downloads show in the switcher and on
    /// Downloads), and another try at machines that couldn't be reached.
    fn machines_tick(&mut self, cx: &mut Context<Self>) {
        let up: Vec<String> = self.mx.remotes.iter().filter(|r| r.up()).map(|r| r.peer.id.clone()).collect();
        for id in up {
            self.machine_queue(&id, cx);
        }
        let down: Vec<String> = self.mx.remotes.iter().filter(|r| matches!(r.link, Link::Down(_)) && r.peer.state == PeerState::Online).map(|r| r.peer.id.clone()).collect();
        for id in down {
            self.machine_connect(&id, cx);
        }
    }

    /// An event from this machine's quiverd: remote changes here, then the app's handling
    /// unless another machine is shown.
    pub fn on_local_event(&mut self, e: Event, cx: &mut Context<Self>) {
        match &e {
            Event::RemoteChanged => self.machines_reload(cx),
            Event::PeerOnline { id, .. } => {
                if let Some(r) = self.mx.remote_mut(id) {
                    r.peer.state = PeerState::Online;
                    if matches!(r.link, Link::Down(_)) {
                        let id = id.clone();
                        self.machine_connect(&id, cx);
                    }
                }
                self.machines_reload(cx);
            }
            Event::PeerOffline { .. } => self.machines_reload(cx),
            Event::PairingEnded { .. } => self.machines_pairing_ended(&e, cx),
            Event::MoonlightPairingRequested { .. } | Event::MoonlightPaired { .. } | Event::MoonlightChanged => self.moonlight_event(&e, cx),
            _ => {}
        }
        if !matches!(self.mx.view, View::Machine(_)) {
            self.on_event(e, cx);
        }
    }

    /// An event from a paired server's quiverd.
    fn on_machine_event(&mut self, id: &str, e: Event, cx: &mut Context<Self>) {
        match &e {
            Event::Disconnected => {
                if let Some(r) = self.mx.remote_mut(id) {
                    r.link = Link::Down("reconnecting…".into());
                }
            }
            Event::Reconnected => {
                if let Some(r) = self.mx.remote_mut(id) {
                    r.link = Link::Up;
                }
                self.machine_load(id, cx);
            }
            Event::LibraryChanged | Event::GameStarted { .. } | Event::GameStopped { .. } => self.machine_load(id, cx),
            Event::Job(_) => self.machine_queue(id, cx),
            _ => {}
        }
        if self.mx.view == View::Machine(id.to_owned()) {
            self.on_event(e, cx);
        }
        cx.notify();
    }

    /// [`Quiver::call`] on a given machine's source.
    pub fn call_on<T, F>(&self, source: Source, req: Request, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let rx = on_tokio(async move { source.call::<T>(req).await });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled")));
            this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -----------------------------------------------------------------------------------------
    // The view

    /// Show every machine, this one, or one paired server (the whole app is then its).
    pub fn set_view(&mut self, v: View, cx: &mut Context<Self>) {
        if self.mx.view == v {
            return;
        }
        let source = match &v {
            View::Machine(id) => match self.mx.remote(id) {
                Some(r) if r.up() => r.source.clone(),
                Some(r) => {
                    let why = match &r.link {
                        Link::Down(e) => e.clone(),
                        _ => "connecting".into(),
                    };
                    return self.show_toast(format!("{} isn't connected: {why}", r.name()), cx);
                }
                None => return,
            },
            _ => self.mx.local.clone(),
        };
        let switching = matches!(v, View::Machine(_)) || matches!(self.mx.view, View::Machine(_));
        self.mx.view = v;
        if switching {
            // Another quiverd: start from its state (jobs, games and pages are its).
            self.source = source;
            self.games.clear();
            self.library_loaded = false;
            if matches!(self.nav.page, Page::Game(_)) {
                self.nav.open_section(Section::Library);
            }
            self.call::<Status, _>(Request::Status, cx, |this, r, cx| match r {
                Ok(s) => this.on_status(s, cx),
                Err(e) => this.show_error(&e, cx),
            });
        } else {
            self.machines_merge(cx);
        }
        cx.notify();
    }

    /// The paired server shown, if any: (id, name).
    pub fn shown_machine(&self) -> Option<(String, String)> {
        self.mx.shown().map(|r| (r.peer.id.clone(), r.peer.name.clone()))
    }

    /// The library's rows after a load: in the merged view, each server's copies and the
    /// titles only a server has; on a server's own view, every row is that machine's.
    pub fn machines_merge(&mut self, cx: &mut Context<Self>) {
        match self.mx.view.clone() {
            View::Machine(id) => {
                for g in &mut self.games {
                    g.home = Some(id.clone());
                    g.remote.clear();
                }
            }
            View::Here => {
                self.games.retain(|g| g.home.is_none());
                for g in &mut self.games {
                    g.remote.clear();
                }
            }
            View::All => {
                self.games.retain(|g| g.home.is_none());
                let copies = |gid: Gid, remotes: &[Remote]| -> Vec<RemoteCopy> {
                    remotes
                        .iter()
                        .filter(|r| r.up())
                        .filter_map(|r| {
                            let e = r.library.iter().find(|e| Gid(e.game) == gid)?;
                            let installing = r.queue.iter().find(|q| Gid(q.game_ref()) == gid && q.state.is_active()).and_then(|q| {
                                (q.total_bytes > 0).then(|| (q.done_bytes as f64 / q.total_bytes as f64) as f32)
                            });
                            Some(RemoteCopy {
                                machine: r.peer.id.clone(),
                                name: r.peer.name.clone(),
                                installed: e.on_disk(),
                                running: e.running,
                                update: e.has_update(),
                                installing,
                            })
                        })
                        .collect()
                };
                for g in &mut self.games {
                    g.remote = copies(g.id(), &self.mx.remotes);
                }
                for r in self.mx.remotes.iter().filter(|r| r.up()) {
                    for e in r.library.iter().filter(|e| e.on_disk()) {
                        let gid = Gid(e.game);
                        if self.games.iter().any(|g| g.id() == gid) {
                            continue;
                        }
                        let mut entry = e.clone();
                        entry.name = clean_name(&entry.name);
                        let mut row = GameRow::new(entry);
                        row.home = Some(r.peer.id.clone());
                        row.remote = copies(gid, &self.mx.remotes);
                        self.games.push(row);
                    }
                }
            }
        }
        // A game picked in the merged library, now that its machine is shown.
        if let Some(id) = self.mx.pending_open
            && self.library_loaded
            && self.game(id).is_some()
        {
            self.mx.pending_open = None;
            self.nav.open_game(id);
            self.load_details(id, cx);
        }
    }

    /// The merged library's "installed on" badge for a game: where it is installed, when a
    /// paired machine is one of them ("Tower", "This PC + Tower", "Tower 38%"), and whether
    /// only paired machines have it.
    pub fn installed_on(&self, g: &GameRow) -> Option<(String, bool)> {
        if !self.mx.merged() {
            return None;
        }
        let mut names: Vec<String> = Vec::new();
        let here = g.home.is_none() && g.entry.on_disk();
        if here {
            names.push("This PC".into());
        }
        for c in &g.remote {
            match c.installing {
                Some(f) if !c.installed => names.push(format!("{} {:.0}%", c.name, (f * 100.).floor())),
                _ if c.installed => names.push(c.name.clone()),
                _ => {}
            }
        }
        (names.len() > usize::from(here)).then(|| (names.join(" + "), !here))
    }

    /// A game that lives on a server only, opened from the merged library: show that
    /// machine, then its page. True when it did (the caller stops there).
    pub fn machines_redirect(&mut self, id: Gid, cx: &mut Context<Self>) -> bool {
        let Some(home) = self.game(id).and_then(|g| g.home.clone()) else { return false };
        if self.mx.view != View::All {
            return false;
        }
        self.mx.pending_open = Some(id);
        self.set_view(View::Machine(home), cx);
        true
    }

    // -----------------------------------------------------------------------------------------
    // Play, stream, install

    /// Where Play goes for a game: `None` here, `Some(k)` stream from paired server `k`.
    /// The person's last choice when it still applies; else here when it is installed here,
    /// else the first server that has it.
    pub fn play_target(&self, g: &GameRow) -> Option<usize> {
        if let Some(r) = self.mx.shown() {
            // The app is on that server: its games stream from it.
            return self.mx.remotes.iter().position(|x| x.peer.id == r.peer.id).filter(|_| g.entry.on_disk());
        }
        if !self.mx.merged() {
            return None;
        }
        let ix = |m: &str| self.mx.remotes.iter().position(|r| r.peer.id == m && r.up());
        let has = |m: &str| g.installed_elsewhere().any(|c| c.machine == m);
        if let Some(choice) = self.stereo.prefs.play_on.get(&g.id().to_string()) {
            if choice == "here" && g.entry.on_disk() && g.home.is_none() {
                return None;
            }
            if has(choice) && let Some(k) = ix(choice) {
                return Some(k);
            }
        }
        if g.entry.on_disk() && g.home.is_none() {
            return None;
        }
        g.installed_elsewhere().find_map(|c| ix(&c.machine))
    }

    /// The Play button's label when machines decide it: "Stream from Tower", "Play on This
    /// PC" (when a server has it too), or "Install on…" (installed nowhere, servers to pick
    /// from). `None`: the usual label.
    pub fn machine_play_label(&self, g: &GameRow) -> Option<(String, Icon)> {
        if g.running() {
            return None;
        }
        // Still downloading here, ready there: stream it meanwhile.
        if let Some(k) = self.play_target(g) {
            return Some((format!("Stream from {}", self.mx.remotes[k].name()), Icon::Cast));
        }
        if !self.mx.merged() || g.installing().is_some() || g.queued() {
            return None;
        }
        if g.entry.on_disk() && g.installed_elsewhere().next().is_some() {
            return Some(("Play on This PC".into(), Icon::Play));
        }
        let installable = g.entry.installable && (g.entry.game.is_ubisoft() || g.entry.game.store == quiver_api::StoreId::Epic);
        if !g.entry.on_disk() && g.installed_elsewhere().next().is_none() && installable && self.mx.remotes.iter().any(|r| r.up()) {
            return Some(("Install on…".into(), Icon::Download));
        }
        None
    }

    /// Play's machine part: stream, or offer where to install. True when it handled it.
    pub fn machines_primary(&mut self, id: Gid, cx: &mut Context<Self>) -> bool {
        let Some(g) = self.game(id).cloned() else { return false };
        if g.running() || g.installing().is_some() || g.queued() {
            return false;
        }
        if let Some(k) = self.play_target(&g) {
            self.stream_from(id, k, cx);
            return true;
        }
        if self.machine_play_label(&g).is_some_and(|(l, _)| l == "Install on…") {
            self.gp.play_menu = false;
            self.nav.open_menu(id);
            return true;
        }
        false
    }

    /// Ask paired server `k` to stream a game here (`StartStream`). The stream client comes
    /// with the integration; until then its answer (or error) shows as a toast.
    pub fn stream_from(&mut self, id: Gid, k: usize, cx: &mut Context<Self>) {
        let Some(r) = self.mx.remotes.get(k) else { return };
        let (machine, name) = (r.peer.id.clone(), r.peer.name.clone());
        let Some(source) = r.source.clone() else {
            return self.show_toast(format!("{name} isn't connected"), cx);
        };
        self.remember_play_on(id, &machine);
        let target = quiver_api::remote::StreamTarget::Game { game: id.0.to_string(), profile: None };
        let req = Request::StartStream { target, prefs: Default::default(), caps: caps(self.viewport) };
        let game = self.game_name(id);
        self.call_on::<StartStreamReply, _>(source, req, cx, move |this, r, cx| match r {
            Ok(s) => this.show_toast(format!("{} is ready to stream {game} ({})", s.host_name, s.ticket.addr), cx),
            Err(e) => this.toast_for(format!("Stream from {name}: {}", err_text(&e)).into(), true, 7000, cx),
        });
    }

    fn remember_play_on(&mut self, id: Gid, choice: &str) {
        let key = id.to_string();
        if self.stereo.prefs.play_on.get(&key).map(String::as_str) == Some(choice) {
            return;
        }
        self.stereo.prefs.play_on.insert(key, choice.to_owned());
        // Demo mode doesn't write the person's settings.
        if !self.demo
            && let Err(e) = self.stereo.prefs.save()
        {
            tracing::warn!("saving where games play: {e}");
        }
    }

    /// Install a game here (the install dialog) or on paired server `k`.
    pub fn install_on(&mut self, id: Gid, k: Option<usize>, cx: &mut Context<Self>) {
        let Some(k) = k else {
            if matches!(self.mx.view, View::Machine(_)) {
                return self.show_toast("Show This PC to install here", cx);
            }
            return self.open_install_dialog(id, cx);
        };
        let Some(r) = self.mx.remotes.get(k) else { return };
        let (machine, name) = (r.peer.id.clone(), r.peer.name.clone());
        let Some(source) = r.source.clone() else { return self.show_toast(format!("{name} isn't connected"), cx) };
        let game = self.game_name(id);
        self.call_on::<u64, _>(source, Request::Install { game: id.0.into(), language: None, after: None }, cx, move |this, r, cx| match r {
            Ok(_) => {
                this.show_toast(format!("Installing {game} on {name}"), cx);
                this.machine_queue(&machine, cx);
            }
            Err(e) => this.toast_for(format!("Install on {name}: {}", err_text(&e)).into(), true, 7000, cx),
        });
    }

    // -----------------------------------------------------------------------------------------
    // Menus

    /// The machine switcher's entries.
    pub fn machine_commands(&self) -> Vec<Command> {
        let here = self.mx.here_name();
        let local_dl = self.games.iter().filter(|g| g.home.is_none() && g.installing().is_some()).count();
        let mut v = vec![
            Command {
                icon: Icon::LayoutGrid,
                label: "All machines".into(),
                detail: Some(format!("One library: This PC and {} more", self.mx.remotes.len()).into()),
                cli: None,
                action: Cmd::Machine(MachineCmd::View(0)),
                future: false,
            },
            Command {
                icon: Icon::Laptop,
                label: format!("This PC · {here}").into(),
                detail: Some(if local_dl > 0 { format!("{local_dl} downloading") } else { "Only this machine's games".into() }.into()),
                cli: None,
                action: Cmd::Machine(MachineCmd::View(1)),
                future: false,
            },
        ];
        for (k, r) in self.mx.remotes.iter().enumerate() {
            let state = match r.state() {
                PeerState::Online => "Online".to_owned(),
                PeerState::Asleep => "Asleep".to_owned(),
                PeerState::Offline => "Offline".to_owned(),
            };
            let activity = r.activity();
            let detail = if activity.is_empty() { state } else { format!("{state} · {activity}") };
            v.push(Command {
                icon: Icon::Server,
                label: r.name().to_owned().into(),
                detail: Some(detail.into()),
                cli: Some(format!("quiver --machine {} library", r.name()).into()),
                action: Cmd::Machine(MachineCmd::View(k + 2)),
                future: false,
            });
        }
        for (k, r) in self.mx.remotes.iter().enumerate().filter(|(_, r)| r.state() == PeerState::Asleep) {
            v.push(Command {
                icon: Icon::Power,
                label: format!("Wake {}", r.name()).into(),
                detail: Some("Wake-on-LAN".into()),
                cli: Some(format!("quiver remote wake {}", r.name()).into()),
                action: Cmd::Machine(MachineCmd::Wake(k)),
                future: false,
            });
        }
        v.push(Command {
            icon: Icon::Settings,
            label: "Machines settings…".into(),
            detail: Some("Pair, rename, wake, remote on or off".into()),
            cli: Some("quiver remote status".into()),
            action: Cmd::Machine(MachineCmd::Settings),
            future: false,
        });
        v
    }

    /// The game menu's machine entries, first: Play on This PC, Stream from …, Install on ….
    pub fn machine_menu_entries(&self, id: Gid) -> Vec<Command> {
        let Some(g) = self.game(id) else { return Vec::new() };
        if !self.mx.merged() || g.running() {
            return Vec::new();
        }
        let chosen = self.play_target(g);
        let mut v = Vec::new();
        let here = g.entry.on_disk() && g.home.is_none();
        if here && g.installed_elsewhere().next().is_some() {
            v.push(Command {
                icon: Icon::Play,
                label: "Play on This PC".into(),
                detail: Some(if chosen.is_none() { "Runs here · what Play does" } else { "Runs here" }.into()),
                cli: Some(format!("quiver play {id}").into()),
                action: Cmd::Machine(MachineCmd::PlayHere(id)),
                future: false,
            });
        }
        for (k, r) in self.mx.remotes.iter().enumerate() {
            let installed = g.installed_elsewhere().any(|c| c.machine == r.peer.id);
            if installed {
                let detail = if !r.up() {
                    format!("{} isn't connected", r.name())
                } else if chosen == Some(k) {
                    "Runs there, plays here · what Play does".into()
                } else {
                    "Runs there, plays here".into()
                };
                v.push(Command {
                    icon: Icon::Cast,
                    label: format!("Stream from {}", r.name()).into(),
                    detail: Some(detail.into()),
                    cli: None,
                    action: Cmd::Machine(MachineCmd::Stream(id, k)),
                    future: false,
                });
            } else if r.up() && g.entry.installable && g.home.is_none() && r.library.iter().any(|e| Gid(e.game) == id) {
                v.push(Command {
                    icon: Icon::Download,
                    label: format!("Install on {}", r.name()).into(),
                    detail: Some(format!("Downloads there, in {}'s queue", r.name()).into()),
                    cli: Some(format!("quiver --machine {} install {id}", r.name()).into()),
                    action: Cmd::Machine(MachineCmd::InstallOn(id, Some(k))),
                    future: false,
                });
            }
        }
        if !g.entry.on_disk() && g.home.is_none() && g.entry.installable && g.installing().is_none() && !g.queued() {
            v.push(Command {
                icon: Icon::Download,
                label: "Install on This PC".into(),
                detail: Some("Downloads here".into()),
                cli: Some(format!("quiver install {id}").into()),
                action: Cmd::Machine(MachineCmd::InstallOn(id, None)),
                future: false,
            });
        }
        v
    }

    pub fn run_machine_cmd(&mut self, c: MachineCmd, cx: &mut Context<Self>) {
        match c {
            MachineCmd::View(0) => self.set_view(View::All, cx),
            MachineCmd::View(1) => self.set_view(View::Here, cx),
            MachineCmd::View(k) => {
                if let Some(id) = self.mx.remotes.get(k - 2).map(|r| r.peer.id.clone()) {
                    self.set_view(View::Machine(id), cx);
                }
            }
            MachineCmd::Wake(k) => {
                if let Some(id) = self.mx.remotes.get(k).map(|r| r.peer.id.clone()) {
                    self.machine_wake(&id, cx);
                }
            }
            MachineCmd::Settings => self.nav.open_settings(SettingsSection::Machines),
            MachineCmd::PlayHere(id) => {
                self.remember_play_on(id, "here");
                self.primary(id, cx);
            }
            MachineCmd::Stream(id, k) => self.stream_from(id, k, cx),
            MachineCmd::InstallOn(id, k) => self.install_on(id, k, cx),
        }
    }

    /// Demo mode: screenshot set-ups from `QUIVER_DEMO_GLOBAL` (`ui/global.rs`), once the
    /// demo machines are connected. `machine=tower` shows Tower (`machine-game=ID` then
    /// opens a game there), `view=here` this PC only, `machines-menu` opens the switcher,
    /// `remote-on` turns remote on (`pair-code` then shows a code), `add-machine` opens Add,
    /// `stream=ID` asks Tower to stream a game.
    pub fn start_machines_demo(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        use crate::ui::global::demo_flag;
        let wanted = ["machine", "machine-game", "view", "machines-menu", "remote-on", "pair-code", "add-machine", "stream"];
        if wanted.iter().all(|f| demo_flag(f).is_none()) {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                let ready = this.update(cx, |this, _| !this.games.is_empty() && this.mx.remotes.iter().any(|r| r.up())).unwrap_or(true);
                if ready {
                    break;
                }
            }
            cx.background_executor().timer(std::time::Duration::from_millis(600)).await;
            let _ = this.update(cx, |this, cx| {
                use crate::ui::settings::machines::MachineAction;
                if demo_flag("view").as_deref() == Some("here") {
                    this.set_view(View::Here, cx);
                }
                if let Some(m) = demo_flag("machine")
                    && let Some(id) = this.mx.remotes.iter().find(|r| r.peer.name.eq_ignore_ascii_case(&m)).map(|r| r.peer.id.clone())
                {
                    this.mx.pending_open = demo_flag("machine-game").and_then(|g| g.parse().ok());
                    this.set_view(View::Machine(id), cx);
                }
                if demo_flag("machines-menu").is_some() {
                    this.nav.open_machines();
                }
                if demo_flag("remote-on").is_some() {
                    this.machine_action(MachineAction::Remote, cx);
                }
                if demo_flag("add-machine").is_some() {
                    this.machine_action(MachineAction::Add, cx);
                }
                if let Some(id) = demo_flag("stream").and_then(|g| g.parse().ok()) {
                    this.stream_from(id, 0, cx);
                }
                cx.notify();
            });
            if demo_flag("pair-code").is_some() {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                let _ = this.update(cx, |this, cx| this.machine_action(crate::ui::settings::machines::MachineAction::PairHere, cx));
            }
        })
        .detach();
    }

    /// Wake-on-LAN for a paired machine, sent by this machine's quiverd.
    pub fn machine_wake(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(local) = self.mx.local.clone() else { return };
        let name = self.mx.remote(id).map(|r| r.peer.name.clone()).unwrap_or_else(|| id.to_owned());
        self.call_on::<(), _>(local, Request::RemoteWake { id: id.to_owned() }, cx, move |this, r, cx| match r {
            Ok(()) => this.show_toast(format!("Sent a wake-up to {name}; it shows as online once it's up"), cx),
            Err(e) => this.show_error(&e, cx),
        });
    }
}
