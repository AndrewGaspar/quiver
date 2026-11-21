//! `quiver playing`: the games being played right now (with the stream each goes out on) and
//! the recently played ones, for status bars (contrib/omarchy's widget) and people.
//!
//! `--json` prints one snapshot; `--follow` prints another line whenever it changes, driven by
//! quiverd's events (sessions, streams, the library, profiles), never by polling. Download
//! progress events are ignored, so a running install costs nothing here. With `--no-start`
//! it never starts quiverd, and while quiverd is away (restarting onto a new build, stopped)
//! the follow prints `"connected": false` and picks up again once it is back.
//!
//! A snapshot is a handful of local reads in quiverd: the session list, the library as it
//! has it (no store refresh), the stream list, the profile names (only while a modded game
//! runs), and each installed game's play-time file (`last_played`), since quiverd keeps no
//! "recently played" index. Steam games carry Steam's own last-played time in the library.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use quiver_api::mods::{GameProfiles, ModsRequest};
use quiver_api::remote::{StreamClient, StreamSession, StreamState};
use quiver_api::{ActiveSession, Client, Event, GameRef, LibraryEntry, Playtime, Request};
use serde::Serialize;

/// Most recent games a snapshot lists.
pub const MAX_RECENT: usize = 20;

/// Events that arrive together (a game stopping sends several) are answered with one snapshot.
const SETTLE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Snapshot {
    pub connected: bool,
    pub playing: Vec<Playing>,
    pub recent: Vec<Recent>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Playing {
    /// What `quiver play|stop` take: the product id for a Ubisoft game, else `store:id`.
    #[serde(rename = "ref")]
    pub game_ref: String,
    pub name: String,
    /// "AC Origins" for "Assassin's Creed® Origins" (the app's short names).
    pub short: String,
    pub started: DateTime<Utc>,
    /// The mod profile it runs, by name; `None` when vanilla (or not modded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Who holds the session: `launcher`, `steam`, `bridge`.
    pub via: Vec<String>,
    /// `s-…`, for `quiver sight toggle --session`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<Stream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
}

/// The remote-play stream a running game goes out on.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stream {
    /// The paired machine (or Moonlight client) it streams to.
    pub peer: String,
    /// `quiver` or `moonlight`.
    pub client: &'static str,
    /// `starting`, `streaming`, or `kept` (the client left; the game waits for a resume).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Recent {
    #[serde(rename = "ref")]
    pub game_ref: String,
    pub name: String,
    pub short: String,
    pub last_played: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
}

/// The ref `quiver` commands take for a game.
pub fn cli_ref(g: GameRef) -> String {
    match g.as_ubisoft() {
        Some(id) => id.to_string(),
        None => g.to_string(),
    }
}

/// Product names without trademark signs and odd whitespace (the app's `clean_name`).
fn clean_name(s: &str) -> String {
    s.replace(['®', '™'], "").replace('’', "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The app's short names (`quiver-app` `model::short_name`): "AC Origins".
pub fn short_name(s: &str) -> String {
    let s = clean_name(s).replace(" - ", " ");
    for (long, short) in [("Assassin's Creed ", "AC "), ("Discovery Tour by Assassin's Creed: ", "Discovery Tour: ")] {
        if let Some(rest) = s.strip_prefix(long) {
            return format!("{short}{rest}");
        }
    }
    s.trim_end_matches(" by Ubisoft").to_owned()
}

/// Where a game's small picture is, if one is on this machine already (nothing is fetched):
/// the icon its .desktop export made, Steam's own icon, or the app's cached copy of its icon
/// or box art.
fn icon_for(e: &LibraryEntry, icons: &Path, app_art: &Path) -> Option<PathBuf> {
    if let Some(p) = quiver_core::exports::art::cached_icon(icons, &e.art) {
        return Some(p);
    }
    if let Some(p) = e.client.as_ref().and_then(|c| c.art.icon.clone()).filter(|p| p.is_file()) {
        return Some(p);
    }
    let app_cached = |f: &Option<String>| {
        f.as_deref()
            .filter(|f| !f.is_empty() && !f.starts_with('.') && f.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
            .map(|f| app_art.join(f))
            .filter(|p| p.is_file())
    };
    app_cached(&e.art.icon).or_else(|| app_cached(&e.art.thumb))
}

/// The app's art cache (`quiver-app` `art::default_cache_dir`).
fn app_art_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("quiver/art")
}

/// A session only lab instances hold is an agent's headless run, not someone playing.
fn lab_only(s: &ActiveSession) -> bool {
    !s.holders.is_empty() && s.holders.iter().all(|h| h == quiver_api::lab::LAB_HOLDER)
}

fn stream_for(streams: &[StreamSession], g: GameRef) -> Option<Stream> {
    let s = streams.iter().find(|s| s.game.as_deref().and_then(|r| r.parse::<GameRef>().ok()) == Some(g))?;
    Some(Stream {
        peer: s.peer.clone(),
        client: match s.client {
            StreamClient::Quiver => "quiver",
            StreamClient::Moonlight => "moonlight",
        },
        state: match s.state {
            StreamState::Starting => "starting",
            StreamState::Streaming => "streaming",
            StreamState::Kept => "kept",
        },
        kept_until: s.kept_until,
    })
}

/// When a game was last played: Steam's own record for a game Steam runs, else the sessions
/// Quiver recorded.
async fn last_played(client: &Client, e: &LibraryEntry) -> Option<DateTime<Utc>> {
    let store = e.client.as_ref().and_then(|c| c.playtime.as_ref()).and_then(|p| p.last_played);
    if e.installed.is_none() {
        return store;
    }
    let pt: Playtime = client.call(Request::Playtime { game: e.game.into() }).await.ok()?;
    pt.last_played.or_else(|| pt.sessions.iter().map(|s| s.ended).max()).max(store)
}

/// One snapshot from quiverd.
pub async fn snapshot(client: &Client, recent: usize) -> Result<Snapshot> {
    let sessions: Vec<ActiveSession> = client.call(Request::Sessions).await?;
    let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: true }).await?;
    let streams: Vec<StreamSession> = client.call(Request::Streams).await.unwrap_or_default();
    let icons = quiver_core::exports::Paths::from_env().icons();
    let app_art = app_art_dir();
    let by_ref: HashMap<GameRef, &LibraryEntry> = lib.iter().map(|e| (e.game, e)).collect();

    let sessions: Vec<&ActiveSession> = sessions.iter().filter(|s| !lab_only(s)).collect();
    // Profile names, only when a running game has a profile.
    let profiles: Vec<GameProfiles> = if sessions.iter().any(|s| s.profile.as_deref().is_some_and(|p| p != "vanilla")) {
        client.call(Request::Mods(ModsRequest::Games)).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let profile_name = |g: GameRef, id: &str| {
        profiles
            .iter()
            .find(|gp| gp.game_ref() == g)
            .and_then(|gp| gp.profiles.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_owned())
    };

    let mut playing: Vec<Playing> = sessions
        .iter()
        .map(|s| {
            let g = s.game_ref();
            let e = by_ref.get(&g);
            let name = e.map(|e| clean_name(&e.name)).filter(|n| !n.is_empty()).unwrap_or_else(|| cli_ref(g));
            Playing {
                game_ref: cli_ref(g),
                short: short_name(&name),
                name,
                started: s.started,
                profile: s.profile.as_deref().filter(|p| *p != "vanilla").map(|p| profile_name(g, p)),
                via: s.holders.clone(),
                session: s.session.clone(),
                stream: stream_for(&streams, g),
                icon: e.and_then(|e| icon_for(e, &icons, &app_art)),
            }
        })
        .collect();
    playing.sort_by_key(|p| std::cmp::Reverse(p.started));

    let mut played: Vec<(DateTime<Utc>, &LibraryEntry)> = Vec::new();
    for e in lib.iter().filter(|e| e.on_disk() && !sessions.iter().any(|s| s.game_ref() == e.game)) {
        if let Some(at) = last_played(client, e).await {
            played.push((at, e));
        }
    }
    played.sort_by_key(|(at, e)| (std::cmp::Reverse(*at), e.name.clone()));
    let recent = played
        .into_iter()
        .take(recent.min(MAX_RECENT))
        .map(|(at, e)| {
            let name = clean_name(&e.name);
            Recent { game_ref: cli_ref(e.game), short: short_name(&name), name, last_played: at, icon: icon_for(e, &icons, &app_art) }
        })
        .collect();
    Ok(Snapshot { connected: true, playing, recent })
}

/// Whether an event can change a snapshot.
fn relevant(e: &Event) -> bool {
    matches!(
        e,
        Event::GameStarted { .. }
            | Event::GameStopped { .. }
            | Event::SessionsChanged { .. }
            | Event::LibraryChanged
            | Event::ModsChanged { .. }
            | Event::StreamsChanged
            | Event::MoonlightChanged
            | Event::Reconnected
    )
}

fn print_line(s: &Snapshot) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(s)?)?;
    out.flush()?;
    Ok(())
}

fn print_text(s: &Snapshot) {
    if s.playing.is_empty() {
        println!("nothing playing");
    }
    for p in &s.playing {
        let mins = (Utc::now() - p.started).num_minutes();
        let profile = p.profile.as_ref().map(|p| format!(", profile {p}")).unwrap_or_default();
        let stream = p.stream.as_ref().map(|s| format!(", {} to {} ({})", s.state, s.peer, s.client)).unwrap_or_default();
        println!("playing {} ({}) for {mins} min{profile}{stream}", p.name, p.game_ref);
    }
    if !s.recent.is_empty() {
        println!("recently played:");
        for r in &s.recent {
            println!("  {:<44} {}  {}", r.name, r.last_played.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), r.game_ref);
        }
    }
}

pub async fn run(client: &Client, json: bool, follow: bool, recent: usize) -> Result<()> {
    if !follow {
        let s = snapshot(client, recent).await?;
        if json { print_line(&s)? } else { print_text(&s) }
        return Ok(());
    }
    let mut events = client.subscribe().await?;
    let mut last = snapshot(client, recent).await?;
    print_line(&last)?;
    while let Some(e) = events.recv().await {
        if let Event::Disconnected = e {
            let down = Snapshot { connected: false, playing: Vec::new(), recent: last.recent.clone() };
            if down != last {
                print_line(&down)?;
                last = down;
            }
            continue;
        }
        if !relevant(&e) {
            continue;
        }
        // Take whatever else arrives meanwhile with it.
        tokio::time::sleep(SETTLE).await;
        while let Ok(e) = events.try_recv() {
            if let Event::Disconnected = e {
                break;
            }
        }
        match snapshot(client, recent).await {
            Ok(s) if s != last => {
                print_line(&s)?;
                last = s;
            }
            Ok(_) => {}
            // quiverd went away between the event and the requests: the Disconnected event
            // follows, then Reconnected brings a fresh snapshot.
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_match_the_app() {
        assert_eq!(short_name("Assassin's Creed® Origins"), "AC Origins");
        assert_eq!(short_name("Assassin’s Creed® Valhalla"), "AC Valhalla");
        assert_eq!(short_name("Discovery Tour by Assassin's Creed: Ancient Egypt"), "Discovery Tour: Ancient Egypt");
        assert_eq!(short_name("Far Cry® 6"), "Far Cry 6");
        assert_eq!(short_name("Alan Wake 2"), "Alan Wake 2");
    }

    #[test]
    fn refs_are_what_play_takes() {
        assert_eq!(cli_ref(GameRef::ubisoft(5059)), "5059");
        assert_eq!(cli_ref(GameRef::steam(620)), "steam:620");
    }

    #[test]
    fn lab_sessions_are_not_someone_playing() {
        let s = |holders: &[&str]| ActiveSession { holders: holders.iter().map(|h| h.to_string()).collect(), ..Default::default() };
        assert!(lab_only(&s(&["lab"])));
        assert!(lab_only(&s(&["lab", "lab"])));
        assert!(!lab_only(&s(&["lab", "bridge"])));
        assert!(!lab_only(&s(&["launcher"])));
        assert!(!lab_only(&s(&[])));
    }

    #[test]
    fn streams_match_by_game() {
        let st = StreamSession {
            id: "x".into(),
            target: quiver_api::remote::StreamTarget::Game { game: "epic:abc".into(), profile: None },
            title: "Alan Wake 2".into(),
            game: Some("epic:abc".into()),
            peer: "roth".into(),
            state: StreamState::Streaming,
            started_at: Utc::now(),
            kept_until: None,
            port: 1,
            pid: 1,
            client: StreamClient::Moonlight,
        };
        let g: GameRef = "epic:abc".parse().unwrap();
        let s = stream_for(std::slice::from_ref(&st), g).unwrap();
        assert_eq!((s.peer.as_str(), s.client, s.state), ("roth", "moonlight", "streaming"));
        assert!(stream_for(&[st], GameRef::ubisoft(1)).is_none());
    }

    #[test]
    fn only_session_and_library_events_refresh() {
        assert!(relevant(&Event::SessionsChanged { sessions: vec![] }));
        assert!(relevant(&Event::StreamsChanged));
        assert!(relevant(&Event::Reconnected));
        assert!(!relevant(&Event::SessionChanged { signed_in: None }));
    }
}
