//! The game page's own state and daemon calls: game info, progression, saves, mods,
//! exports, the launch-options form, dialogs and dropdowns. Rendering lives next to this
//! file, one module per tab.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AnyElement, App, AppContext, ClipboardItem, Context, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use serde::Deserialize;
use quiver_api::exports::{ExportKind, ExportState, ExportStatus, RestartReport, SteamExportResult, SteamUser, SteamUsers};
use quiver_api::info::{GameInfo, Progress, RestoreResult, SaveBackup, SavesReport};
use quiver_api::mods::{Layer, ModsState, ProfileRevision, VANILLA};
use quiver_api::{GameSettings, ModsRequest, Profile, Request, Revision, SettingOp};

use crate::app::{Cmd, Command, InputMode, Quiver};
use crate::err_text;
use crate::nav::{Input, Page, Tab};
use crate::source::Source;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, SEMIBOLD, Size, button, icon, mono, soft_shadow};

// ---------------------------------------------------------------------------------------
// State

/// Something fetched from the daemon.
#[derive(Clone, Debug, Default)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Load::Idle)
    }
}

/// `Request::RemotePlaytime`'s reply (Ubisoft's demux play-time record).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RemotePlaytime {
    #[serde(default)]
    pub seconds: Option<u64>,
}

/// A per-launch log in `<prefix>/ubi-run/`.
pub const LOG_FILES: [(&str, &str); 3] =
    [("bridge.log", "Bridge"), ("game.log", "Game (Proton)"), ("bridge-proton.log", "Bridge (Proton)")];

#[derive(Clone, Debug)]
pub struct LogText {
    pub path: PathBuf,
    pub lines: Arc<Vec<SharedString>>,
    pub bytes: u64,
    /// Only the end of a large file is shown.
    pub truncated: bool,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AddonFilter {
    #[default]
    All,
    Owned,
    NotOwned,
}

/// Export actions offered in the game menu and the action bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportAction {
    SteamAdd,
    SteamRemove,
    SteamRestart,
    DesktopAdd,
    DesktopRemove,
}

pub struct KvRow {
    pub key: Entity<TextInput>,
    pub value: Entity<TextInput>,
}

/// The launch-options form: staged edits over the current revision.
pub struct LaunchForm {
    pub game: u32,
    pub base: GameSettings,
    pub proton: Option<String>,
    pub account: Option<String>,
    pub wrappers: Entity<TextInput>,
    pub args: Entity<TextInput>,
    pub env: Vec<KvRow>,
    pub dll: Vec<KvRow>,
    pub message: Entity<TextInput>,
}

/// Unsaved launch-options edits waiting for their form: carried over a self-update
/// relaunch, or seeded by demo mode. Applied over the saved settings when the form is built.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LaunchDraft {
    pub game: u32,
    pub ops: Vec<SettingOp>,
    pub message: String,
}

/// The unsaved-changes bar's buttons: the Launch options tab's first body rows.
pub const LAUNCH_BAR_SAVE: usize = 0;
pub const LAUNCH_BAR_DISCARD: usize = 1;

/// What to start once unsaved launch options are dealt with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayAfter {
    /// Play (or what stands in for it): the daemon's default profile.
    Primary,
    /// Play once with play-menu entry `i` (0: Vanilla).
    With(usize),
}

pub enum Dialog {
    RestoreSettings { game: u32, revision: u32 },
    RestoreSaves { game: u32, backup: SaveBackup },
    DeleteProfile { game: u32, profile: Profile },
    RestoreProfile { game: u32, id: String, name: String, revision: u32 },
    RemoveLayer { game: u32, layer: Layer },
    /// `saves`: the store picked (`None`: follow the advice for the ticked layers).
    NewProfile { game: u32, name: Entity<TextInput>, layers: Vec<(String, String, bool)>, saves: Option<String> },
    SteamRestart { waiting: Vec<String> },
    SteamUser { game: u32, users: Vec<SteamUser> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropdown {
    Proton,
    Account,
}

#[derive(Default)]
pub struct GamePage {
    /// The game this state belongs to (0: none yet).
    pub id: u32,
    pub info: Load<GameInfo>,
    pub progress: Load<Progress>,
    pub remote: Load<RemotePlaytime>,
    pub saves: Load<SavesReport>,
    /// The last saves load included the cloud.
    pub saves_cloud: bool,
    pub mods: Load<ModsState>,
    pub exports: Load<Vec<ExportStatus>>,
    pub protons: Load<Vec<String>>,
    pub launch: Option<LaunchForm>,
    /// Edited forms of other games, kept while the page shows another game.
    pub launch_drafts: HashMap<u32, LaunchForm>,
    /// Edits waiting for their form (see [`LaunchDraft`]).
    pub launch_seeds: HashMap<u32, LaunchDraft>,
    /// The next Play of this game skips the unsaved-edits question (already answered).
    pub play_unguarded: Option<u32>,
    /// Selected revision in the launch-options history.
    pub rev_sel: Option<u32>,
    /// Selected profile on the Mods tab (`vanilla` or a profile id).
    pub prof_sel: Option<String>,
    pub prof_hist: Load<(String, Vec<ProfileRevision>)>,
    pub layer_name: Option<Entity<TextInput>>,
    /// Mods tab: the mod whose versions are shown (its id).
    pub mod_open: Option<String>,
    pub layer_path: Option<Entity<TextInput>>,
    pub log_file: usize,
    pub log: Load<LogText>,
    pub addon_filter: AddonFilter,
    pub dialog: Option<Dialog>,
    /// Focused dialog button (keyboard and controller).
    pub dialog_btn: usize,
    pub dropdown: Option<Dropdown>,
    pub dropdown_sel: usize,
    /// The game menu shows play options (X / the Play arrow) instead of the game menu (Y).
    pub play_menu: bool,
    /// With `play_menu`: the profile chip's menu, which picks the game's default profile
    /// (the daemon's `profile` setting) instead of playing once.
    pub default_menu: bool,
    pub busy: bool,
    /// The game's save stores (Saves tab).
    pub stores: Load<quiver_api::savestores::SaveStoresReport>,
    /// Narrow windows: the game list is open over the page.
    pub side_open: bool,
    /// The hero's art chip menu (Ubisoft's or Steam's art first) is open.
    pub art_menu: bool,
}

impl GamePage {
    fn reset(&mut self, id: u32) {
        let protons = std::mem::take(&mut self.protons);
        let exports = std::mem::take(&mut self.exports);
        // Unsaved launch options belong to their game and outlive the page.
        let mut launch_drafts = std::mem::take(&mut self.launch_drafts);
        if let Some(f) = self.launch.take() {
            launch_drafts.insert(f.game, f);
        }
        let launch_seeds = std::mem::take(&mut self.launch_seeds);
        *self = GamePage { id, protons, exports, launch_drafts, launch_seeds, ..Default::default() };
    }

    pub fn exports_for(&self, id: u32) -> Vec<ExportStatus> {
        self.exports
            .ready()
            .map(|l| l.iter().filter(|e| e.record.product_id == id && e.record.profile_id.is_none()).cloned().collect())
            .unwrap_or_default()
    }

    pub fn export(&self, id: u32, kind: ExportKind) -> Option<ExportStatus> {
        self.exports_for(id).into_iter().find(|e| e.record.kind == kind)
    }

    /// Live profiles of the page's game (empty until loaded).
    pub fn profiles(&self) -> Vec<Profile> {
        self.mods.ready().map(|m| m.profiles.iter().map(|p| p.profile.clone()).collect()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------
// Pure helpers (tested below)

/// The decoded `SettingsGet` reply: the daemon sends `{revision, settings}`; older demo data
/// sent the settings alone.
pub fn settings_from_reply(v: serde_json::Value) -> (GameSettings, Option<u32>) {
    if v.get("settings").is_some() {
        let rev = v.get("revision").and_then(|r| r.as_u64()).map(|r| r as u32);
        let s = serde_json::from_value(v["settings"].clone()).unwrap_or_default();
        (s, rev)
    } else {
        (serde_json::from_value(v).unwrap_or_default(), None)
    }
}

/// Settings as flat `key = value` pairs, the way `quiver set` names them.
pub fn flatten(s: &GameSettings) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(p) = &s.proton {
        m.insert("proton".into(), p.clone());
    }
    if let Some(a) = &s.account {
        m.insert("account".into(), a.clone());
    }
    if let Some(l) = &s.language {
        m.insert("language".into(), l.clone());
    }
    if let Some(p) = &s.profile {
        m.insert("profile".into(), p.clone());
    }
    if !s.wrappers.is_empty() {
        m.insert("wrappers".into(), s.wrappers.join(","));
    }
    if !s.args.is_empty() {
        m.insert("args".into(), s.args.join(" "));
    }
    for (k, v) in &s.env {
        m.insert(format!("env.{k}"), v.clone());
    }
    for (k, v) in &s.dll_overrides {
        m.insert(format!("dll.{k}"), v.clone());
    }
    m
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    Removed(String, String),
    Added(String, String),
}

/// Key-level differences from `a` to `b`.
pub fn diff(a: &GameSettings, b: &GameSettings) -> Vec<DiffLine> {
    let (fa, fb) = (flatten(a), flatten(b));
    let mut keys: Vec<&String> = fa.keys().chain(fb.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out = vec![];
    for k in keys {
        match (fa.get(k), fb.get(k)) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => {
                if let Some(x) = x {
                    out.push(DiffLine::Removed(k.clone(), x.clone()));
                }
                if let Some(y) = y {
                    out.push(DiffLine::Added(k.clone(), y.clone()));
                }
            }
        }
    }
    out
}

/// The ops that turn `base` into `staged`.
pub fn ops_between(base: &GameSettings, staged: &GameSettings) -> Vec<SettingOp> {
    let (fa, fb) = (flatten(base), flatten(staged));
    let mut out = vec![];
    let mut keys: Vec<&String> = fa.keys().chain(fb.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        match (fa.get(k), fb.get(k)) {
            (Some(x), Some(y)) if x == y => {}
            (_, Some(y)) => out.push(SettingOp::Set { key: k.clone(), value: y.clone() }),
            (Some(_), None) => out.push(SettingOp::Unset { key: k.clone() }),
            (None, None) => {}
        }
    }
    out
}

/// What a launch-options form would save: the saved settings with the form's fields.
pub fn form_staged(f: &LaunchForm, cx: &App) -> GameSettings {
    let mut s = f.base.clone();
    s.proton = f.proton.clone();
    s.account = f.account.clone();
    s.wrappers = split_wrappers(f.wrappers.read(cx).text());
    s.args = f.args.read(cx).text().split_whitespace().map(str::to_owned).collect();
    let kv = |rows: &[KvRow]| -> BTreeMap<String, String> {
        rows.iter()
            .map(|r| (r.key.read(cx).text().trim().to_owned(), r.value.read(cx).text().to_owned()))
            .filter(|(k, _)| !k.is_empty())
            .collect()
    };
    s.env = kv(&f.env);
    s.dll_overrides = kv(&f.dll);
    s
}

/// A form's unsaved edits.
pub fn form_ops(f: &LaunchForm, cx: &App) -> Vec<SettingOp> {
    ops_between(&f.base, &form_staged(f, cx))
}

/// The keys edits touch, for a one-line summary: "wrappers, env.MANGOHUD".
pub fn op_keys(ops: &[SettingOp]) -> Vec<String> {
    ops.iter()
        .map(|o| match o {
            SettingOp::Set { key, .. } | SettingOp::Unset { key } => key.clone(),
        })
        .collect()
}

/// A profile in a menu line: "2 layers · Default saves, synced · pinned".
pub fn profile_summary(i: &quiver_api::mods::ProfileInfo) -> String {
    let p = &i.profile;
    let saves = match &i.saves {
        Some(s) => format!("{} saves, {}", s.store_name.as_deref().unwrap_or("own"), if s.cloud { "synced" } else { "this PC only" }),
        None => "saves not migrated".into(),
    };
    format!(
        "{} mod{} · {}{}",
        p.layers.len(),
        if p.layers.len() == 1 { "" } else { "s" },
        saves,
        if p.pinned { " · pinned" } else { "" }
    )
}

/// A size in the unit that reads best: "2 KB", "81 MB", "12.8 GB".
pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_000_000 => format!("{} KB", bytes.div_ceil(1000).max(1)),
        1_000_000..1_000_000_000 => format!("{:.0} MB", bytes as f64 / 1e6),
        _ => crate::model::gb(bytes),
    }
}

/// Quote a shell word if it needs it.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=,:+@%".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The `quiver set` command equivalent to `ops`.
pub fn set_command(id: u32, ops: &[SettingOp], message: Option<&str>) -> String {
    let mut cmd = format!("quiver set {id}");
    for op in ops {
        let word = match op {
            SettingOp::Set { key, value } => format!("{key}={value}"),
            SettingOp::Unset { key } => format!("{key}="),
        };
        cmd.push(' ');
        cmd.push_str(&sh_quote(&word));
    }
    if let Some(m) = message.filter(|m| !m.trim().is_empty()) {
        cmd.push_str(" -m ");
        cmd.push_str(&sh_quote(m.trim()));
    }
    cmd
}

/// Parse a wrappers field like `GameSettings::apply` does.
pub fn split_wrappers(s: &str) -> Vec<String> {
    s.split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// The last `max` bytes of a log, split into lines (lossy UTF-8).
pub fn log_lines(bytes: &[u8], max: usize) -> (Vec<SharedString>, bool) {
    let truncated = bytes.len() > max;
    let tail = &bytes[bytes.len().saturating_sub(max)..];
    let text = String::from_utf8_lossy(tail);
    let mut lines: Vec<SharedString> = text.lines().map(|l| SharedString::from(l.replace('\t', "    "))).collect();
    if truncated && !lines.is_empty() {
        lines.remove(0); // probably cut mid-line
    }
    (lines, truncated)
}

// ---------------------------------------------------------------------------------------
// Loading

/// Game info, progression and Ubisoft's play time ask Ubisoft; past this the page says so
/// instead of loading forever.
const PAGE_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Quiver {
    /// Load what the open game page shows (called with the app's own `load_details`).
    pub fn load_page(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.nav.page != Page::Game(id) {
            return;
        }
        let fresh = self.gp.id != id;
        if fresh {
            self.gp.reset(id);
            self.load_info(id, false, cx);
            self.load_progress(id, false, cx);
            self.load_remote(id, cx);
        }
        let installed = self.game(id).is_some_and(|g| g.installed());
        if installed {
            self.load_mods(id, cx);
        }
        self.load_exports(cx);
        if self.gp.protons.is_idle() {
            self.gp.protons = Load::Loading;
            self.call::<Vec<String>, _>(Request::ProtonList, cx, |this, r, _| {
                this.gp.protons = match r {
                    Ok(p) => Load::Ready(p),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            });
        }
        if !fresh && installed && self.gp.saves.ready().is_some() {
            let cloud = self.gp.saves_cloud;
            self.load_saves(id, cloud, cx);
        }
    }

    pub fn load_info(&mut self, id: u32, refresh: bool, cx: &mut Context<Self>) {
        if self.gp.info.ready().is_none() {
            self.gp.info = Load::Loading;
        }
        self.call_within::<GameInfo, _>(Request::GameInfo { product_id: id, refresh }, PAGE_LOAD_TIMEOUT, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.info = match r {
                    Ok(i) => Load::Ready(i),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub fn load_progress(&mut self, id: u32, refresh: bool, cx: &mut Context<Self>) {
        if self.gp.progress.ready().is_none() {
            self.gp.progress = Load::Loading;
        }
        self.call_within::<Progress, _>(Request::GameProgress { product_id: id, refresh }, PAGE_LOAD_TIMEOUT, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.progress = match r {
                    Ok(p) => Load::Ready(p),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    fn load_remote(&mut self, id: u32, cx: &mut Context<Self>) {
        self.gp.remote = Load::Loading;
        self.call_within::<RemotePlaytime, _>(Request::RemotePlaytime { product_id: id }, PAGE_LOAD_TIMEOUT, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.remote = match r {
                    Ok(p) => Load::Ready(p),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub fn load_saves(&mut self, id: u32, cloud: bool, cx: &mut Context<Self>) {
        if self.gp.saves.ready().is_none() {
            self.gp.saves = Load::Loading;
        }
        self.gp.saves_cloud = cloud;
        self.call::<SavesReport, _>(Request::Saves { product_id: id, cloud }, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.saves = match r {
                    Ok(s) => Load::Ready(s),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub fn load_mods(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.gp.mods.ready().is_none() {
            self.gp.mods = Load::Loading;
        }
        self.call::<ModsState, _>(Request::Mods(ModsRequest::List { product_id: id }), cx, move |this, r, cx| {
            if this.gp.id != id {
                return;
            }
            this.gp.mods = match r {
                Ok(m) => Load::Ready(m),
                Err(e) => Load::Failed(err_text(&e)),
            };
            if let Some(sel) = this.gp.prof_sel.clone().filter(|s| s != VANILLA) {
                this.load_profile_history(id, &sel, cx);
            }
            // Store users and flags change with profiles.
            if !this.gp.stores.is_idle() {
                this.load_stores(id, cx);
            }
        });
    }

    pub fn load_profile_history(&mut self, id: u32, profile: &str, cx: &mut Context<Self>) {
        let p = profile.to_owned();
        self.call::<Vec<ProfileRevision>, _>(
            Request::Mods(ModsRequest::ProfileHistory { product_id: id, profile: p.clone() }),
            cx,
            move |this, r, _| {
                if this.gp.id == id {
                    this.gp.prof_hist = match r {
                        Ok(h) => Load::Ready((p, h)),
                        Err(e) => Load::Failed(err_text(&e)),
                    };
                }
            },
        );
    }

    pub fn load_exports(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<ExportStatus>, _>(Request::ExportList, cx, |this, r, _| match r {
            Ok(l) => this.gp.exports = Load::Ready(l),
            Err(e) => {
                if this.gp.exports.ready().is_none() {
                    this.gp.exports = Load::Failed(err_text(&e));
                }
            }
        });
    }

    /// The selected log of the open game, read from `<prefix>/ubi-run/` (fixtures in demo
    /// mode).
    pub fn load_log(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(prefix) = self.game(id).and_then(|g| g.entry.installed.as_ref()).map(|g| g.prefix.clone()) else {
            self.gp.log = Load::Failed("Not installed".into());
            return;
        };
        let (file, _) = LOG_FILES[self.gp.log_file.min(LOG_FILES.len() - 1)];
        let path = prefix.join("ubi-run").join(file);
        self.gp.log = Load::Loading;
        if self.demo {
            self.gp.log = match crate::demo::detail::log_text(id, file) {
                Some(t) => {
                    let (lines, truncated) = log_lines(t.as_bytes(), 512 * 1024);
                    Load::Ready(LogText { path, lines: Arc::new(lines), bytes: t.len() as u64, truncated, modified: Some(chrono::Utc::now() - chrono::Duration::hours(20)) })
                }
                None => Load::Failed(format!("{} doesn't exist yet", path.display())),
            };
            return;
        }
        let rx = crate::on_tokio(async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<LogText> {
                let meta = std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                let bytes = std::fs::read(&path)?;
                let (lines, truncated) = log_lines(&bytes, 512 * 1024);
                let modified = meta.modified().ok().map(chrono::DateTime::<chrono::Utc>::from);
                Ok(LogText { path, lines: Arc::new(lines), bytes: meta.len(), truncated, modified })
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        });
        cx.spawn(async move |this, cx| {
            let r = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled")));
            this.update(cx, |this, cx| {
                if this.gp.id == id {
                    this.gp.log = match r {
                        Ok(t) => Load::Ready(t),
                        Err(e) => Load::Failed(err_text(&e)),
                    };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // -----------------------------------------------------------------------------------
    // Actions

    /// Open a folder or URL with the desktop's handler. Never runs in demo mode or tests.
    pub fn open_external(&mut self, target: &str, cx: &mut Context<Self>) {
        if self.demo || cfg!(test) {
            self.show_toast(format!("Demo mode: would open {target}"), cx);
            return;
        }
        if !target.contains("://") && !Path::new(target).exists() {
            self.show_toast(format!("{target} doesn't exist yet"), cx);
            return;
        }
        match std::process::Command::new("xdg-open")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => self.show_toast(format!("Could not run xdg-open: {e}"), cx),
        }
    }

    pub fn copy_text(&mut self, text: String, what: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        if what.is_empty() {
            self.show_toast(format!("Copied: {text}"), cx);
        } else {
            self.show_toast(format!("Copied {what}"), cx);
        }
    }

    /// The game's `profile` setting as saved (a profile id; `None`: vanilla). It may name a
    /// deleted profile; [`Self::default_profile`] is what Play really runs.
    pub fn default_profile_setting(&self, id: u32) -> Option<String> {
        self.game(id)?.settings.as_ref()?.profile.clone()
    }

    /// The profile Play runs for `id` when none is named (the daemon resolves the same
    /// way): the game's default, unless it's no longer a live profile.
    pub fn default_profile(&self, id: u32) -> Option<String> {
        let p = self.default_profile_setting(id)?;
        if self.gp.id == id && self.gp.mods.ready().is_some() && !self.gp.profiles().iter().any(|x| x.id == p) {
            return None;
        }
        Some(p)
    }

    /// Play once with profile entry `i` of the play menu (0: vanilla); the default stays.
    pub fn play_with(&mut self, id: u32, i: usize, cx: &mut Context<Self>) {
        let profiles = self.gp.profiles();
        let chosen = if i == 0 { None } else { profiles.get(i - 1).cloned() };
        let label = chosen.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "Vanilla".into());
        let name = self.game(id).map(|g| g.entry.name.clone()).unwrap_or_default();
        if self.game(id).is_some_and(|g| g.running()) {
            self.show_toast(format!("{name} is already running"), cx);
            return;
        }
        if self.launch_guard(id, PlayAfter::With(i), cx) {
            return;
        }
        // Named explicitly, so the daemon doesn't apply the default (`vanilla` forces it).
        let profile = Some(chosen.map(|p| p.id).unwrap_or_else(|| VANILLA.into()));
        self.call::<u64, _>(Request::Play { product_id: id, profile }, cx, move |this, r, cx| match r {
            Ok(_) => this.show_toast(format!("Starting {name} · {label}…"), cx),
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    /// Make profile entry `i` (0: vanilla) the game's default: the `profile` setting, a
    /// revision in its settings history, which Play, `quiver play` and shortcuts follow.
    pub fn set_default_profile(&mut self, id: u32, i: usize, cx: &mut Context<Self>) {
        let profiles = self.gp.profiles();
        let chosen = if i == 0 { None } else { profiles.get(i - 1).cloned() };
        let label = chosen.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "Vanilla".into());
        let value = chosen.as_ref().map(|p| p.id.clone());
        if self.default_profile_setting(id) == value {
            self.show_toast(format!("Play already uses {label}"), cx);
            return;
        }
        let ops = vec![SettingOp::Set { key: "profile".into(), value: value.unwrap_or_else(|| VANILLA.into()) }];
        let message = Some(format!("Default for Play: {label}"));
        self.call::<Revision, _>(Request::SettingsSet { product_id: id, ops, message }, cx, move |this, r, cx| match r {
            Ok(rev) => {
                this.show_toast(format!("Play now uses {label} (launch options r{})", rev.revision), cx);
                this.load_details(id, cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    /// The profile chip's menu: pick the game's default profile.
    fn default_menu_commands(&self, id: u32) -> Vec<Command> {
        let current = self.default_profile(id);
        let entry = |i: usize, key: Option<&str>, name: &str, what: String| {
            let on = current.as_deref() == key;
            Command {
                icon: if on { Icon::CircleCheck } else if key.is_some() { Icon::Layers } else { Icon::ShieldCheck },
                label: name.to_owned().into(),
                detail: Some(if on { format!("Default for Play · {what}") } else { what }.into()),
                cli: Some(format!("quiver mods {id} default {}", key.map(|_| sh_quote(name)).unwrap_or_else(|| VANILLA.into())).into()),
                action: Cmd::DefaultProfile(id, i),
                future: false,
            }
        };
        let mut v = vec![entry(0, None, "Vanilla", "The base install, the game's own saves".into())];
        let deleted = self.default_profile_setting(id).filter(|_| current.is_none());
        if let Some(m) = self.gp.mods.ready().filter(|_| self.gp.id == id) {
            for (i, p) in m.profiles.iter().enumerate() {
                let p = &p.profile;
                v.push(entry(i + 1, Some(p.id.as_str()), &p.name, profile_summary(&m.profiles[i])));
            }
            if let Some(d) = deleted.and_then(|d| m.deleted.iter().find(|x| x.profile.id == d)) {
                v[0].detail = Some(format!("Default for Play · the default, {}, was deleted", d.profile.name).into());
            }
        }
        v
    }

    /// The play menu (X, or the Play button's arrow): play once with Vanilla or a profile.
    /// From the profile chip it's the default-profile menu instead.
    pub fn play_menu_commands(&self, id: u32) -> Option<Vec<Command>> {
        if !self.gp.play_menu {
            return None;
        }
        if self.gp.default_menu {
            return Some(self.default_menu_commands(id));
        }
        let current = self.default_profile(id);
        let mods = self.gp.mods.ready().filter(|_| self.gp.id == id);
        let is_default = |on: bool| if on { " · default" } else { "" };
        let mut v = vec![Command {
            icon: Icon::Play,
            label: "Play Vanilla".into(),
            detail: Some(format!("The base install, the game's own saves{}", is_default(current.is_none())).into()),
            cli: Some(format!("quiver play {id} --vanilla").into()),
            action: Cmd::PlayWith(id, 0),
            future: false,
        }];
        if let Some(m) = mods {
            for (i, p) in m.profiles.iter().enumerate() {
                let p = &p.profile;
                v.push(Command {
                    icon: Icon::Layers,
                    label: format!("Play {}", p.name).into(),
                    detail: Some(format!("{}{}", profile_summary(&m.profiles[i]), is_default(current.as_deref() == Some(p.id.as_str()))).into()),
                    cli: Some(format!("quiver play {id} --profile {}", sh_quote(&p.name)).into()),
                    action: Cmd::PlayWith(id, i + 1),
                    future: false,
                });
            }
            if !m.profiles.is_empty() {
                v.push(Command {
                    icon: Icon::CircleCheck,
                    label: "Change the default…".into(),
                    detail: Some("What Play, shortcuts and `quiver play` use".into()),
                    cli: Some(format!("quiver mods {id} default PROFILE").into()),
                    action: Cmd::DefaultMenu(id),
                    future: false,
                });
            }
        }
        Some(v)
    }

    /// Steam, .desktop and Verify entries for the game menu, with their state.
    pub fn export_commands(&self, id: u32) -> Vec<Command> {
        let installed = self.game(id).is_some_and(|g| g.installed());
        let steam = self.gp.export(id, ExportKind::Steam);
        let desktop = self.gp.export(id, ExportKind::Desktop);
        let mut v = vec![];
        let state = |e: &Option<ExportStatus>| match e.as_ref().map(|e| e.state) {
            Some(ExportState::Ok) => "added",
            Some(ExportState::WaitingForSteam) => "waiting for a Steam restart",
            Some(ExportState::Missing) => "deleted outside Quiver",
            Some(ExportState::Changed) => "edited outside Quiver",
            None => "",
        };
        match steam.as_ref().map(|e| e.state) {
            Some(ExportState::Ok) | Some(ExportState::WaitingForSteam) => {
                if steam.as_ref().is_some_and(|e| e.state == ExportState::WaitingForSteam) {
                    v.push(Command {
                        icon: Icon::RefreshCw,
                        label: "Restart Steam…".into(),
                        detail: Some("Shows the waiting shortcut; asks first".into()),
                        cli: Some("quiver steam restart".into()),
                        action: Cmd::Export(id, ExportAction::SteamRestart),
                        future: false,
                    });
                }
                v.push(Command {
                    icon: Icon::Gamepad,
                    label: "Remove from Steam".into(),
                    detail: Some(format!("Steam shortcut {}", state(&steam)).into()),
                    cli: Some(format!("quiver export steam {id} --remove").into()),
                    action: Cmd::Export(id, ExportAction::SteamRemove),
                    future: false,
                });
            }
            _ => v.push(Command {
                icon: Icon::Gamepad,
                label: if steam.is_some() { "Add to Steam again".into() } else { "Add to Steam".into() },
                detail: Some(if steam.is_some() { format!("Shortcut {}", state(&steam)).into() } else { "Non-Steam shortcut with Ubisoft art".into() }),
                cli: Some(format!("quiver export steam {id}").into()),
                action: if installed { Cmd::Export(id, ExportAction::SteamAdd) } else { Cmd::NotYet },
                future: !installed,
            }),
        }
        match desktop.as_ref().map(|e| e.state) {
            Some(ExportState::Ok) => v.push(Command {
                icon: Icon::AppWindow,
                label: "Remove .desktop entry".into(),
                detail: Some(format!("quiver-{id}.desktop in the app launcher").into()),
                cli: Some(format!("quiver export desktop {id} --remove").into()),
                action: Cmd::Export(id, ExportAction::DesktopRemove),
                future: false,
            }),
            _ => v.push(Command {
                icon: Icon::AppWindow,
                label: if desktop.is_some() { "Create .desktop entry again".into() } else { "Create .desktop entry".into() },
                detail: Some(if desktop.is_some() { format!("Entry {}", state(&desktop)).into() } else { "Show it in the app launcher".into() }),
                cli: Some(format!("quiver export desktop {id}").into()),
                action: if installed { Cmd::Export(id, ExportAction::DesktopAdd) } else { Cmd::NotYet },
                future: !installed,
            }),
        }
        if installed {
            v.push(Command {
                icon: Icon::ShieldCheck,
                label: "Verify files…".into(),
                detail: Some("Hash the install against its manifest; repair only what's bad".into()),
                cli: Some(format!("quiver verify {id}").into()),
                action: Cmd::Downloads(crate::ui::downloads::DlCmd::Verify(id)),
                future: false,
            });
        }
        v
    }

    /// The action bar's Steam button.
    pub fn steam_button(&mut self, id: u32, cx: &mut Context<Self>) {
        // A Steam copy: switch it to Quiver, or show its options.
        if let Some(g) = self.game(id).filter(|g| g.steam_only()).and_then(|g| g.steam().cloned()) {
            let copy = self.steam_copy(id).unwrap_or(g);
            match crate::ui::steamgames::control(&copy) {
                Some(true) => self.steam_patch(copy.app_id, false, cx),
                _ => {
                    self.gp.play_menu = false;
                    self.nav.open_menu(id);
                }
            }
            return;
        }
        match self.gp.export(id, ExportKind::Steam).map(|e| e.state) {
            None | Some(ExportState::Missing) | Some(ExportState::Changed) => self.run_export(id, ExportAction::SteamAdd, cx),
            Some(ExportState::WaitingForSteam) => self.run_export(id, ExportAction::SteamRestart, cx),
            Some(ExportState::Ok) => {
                self.gp.play_menu = false;
                self.nav.open_menu(id);
            }
        }
    }

    pub fn run_export(&mut self, id: u32, a: ExportAction, cx: &mut Context<Self>) {
        let name = self.game(id).map(|g| g.entry.name.clone()).unwrap_or_default();
        match a {
            ExportAction::SteamAdd => {
                self.call::<SteamUsers, _>(Request::SteamUsers, cx, move |this, r, cx| match r {
                    Ok(u) if u.root.is_none() => this.show_toast("Steam isn't installed (no ~/.local/share/Steam)", cx),
                    Ok(u) if u.selected.is_none() && u.users.len() > 1 => {
                        this.open_dialog(Dialog::SteamUser { game: id, users: u.users });
                    }
                    Ok(_) => this.export_steam(id, None, cx),
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            ExportAction::SteamRemove => {
                self.call::<SteamExportResult, _>(Request::ExportSteam { product_id: id, remove: true, steam_user: None }, cx, move |this, r, cx| {
                    match r {
                        Ok(r) if r.staged => this.show_toast(format!("{name} leaves Steam when Steam next restarts"), cx),
                        Ok(_) => this.show_toast(format!("Removed {name} from Steam"), cx),
                        Err(e) => this.show_toast(err_text(&e), cx),
                    }
                    this.load_exports(cx);
                });
            }
            ExportAction::SteamRestart => {
                let waiting: Vec<String> = self
                    .gp
                    .exports
                    .ready()
                    .map(|l| l.iter().filter(|e| e.state == ExportState::WaitingForSteam).map(|e| e.record.name.clone()).collect())
                    .unwrap_or_default();
                self.open_dialog(Dialog::SteamRestart { waiting });
            }
            ExportAction::DesktopAdd | ExportAction::DesktopRemove => {
                let remove = a == ExportAction::DesktopRemove;
                self.call::<serde_json::Value, _>(Request::ExportDesktop { product_id: id, remove }, cx, move |this, r, cx| {
                    match r {
                        Ok(v) => {
                            let path = v.get("path").and_then(|p| p.as_str()).map(|p| crate::model::tilde(Path::new(p))).unwrap_or_default();
                            this.show_toast(if remove { format!("Removed {path}") } else { format!("Created {path}") }, cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    }
                    this.load_exports(cx);
                });
            }
        }
    }

    fn export_steam(&mut self, id: u32, user: Option<String>, cx: &mut Context<Self>) {
        let name = self.game(id).map(|g| g.entry.name.clone()).unwrap_or_default();
        self.call::<SteamExportResult, _>(Request::ExportSteam { product_id: id, remove: false, steam_user: user }, cx, move |this, r, cx| {
            match r {
                Ok(r) if r.staged => {
                    this.load_exports(cx);
                    // Steam only reads shortcuts at start: offer (never force) a restart.
                    this.open_dialog(Dialog::SteamRestart { waiting: vec![name] });
                }
                Ok(_) => {
                    this.show_toast(format!("Added {name} to Steam"), cx);
                    this.load_exports(cx);
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            }
        });
    }

    /// "Restart Steam" in a confirmation dialog: the person asked for it just now. quiverd
    /// does it (it knows what's staged), so this app's version doesn't matter.
    pub fn restart_steam(&mut self, cx: &mut Context<Self>) {
        let demo = matches!(self.source, Some(Source::Demo(_)));
        if !demo {
            self.show_toast("Closing Steam…", cx);
        }
        self.call::<RestartReport, _>(Request::SteamRestart { confirmed: true }, cx, move |this, r, cx| {
            let msg = match r {
                Ok(r) if !r.was_running => format!("Steam isn't running; {} staged change(s) written", r.applied),
                Ok(r) => format!("Steam restarted; {} staged change(s) written{}", r.applied, if demo { " (demo)" } else { "" }),
                Err(e) => format!("Steam restart failed: {}", err_text(&e)),
            };
            this.show_toast(msg, cx);
            this.load_exports(cx);
            this.load_global_exports(cx);
        });
    }

    // -----------------------------------------------------------------------------------
    // Launch options

    fn text_input(&self, placeholder: &str, text: &str, mono: bool, cx: &mut Context<Self>) -> Entity<TextInput> {
        let text = text.to_owned();
        let e = cx.new(|cx| {
            let mut t = TextInput::new(cx, placeholder, false).compact(mono);
            t.set_text(text, cx);
            t
        });
        cx.observe(&e, |_, _, cx| cx.notify()).detach();
        e
    }

    fn kv_rows(&self, m: &BTreeMap<String, String>, kp: &str, vp: &str, cx: &mut Context<Self>) -> Vec<KvRow> {
        m.iter()
            .map(|(k, v)| KvRow { key: self.text_input(kp, k, true, cx), value: self.text_input(vp, v, true, cx) })
            .collect()
    }

    /// (Re)build the form when there's none, it's for another game, or the saved settings
    /// changed under an unedited form. Another game's edited form is kept for later.
    pub fn ensure_launch_form(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(saved) = self.game(id).and_then(|g| g.settings.clone()) else { return };
        if self.gp.launch.as_ref().is_some_and(|f| f.game != id)
            && let Some(f) = self.gp.launch.take()
        {
            self.gp.launch_drafts.insert(f.game, f);
        }
        if self.gp.launch.is_none()
            && let Some(f) = self.gp.launch_drafts.remove(&id)
        {
            self.gp.launch = Some(f);
        }
        let rebuild = match &self.gp.launch {
            None => true,
            Some(f) => f.base != saved && form_ops(f, cx).is_empty(),
        };
        if !rebuild {
            return;
        }
        // Edits carried over a relaunch (or seeded by demo mode) go over the saved settings.
        let seed = self.gp.launch_seeds.remove(&id);
        let mut staged = saved.clone();
        for op in seed.iter().flat_map(|d| &d.ops) {
            let _ = staged.apply(op);
        }
        let f = LaunchForm {
            game: id,
            proton: staged.proton.clone(),
            account: staged.account.clone(),
            wrappers: self.text_input("gamemoderun, mangohud", &staged.wrappers.join(", "), true, cx),
            args: self.text_input("Extra arguments", &staged.args.join(" "), true, cx),
            env: self.kv_rows(&staged.env, "NAME", "value", cx),
            dll: self.kv_rows(&staged.dll_overrides, "dll", "n,b", cx),
            message: self.text_input("Why (optional), e.g. \"Turn on HDR\"", seed.as_ref().map(|d| d.message.as_str()).unwrap_or(""), false, cx),
            base: saved,
        };
        self.gp.launch = Some(f);
    }

    /// The form holding `id`'s launch options, if one was built (open, or kept for later).
    pub fn launch_form(&self, id: u32) -> Option<&LaunchForm> {
        self.gp.launch.as_ref().filter(|f| f.game == id).or_else(|| self.gp.launch_drafts.get(&id))
    }

    pub fn launch_staged(&self, cx: &App) -> Option<GameSettings> {
        self.gp.launch.as_ref().map(|f| form_staged(f, cx))
    }

    pub fn launch_ops(&self, cx: &App) -> Vec<SettingOp> {
        self.gp.launch.as_ref().map(|f| form_ops(f, cx)).unwrap_or_default()
    }

    /// Unsaved launch-options edits of game `id`, wherever they are: its form (open, or
    /// kept while another game shows) or carried-over edits that have no form yet.
    pub fn launch_pending(&self, id: u32, cx: &App) -> Vec<SettingOp> {
        if let Some(f) = self.launch_form(id) {
            return form_ops(f, cx);
        }
        let Some(d) = self.gp.launch_seeds.get(&id) else { return vec![] };
        match self.game(id).and_then(|g| g.settings.as_ref()) {
            Some(saved) => {
                let mut staged = saved.clone();
                for op in &d.ops {
                    let _ = staged.apply(op);
                }
                ops_between(saved, &staged)
            }
            None => d.ops.clone(),
        }
    }

    /// Every game's unsaved launch-options edits, for a relaunch to carry over.
    pub fn launch_drafts_all(&self, cx: &App) -> Vec<LaunchDraft> {
        let mut games: Vec<u32> = self
            .gp
            .launch
            .iter()
            .map(|f| f.game)
            .chain(self.gp.launch_drafts.keys().copied())
            .chain(self.gp.launch_seeds.keys().copied())
            .collect();
        games.sort_unstable();
        games.dedup();
        games
            .into_iter()
            .filter_map(|game| {
                let ops = self.launch_pending(game, cx);
                let message = self.launch_message(game, cx).unwrap_or_default();
                (!ops.is_empty()).then_some(LaunchDraft { game, ops, message })
            })
            .collect()
    }

    /// The "why" typed for game `id`'s unsaved edits.
    fn launch_message(&self, id: u32, cx: &App) -> Option<String> {
        self.launch_form(id)
            .map(|f| f.message.read(cx).text().trim().to_owned())
            .or_else(|| self.gp.launch_seeds.get(&id).map(|d| d.message.clone()))
            .filter(|m| !m.is_empty())
    }

    pub fn launch_add_row(&mut self, dll: bool, cx: &mut Context<Self>) {
        let row = if dll {
            KvRow { key: self.text_input("dll", "", true, cx), value: self.text_input("n,b", "n,b", true, cx) }
        } else {
            KvRow { key: self.text_input("NAME", "", true, cx), value: self.text_input("value", "", true, cx) }
        };
        if let Some(f) = &mut self.gp.launch {
            if dll { f.dll.push(row) } else { f.env.push(row) }
        }
    }

    /// Drop game `id`'s unsaved launch options.
    pub fn launch_discard(&mut self, id: u32) {
        if self.gp.launch.as_ref().is_some_and(|f| f.game == id) {
            self.gp.launch = None;
        }
        self.gp.launch_drafts.remove(&id);
        self.gp.launch_seeds.remove(&id);
    }

    /// Save game `id`'s edits as a new revision; then start it, if asked to.
    pub fn launch_save(&mut self, id: u32, then: Option<PlayAfter>, cx: &mut Context<Self>) {
        let ops = self.launch_pending(id, cx);
        if self.gp.busy {
            return;
        }
        if ops.is_empty() {
            if let Some(p) = then {
                self.play_after(id, p, cx);
            }
            return;
        }
        let message = self.launch_message(id, cx);
        self.gp.busy = true;
        self.call::<Revision, _>(Request::SettingsSet { product_id: id, ops, message }, cx, move |this, r, cx| {
            this.gp.busy = false;
            match r {
                Ok(rev) => {
                    this.show_toast(format!("Saved launch options as r{}", rev.revision), cx);
                    this.launch_discard(id);
                    this.gp.rev_sel = None;
                    this.load_details(id, cx);
                    if let Some(p) = then {
                        this.play_after(id, p, cx);
                    }
                }
                // Not saved: don't start the game without them either.
                Err(e) => this.show_toast(err_text(&e), cx),
            }
        });
    }

    pub fn restore_settings(&mut self, id: u32, revision: u32, cx: &mut Context<Self>) {
        self.call::<Revision, _>(Request::SettingsRestore { product_id: id, revision }, cx, move |this, r, cx| match r {
            Ok(rev) => {
                this.show_toast(format!("Restored r{revision} as r{}", rev.revision), cx);
                this.launch_discard(id);
                this.gp.rev_sel = Some(rev.revision);
                this.load_details(id, cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    // -----------------------------------------------------------------------------------
    // Mods

    pub fn ensure_mods_inputs(&mut self, cx: &mut Context<Self>) {
        if self.gp.layer_name.is_none() {
            self.gp.layer_name = Some(self.text_input("Mod name, e.g. ReShade", "", false, cx));
            self.gp.layer_path = Some(self.text_input("/home/you/mods/reshade (laid out like the install folder)", "", true, cx));
        }
    }

    pub fn select_profile(&mut self, id: u32, profile: Option<String>, cx: &mut Context<Self>) {
        let p = profile.unwrap_or_else(|| VANILLA.into());
        if p != VANILLA {
            self.load_profile_history(id, &p, cx);
        }
        self.gp.prof_hist = Load::Idle;
        self.gp.prof_sel = Some(p);
    }

    /// Edit a profile (`ProfileSet`) and reload.
    pub fn profile_set(&mut self, id: u32, profile: &str, ops: Vec<SettingOp>, cx: &mut Context<Self>) {
        let p = profile.to_owned();
        self.call::<ProfileRevision, _>(
            Request::Mods(ModsRequest::ProfileSet { product_id: id, profile: p.clone(), ops, message: None }),
            cx,
            move |this, r, cx| match r {
                Ok(rev) => {
                    this.load_mods(id, cx);
                    this.load_profile_history(id, &p, cx);
                    let _ = rev;
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            },
        );
    }

    pub fn add_layer(&mut self, id: u32, cx: &mut Context<Self>) {
        let (Some(n), Some(p)) = (&self.gp.layer_name, &self.gp.layer_path) else { return };
        let name = n.read(cx).text().trim().to_owned();
        let path = p.read(cx).text().trim().to_owned();
        let path = match path.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
            None => PathBuf::from(path),
        };
        if path.as_os_str().is_empty() {
            self.show_toast("Enter the folder to copy (laid out like the install folder)", cx);
            return;
        }
        let name = if name.is_empty() { path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default() } else { name };
        let (ni, pi) = (n.clone(), p.clone());
        self.show_toast(format!("Copying {} into the mod store…", crate::model::tilde(&path)), cx);
        self.call::<Layer, _>(Request::Mods(ModsRequest::LayerAdd { product_id: id, name, path, deletes: Vec::new() }), cx, move |this, r, cx| match r {
            Ok(l) => {
                this.show_toast(format!("Added layer {} ({} files)", l.name, l.files), cx);
                ni.update(cx, |t, cx| t.set_text("", cx));
                pi.update(cx, |t, cx| t.set_text("", cx));
                this.load_mods(id, cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    pub fn new_profile_dialog(&mut self, id: u32, cx: &mut Context<Self>) {
        let layers = self
            .gp
            .mods
            .ready()
            .map(|m| m.all_mods().iter().map(|r| (r.id.clone(), r.name.clone(), false)).collect())
            .unwrap_or_default();
        let name = self.text_input("Profile name, e.g. Visual overhaul", "", false, cx);
        self.open_dialog(Dialog::NewProfile { game: id, name, layers, saves: None });
    }

    // -----------------------------------------------------------------------------------
    // Dialogs

    pub fn open_dialog(&mut self, d: Dialog) {
        self.gp.dropdown = None;
        // Focus starts on the safe choice.
        self.gp.dialog_btn = 0;
        self.gp.dialog = Some(d);
    }

    /// The buttons of the open dialog, cancel first.
    pub fn dialog_buttons(&self) -> Vec<(SharedString, bool)> {
        match &self.gp.dialog {
            None => vec![],
            Some(Dialog::SteamRestart { .. }) => vec![("Not now".into(), false), ("Restart Steam".into(), true)],
            Some(Dialog::SteamUser { users, .. }) => {
                let mut v: Vec<(SharedString, bool)> = vec![("Cancel".into(), false)];
                for u in users {
                    v.push((u.persona_name.clone().unwrap_or_else(|| u.id.clone()).into(), true));
                }
                v
            }
            Some(Dialog::RestoreSettings { revision, .. }) => vec![("Cancel".into(), false), (format!("Restore r{revision}").into(), true)],
            Some(Dialog::RestoreSaves { .. }) => vec![("Cancel".into(), false), ("Restore backup".into(), true)],
            Some(Dialog::DeleteProfile { .. }) => vec![("Cancel".into(), false), ("Delete profile".into(), true)],
            Some(Dialog::RestoreProfile { revision, .. }) => vec![("Cancel".into(), false), (format!("Restore r{revision}").into(), true)],
            Some(Dialog::RemoveLayer { .. }) => vec![("Cancel".into(), false), ("Remove layer".into(), true)],
            Some(Dialog::NewProfile { .. }) => vec![("Cancel".into(), false), ("Create profile".into(), true)],
        }
    }

    /// Press dialog button `i` (0: cancel / not now).
    pub fn dialog_press(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(d) = self.gp.dialog.take() else { return };
        if i == 0 {
            if let Dialog::SteamRestart { .. } = d {
                self.show_toast("Not restarting Steam; the shortcut appears after Steam next restarts", cx);
            }
            return;
        }
        match d {
            Dialog::SteamRestart { .. } => self.restart_steam(cx),
            Dialog::SteamUser { game, users } => {
                if let Some(u) = users.get(i - 1) {
                    self.export_steam(game, Some(u.id.clone()), cx);
                }
            }
            Dialog::RestoreSettings { game, revision } => self.restore_settings(game, revision, cx),
            Dialog::RestoreSaves { game, backup } => {
                // The person confirmed in this dialog, so the request says so.
                let bid = backup.id.clone();
                self.call::<RestoreResult, _>(Request::SavesRestore { product_id: game, backup: bid, confirmed: true }, cx, move |this, r, cx| {
                    match r {
                        Ok(r) => {
                            let prev = r.previous.map(|p| format!("; the saves that were there are in backup {}", p.id)).unwrap_or_default();
                            this.show_toast(format!("Restored slot(s) {:?}{prev}", r.restored), cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    }
                    let cloud = this.gp.saves_cloud;
                    this.load_saves(game, cloud, cx);
                });
            }
            Dialog::DeleteProfile { game, profile } => {
                let name = profile.name.clone();
                self.call::<ProfileRevision, _>(
                    Request::Mods(ModsRequest::ProfileDelete { product_id: game, profile: profile.id.clone(), confirmed: true }),
                    cx,
                    move |this, r, cx| match r {
                        Ok(rev) => {
                            let was_default = this.default_profile_setting(game).as_deref() == Some(rev.profile.id.as_str());
                            let then = if was_default { "; Play uses Vanilla until it's back" } else { "" };
                            this.show_toast(format!("Deleted {name}; restore it from Deleted profiles (r{}){then}", rev.revision - 1), cx);
                            this.gp.prof_sel = None;
                            this.load_mods(game, cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    },
                );
            }
            Dialog::RestoreProfile { game, id, name, revision } => {
                self.call::<ProfileRevision, _>(
                    Request::Mods(ModsRequest::ProfileRestore { product_id: game, profile: id.clone(), revision, confirmed: true }),
                    cx,
                    move |this, r, cx| match r {
                        Ok(rev) => {
                            this.show_toast(format!("Restored {name} r{revision} as r{}", rev.revision), cx);
                            this.gp.prof_sel = Some(id.clone());
                            this.load_mods(game, cx);
                            this.load_profile_history(game, &id, cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    },
                );
            }
            Dialog::RemoveLayer { game, layer } => {
                let name = layer.name.clone();
                self.call::<Layer, _>(
                    Request::Mods(ModsRequest::LayerRemove { product_id: game, layer: layer.id, confirmed: true }),
                    cx,
                    move |this, r, cx| match r {
                        Ok(_) => {
                            this.show_toast(format!("Removed layer {name}"), cx);
                            this.load_mods(game, cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    },
                );
            }
            Dialog::NewProfile { game, name, layers, saves } => {
                let n = name.read(cx).text().trim().to_owned();
                if n.is_empty() {
                    self.show_toast("A profile needs a name", cx);
                    self.gp.dialog = Some(Dialog::NewProfile { game, name, layers, saves });
                    self.gp.dialog_btn = 1;
                    return;
                }
                let chosen: Vec<String> = layers.iter().filter(|l| l.2).map(|l| l.0.clone()).collect();
                self.call::<ProfileRevision, _>(
                    Request::Mods(ModsRequest::ProfileCreate { product_id: game, name: n, layers: chosen, message: None, saves }),
                    cx,
                    move |this, r, cx| match r {
                        Ok(rev) => {
                            this.show_toast(format!("Created {} ({})", rev.profile.name, rev.profile.id), cx);
                            this.load_mods(game, cx);
                            this.select_profile(game, Some(rev.profile.id), cx);
                        }
                        Err(e) => this.show_toast(err_text(&e), cx),
                    },
                );
            }
        }
    }

    /// Keyboard and controller input while a dialog or dropdown is open. True: consumed.
    pub fn page_input(&mut self, input: Input, cx: &mut Context<Self>) -> bool {
        if !matches!(self.nav.page, Page::Game(_)) {
            self.gp.dialog = None;
            self.gp.dropdown = None;
        }
        if self.gp.dialog.is_some() {
            let n = self.dialog_buttons().len().max(1);
            match input {
                Input::Left | Input::Up => self.gp.dialog_btn = (self.gp.dialog_btn + n - 1) % n,
                Input::Right | Input::Down => self.gp.dialog_btn = (self.gp.dialog_btn + 1) % n,
                Input::Activate => {
                    let i = self.gp.dialog_btn.min(n - 1);
                    self.dialog_press(i, cx);
                }
                Input::Back => self.dialog_press(0, cx),
                _ => {}
            }
            return true;
        }
        if let Some(dd) = self.gp.dropdown {
            let opts = self.dropdown_options(dd);
            let n = opts.len().max(1);
            match input {
                Input::Up | Input::Left => self.gp.dropdown_sel = (self.gp.dropdown_sel + n - 1) % n,
                Input::Down | Input::Right => self.gp.dropdown_sel = (self.gp.dropdown_sel + 1) % n,
                Input::Activate => {
                    let v = opts.get(self.gp.dropdown_sel).map(|o| o.0.clone());
                    if let Some(v) = v {
                        self.dropdown_pick(dd, v);
                    }
                }
                Input::Back => self.gp.dropdown = None,
                _ => {}
            }
            return true;
        }
        // X opens play options, Y the game menu (both open the same overlay).
        if self.nav.overlay.is_none() {
            match input {
                Input::PlayOptions => (self.gp.play_menu, self.gp.default_menu) = (true, false),
                Input::Menu => (self.gp.play_menu, self.gp.default_menu) = (false, false),
                _ => {}
            }
        }
        false
    }

    /// (value, label, detail) options of a dropdown. `None` values unset the key.
    pub fn dropdown_options(&self, dd: Dropdown) -> Vec<(Option<String>, String, String)> {
        match dd {
            Dropdown::Proton => {
                let def = self.status.as_ref().and_then(|s| s.proton_default.clone()).unwrap_or_else(|| "newest installed".into());
                let mut v = vec![(None, "Global default".to_string(), def)];
                for p in self.gp.protons.ready().cloned().unwrap_or_default() {
                    v.push((Some(p), String::new(), "compatibilitytools.d".into()));
                }
                // Keep a saved build that's no longer installed visible.
                if let Some(cur) = self.gp.launch.as_ref().and_then(|f| f.proton.clone())
                    && !v.iter().any(|o| o.0.as_deref() == Some(cur.as_str()))
                {
                    v.push((Some(cur), String::new(), "not installed".into()));
                }
                v
            }
            Dropdown::Account => {
                let accounts = self.status.as_ref().map(|s| s.accounts.clone()).unwrap_or_default();
                let owners = self.game(self.gp.id).map(|g| g.entry.owned_by.clone()).unwrap_or_default();
                let name_of = |id: &str| accounts.iter().find(|a| a.user_id == id).map(|a| a.name.clone()).unwrap_or_else(|| id.to_owned());
                let default = accounts.iter().find(|a| a.default).map(|a| a.name.clone());
                let mut v = vec![(None, "Default account".to_string(), default.map(|d| format!("{d}, if it owns the game")).unwrap_or_default())];
                for o in owners {
                    v.push((Some(o.clone()), name_of(&o), "owns it · its cloud saves".into()));
                }
                v
            }
        }
    }

    pub fn dropdown_pick(&mut self, dd: Dropdown, v: Option<String>) {
        if let Some(f) = &mut self.gp.launch {
            match dd {
                Dropdown::Proton => f.proton = v,
                Dropdown::Account => f.account = v,
            }
        }
        self.gp.dropdown = None;
    }

    // -----------------------------------------------------------------------------------
    // Controller rows in each tab's body

    /// Body rows the unsaved-changes bar adds in front of the history (Save, Discard).
    pub fn launch_bar_rows(&self, id: u32, cx: &App) -> usize {
        match self.launch_form(id) {
            Some(f) if !form_ops(f, cx).is_empty() => 2,
            _ => 0,
        }
    }

    pub fn page_body_len(&self, id: u32, tab: Tab, cx: &App) -> usize {
        let Some(g) = self.game(id) else { return 0 };
        match tab {
            Tab::Overview => g.playtime.as_ref().map(|p| p.sessions.len().min(8)).unwrap_or(0),
            Tab::LaunchOptions => self.launch_bar_rows(id, cx) + g.history.as_ref().map(|h| h.len()).unwrap_or(0),
            Tab::Mods => {
                if !g.installed() {
                    0
                } else {
                    2 + self.gp.profiles().len()
                }
            }
            Tab::Saves => self.gp.saves.ready().map(|s| s.backups.len()).unwrap_or(0),
            Tab::AddOns => self.addons_shown().len(),
            Tab::Files => {
                if g.installed() {
                    4
                } else {
                    0
                }
            }
            Tab::Logs => {
                if g.installed() {
                    LOG_FILES.len()
                } else {
                    0
                }
            }
        }
    }

    pub fn page_body_activate(&mut self, id: u32, i: usize, cx: &mut Context<Self>) {
        match self.nav.tab {
            Tab::Overview => {}
            Tab::LaunchOptions => {
                let bar = self.launch_bar_rows(id, cx);
                if i < bar {
                    match i {
                        LAUNCH_BAR_SAVE => self.launch_save(id, None, cx),
                        _ => self.launch_discard(id),
                    }
                    return;
                }
                let i = i - bar;
                let mut revs: Vec<u32> = self.game(id).and_then(|g| g.history.as_ref()).map(|h| h.iter().map(|r| r.revision).collect()).unwrap_or_default();
                revs.sort_unstable_by(|a, b| b.cmp(a));
                if let Some(r) = revs.get(i).copied() {
                    let current = revs.first().copied();
                    if self.gp.rev_sel == Some(r) && Some(r) != current {
                        self.open_dialog(Dialog::RestoreSettings { game: id, revision: r });
                    } else {
                        self.gp.rev_sel = Some(r);
                    }
                }
            }
            Tab::Mods => {
                let profiles = self.gp.profiles();
                if i == 0 {
                    self.select_profile(id, None, cx);
                } else if let Some(p) = profiles.get(i - 1) {
                    let pid = p.id.clone();
                    self.select_profile(id, Some(pid), cx);
                } else {
                    self.new_profile_dialog(id, cx);
                }
            }
            Tab::Saves => {
                if let Some(b) = self.gp.saves.ready().and_then(|s| s.backups.get(i)).cloned() {
                    self.open_dialog(Dialog::RestoreSaves { game: id, backup: b });
                }
            }
            Tab::AddOns => {
                if let Some(url) = self.addons_shown().get(i).and_then(|a| a.store_url.clone()) {
                    self.open_external(&url, cx);
                }
            }
            Tab::Files => {
                if let Some(p) = self.file_locations(id).get(i).map(|l| l.1.clone()) {
                    self.open_external(&p.to_string_lossy(), cx);
                }
            }
            Tab::Logs => {
                self.gp.log_file = i.min(LOG_FILES.len() - 1);
                self.load_log(id, cx);
            }
        }
    }

    /// Add-ons in the Add-ons tab's current filter, owned first.
    pub fn addons_shown(&self) -> Vec<quiver_api::info::AddOn> {
        let Some(info) = self.gp.info.ready() else { return vec![] };
        let mut v: Vec<_> = info
            .addons
            .iter()
            .filter(|a| a.visible || a.owned)
            .filter(|a| match self.gp.addon_filter {
                AddonFilter::All => true,
                AddonFilter::Owned => a.owned,
                AddonFilter::NotOwned => !a.owned,
            })
            .cloned()
            .collect();
        v.sort_by_key(|a| (!a.owned, a.name.clone().unwrap_or_default()));
        v
    }

    /// (label, path, what) for the Files tab: install, prefix, saves, logs.
    pub fn file_locations(&self, id: u32) -> Vec<(&'static str, PathBuf, &'static str)> {
        let Some(game) = self.game(id).and_then(|g| g.entry.installed.clone()) else { return vec![] };
        let saves = self.gp.saves.ready().and_then(|s| s.dir.clone()).unwrap_or_else(|| {
            game.prefix.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames")
        });
        vec![
            ("Install folder", game.install_dir.clone(), "The base install; mod profiles never change it"),
            ("Compatibility folder", game.prefix.clone(), "This game's own Windows setup (its Proton prefix)"),
            ("Save folder", saves, "savegames/<user>/<product> in the compatibility folder"),
            ("Launch logs", game.prefix.join("ubi-run"), "bridge.log, game.log, bridge-proton.log"),
        ]
    }

    /// A location for display: `~` for home, and the save folder's long prefix path cut
    /// to `…/savegames/<user>/<product>`.
    pub fn short_path(&self, p: &Path) -> String {
        let s = crate::model::tilde(p);
        match s.find("/savegames/") {
            Some(i) => {
                let head = &s[..i];
                let root = head.find("/drive_c").map(|j| &head[..j]).unwrap_or(head);
                format!("{root}/…{}", &s[i..])
            }
            None => s,
        }
    }

    pub fn account_name(&self, user_id: &str) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.accounts.iter().find(|a| a.user_id == user_id))
            .map(|a| a.name.clone())
            .unwrap_or_else(|| user_id.to_owned())
    }

    /// The account a game launches with, and whether that's an override.
    pub fn launch_account(&self, id: u32) -> (String, bool) {
        let g = self.game(id);
        if let Some(a) = g.and_then(|g| g.settings.as_ref()).and_then(|s| s.account.clone()) {
            return (self.account_name(&a), true);
        }
        if let Some(a) = self.gp.info.ready().filter(|_| self.gp.id == id).and_then(|i| i.account.clone()) {
            return (self.account_name(&a), false);
        }
        let default = self.status.as_ref().and_then(|s| s.accounts.iter().find(|a| a.default).map(|a| a.name.clone()));
        (default.or_else(|| self.status.as_ref().and_then(|s| s.signed_in.clone())).unwrap_or_else(|| "—".into()), false)
    }

    /// Ubisoft's play-time total, when known, and where it came from.
    pub fn ubisoft_playtime(&self, id: u32) -> Option<(u64, &'static str)> {
        if self.gp.id != id {
            return None;
        }
        if let Some(p) = self.gp.progress.ready() {
            if let Some(s) = p.playtime_seconds {
                let stat = p.playtime_source.as_deref().is_some_and(|s| s.starts_with("stat:"));
                return Some((s, if stat { "the game's play-time stat" } else { "Ubisoft's play-session record" }));
            }
            // Older quiverd only knew `TimePlayed`; Black Flag Resynced calls it `Playtime`.
            if let Some(s) = p
                .stats
                .iter()
                .find(|s| ["TimePlayed", "Playtime", "PlayTime"].iter().any(|id| s.id.eq_ignore_ascii_case(id)))
                .and_then(|s| s.number)
                .filter(|n| *n > 0.)
            {
                return Some((s as u64, "the game's play-time stat"));
            }
        }
        self.gp.remote.ready().and_then(|r| r.seconds).filter(|s| *s > 0).map(|s| (s, "Ubisoft's play-time record"))
    }
}

// ---------------------------------------------------------------------------------------
// Shared widgets for the tabs

pub fn toggle(th: &Theme, on: bool) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .w(px(32.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .bg(if on { th.acc } else { th.ink5 })
        .when(on, |d| d.justify_end())
        .child(div().size(px(14.)).rounded_full().bg(if on { th.acc_ink } else { th.tx3 }))
}

pub fn checkbox(th: &Theme, on: bool) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .border_color(if on { th.acc } else { th.line2 })
        .bg(if on { th.acc } else { th.ink1 })
        .when(on, |d| d.child(icon(Icon::Check, 12., th.acc_ink)))
}

/// A read-only value in a field box.
pub fn field_box(th: &Theme, text: impl Into<SharedString>, muted: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(9.))
        .rounded(px(8.))
        .bg(th.ink1)
        .border_1()
        .border_color(th.line2)
        .text_size(px(12.5))
        .text_color(if muted { th.tx3 } else { th.tx1 })
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.into())
}

/// A form row: label (and help) on the left, the control on the right.
pub fn form_row(th: &Theme, label: &str, help: Option<&str>, changed: bool, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .py(px(9.))
        .border_t_1()
        .border_color(th.line1)
        .child(
            div()
                .w(px(180.))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .font_weight(MEDIUM)
                        .text_size(px(12.5))
                        .when(changed, |d| d.child(div().size(px(6.)).rounded_full().bg(th.warn)))
                        .child(label.to_owned()),
                )
                .when_some(help, |d, h| d.child(div().text_size(px(11.5)).text_color(th.tx3).line_height(px(15.)).child(h.to_owned()))),
        )
        .child(div().flex_1().min_w(px(220.)).child(control))
}

/// A titled card section ("Runtime · Wine/Proton build").
pub fn section(th: &Theme, title: &str, sub: impl Into<SharedString>, right: Option<AnyElement>) -> gpui::Div {
    crate::ui::card(th).px(px(16.)).pb(px(8.)).child(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(8.))
            .gap_y(px(4.))
            .pt(px(12.))
            .pb(px(10.))
            .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child(title.to_owned()))
            .child(div().text_color(th.tx3).text_size(px(12.)).child(sub.into()))
            .child(div().flex_1())
            .children(right),
    )
}

/// Key/value line in a card.
pub fn kv(th: &Theme, k: &str, v: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_start()
        .gap_x(px(12.))
        .gap_y(px(2.))
        .text_size(px(12.5))
        .child(div().w(px(112.)).flex_none().text_color(th.tx3).child(k.to_owned()))
        .child(div().flex_1().min_w(px(160.)).child(v))
}

/// A small secondary button.
pub fn small(id: impl Into<gpui::ElementId>, th: &Theme, i: Option<Icon>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    button(id, th, Kind::Secondary, Size::Small, i, label)
}

pub fn danger(id: impl Into<gpui::ElementId>, th: &Theme, i: Option<Icon>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let bad = th.bad;
    let soft = th.bad_soft;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(7.))
        .h(px(26.))
        .px(px(9.))
        .rounded(px(6.))
        .border_1()
        .border_color(bad.opacity(0.35))
        .text_size(px(12.))
        .font_weight(MEDIUM)
        .text_color(bad)
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |s| s.bg(soft))
        .when_some(i, |d, i| d.child(icon(i, 14., bad)))
        .child(label.into())
}

/// Empty or loading state inside a card.
pub fn note(th: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().p(px(16.)).text_size(px(12.5)).text_color(th.tx3).child(text.into())
}

/// A load's placeholder: loading, or why it failed.
pub fn load_note<T>(th: &Theme, l: &Load<T>, what: &str) -> Option<gpui::Div> {
    match l {
        Load::Ready(_) => None,
        Load::Failed(e) => Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .p(px(16.))
                .text_size(px(12.5))
                .text_color(th.warn)
                .child(icon(Icon::TriangleAlert, 14., th.warn))
                .child(format!("Couldn't load {what}: {e}")),
        ),
        _ => Some(note(th, format!("Loading {what}…"))),
    }
}

impl Quiver {
    /// `$ quiver …` that copies itself when clicked.
    pub fn cli_line(&self, id: impl Into<gpui::ElementId>, th: &Theme, cmd: String, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let c = cmd.clone();
        crate::ui::command_line(th, cmd)
            .id(id)
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .on_click(cx.listener(move |this, _, _, cx| this.copy_text(c.clone(), "", cx)))
    }

    /// The open dialog, drawn over the whole window.
    pub fn render_dialog(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.gp.dialog.as_ref()?;
        let pad = self.mode != InputMode::Pointer;
        let (ic, tone, title, body): (Icon, gpui::Hsla, String, Vec<AnyElement>) = match d {
            Dialog::SteamRestart { waiting, .. } => (
                Icon::RefreshCw,
                th.warn,
                "Restart Steam now to show the new shortcut?".into(),
                vec![
                    div()
                        .text_color(th.tx2)
                        .child("Steam is running. Steam only reads shortcuts when it starts, so Quiver can close it cleanly, write the shortcut and start it again.")
                        .into_any_element(),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .children(waiting.iter().map(|n| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(12.))
                                .child(icon(Icon::Gamepad, 12., th.tx3))
                                .child(n.clone())
                        }))
                        .into_any_element(),
                    div()
                        .text_size(px(11.5))
                        .text_color(th.tx3)
                        .child("Not now keeps it staged: it appears the next time Steam restarts. Quiver never restarts Steam on its own.")
                        .into_any_element(),
                ],
            ),
            Dialog::SteamUser { users, .. } => (
                Icon::Users,
                th.acc_hi,
                "Which Steam user gets the shortcut?".into(),
                vec![div()
                    .text_color(th.tx2)
                    .child(format!("This machine has {} Steam users. Quiver remembers the one you pick (Settings › Appearance › Steam user).", users.len()))
                    .into_any_element()],
            ),
            Dialog::RestoreSettings { revision, game } => {
                let g = self.game(*game);
                let cur = g.and_then(|g| g.history.as_ref()).and_then(|h| h.iter().max_by_key(|r| r.revision)).map(|r| r.revision).unwrap_or(0);
                (
                    Icon::RotateCcw,
                    th.acc_hi,
                    format!("Restore launch options r{revision}?"),
                    vec![div()
                        .text_color(th.tx2)
                        .child(format!(
                            "This saves r{revision}'s settings as a new revision, r{}. Nothing is lost: r{cur} stays in the history and can be restored too.",
                            cur + 1
                        ))
                        .into_any_element()],
                )
            }
            Dialog::RestoreSaves { backup, .. } => {
                let when = backup.created.map(|t| format!("{} {}", crate::model::day(t), crate::model::clock(t))).unwrap_or_else(|| backup.id.clone());
                let slots: Vec<String> = backup.slots.iter().map(|(n, _)| n.to_string()).collect();
                (
                    Icon::ArchiveRestore,
                    th.warn,
                    format!("Restore the save backup from {when}?"),
                    vec![
                        div()
                            .text_color(th.tx2)
                            .child(format!(
                                "Slots {} are copied back into the save folder. The saves there now are backed up first, so you can undo this from the same list.",
                                slots.join(", ")
                            ))
                            .into_any_element(),
                        div()
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .child("The next launch uploads them to the cloud if cloud saves are on. Quit the game before restoring.")
                            .into_any_element(),
                        mono(11.5, th.tx3).child(backup.id.clone()).into_any_element(),
                    ],
                )
            }
            Dialog::DeleteProfile { profile, .. } => (
                Icon::Trash,
                th.bad,
                format!("Delete the profile {}?", profile.name),
                vec![
                    div()
                        .text_color(th.tx2)
                        .child("The profile is removed from the Play menu. Its saves and the files the game wrote for it are kept, and the profile can be restored from its history.")
                        .into_any_element(),
                    div()
                        .text_size(px(11.5))
                        .text_color(th.tx3)
                        .child(format!("Its layers stay in the store. Exports of this profile, if any, are not touched. Id {}", profile.id))
                        .into_any_element(),
                ],
            ),
            Dialog::RestoreProfile { name, revision, .. } => (
                Icon::RotateCcw,
                th.acc_hi,
                format!("Restore {name} r{revision}?"),
                vec![div()
                    .text_color(th.tx2)
                    .child("Its layers and switches from that revision become current again, as a new revision. A deleted profile comes back.")
                    .into_any_element()],
            ),
            Dialog::RemoveLayer { layer, .. } => (
                Icon::Trash,
                th.bad,
                format!("Remove the layer {}?", layer.name),
                vec![div()
                    .text_color(th.tx2)
                    .child(format!(
                        "Deletes its {} files ({}) from Quiver's mod store. The folder it was copied from isn't touched.",
                        layer.files,
                        human_size(layer.bytes)
                    ))
                    .into_any_element()],
            ),
            Dialog::NewProfile { name, layers, saves, .. } => {
                let rows: Vec<AnyElement> = layers
                    .iter()
                    .enumerate()
                    .map(|(i, (_, n, on))| {
                        div()
                            .id(("np-layer", i))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(10.))
                            .py(px(7.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .hover(|s| s.bg(th.ink3))
                            .child(checkbox(th, *on))
                            .child(div().flex_1().child(n.clone()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Dialog::NewProfile { layers, .. }) = &mut this.gp.dialog
                                    && let Some(l) = layers.get_mut(i)
                                {
                                    l.2 = !l.2;
                                }
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect();
                (
                    Icon::Layers,
                    th.acc_hi,
                    "New mod profile".into(),
                    vec![
                        div().child(name.clone()).into_any_element(),
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(crate::ui::caption("Layers, bottom to top", th.tx3).mb(px(4.)))
                            .when(rows.is_empty(), |d| d.child(div().text_color(th.tx3).text_size(px(12.)).child("No layers yet: add one from a folder first, or create an empty profile.")))
                            .children(rows)
                            .into_any_element(),
                        super::stores::new_profile_saves(th, self.gp.mods.ready(), layers, saves.as_deref(), cx),
                    ],
                )
            }
        };
        let buttons = self.dialog_buttons();
        let focus = self.gp.dialog_btn.min(buttons.len().saturating_sub(1));
        let danger_dialog = matches!(d, Dialog::DeleteProfile { .. } | Dialog::RemoveLayer { .. });
        let btns: Vec<AnyElement> = buttons
            .into_iter()
            .enumerate()
            .map(|(i, (label, primary))| {
                let kind = if primary { Kind::Primary } else { Kind::Secondary };
                let b = if primary && danger_dialog {
                    div()
                        .id(("dlg-btn", i))
                        .flex()
                        .flex_none()
                        .items_center()
                        .h(px(32.))
                        .px(px(13.))
                        .rounded(px(8.))
                        .bg(th.bad)
                        .text_color(gpui::white())
                        .font_weight(MEDIUM)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .child(label)
                } else {
                    button(("dlg-btn", i), th, kind, Size::Medium, None, label)
                };
                b.focus_ring(pad && i == focus, th, 8.)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dialog_press(i, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let (w, h) = self.viewport;
        let panel = div()
            .id("dialog")
            .w(px(self.dialog_width(460.)))
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(20.))
            .rounded(px(14.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line2)
            .shadow(vec![soft_shadow(crate::theme::rgba(0x000000, 0.6), 30., 80.)])
            .text_size(px(12.5))
            .line_height(px(18.))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(36.))
                            .rounded(px(10.))
                            .bg(tone.opacity(0.14))
                            .child(icon(ic, 16., tone)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(div().text_size(px(14.5)).font_weight(gpui::FontWeight::BOLD).line_height(px(20.)).child(title))
                            .children(body),
                    ),
            )
            .child(div().flex().flex_wrap().justify_end().gap(px(8.)).children(btns));
        Some(
            gpui::deferred(
                gpui::anchored().position(gpui::point(px(0.), px(0.))).child(
                    div()
                        .id("dialog-scrim")
                        .w(px(w))
                        .h(px(h))
                        .bg(th.scrim)
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.dialog_press(0, cx);
                            cx.notify();
                        }))
                        .child(panel),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ops: &[&str]) -> GameSettings {
        let mut s = GameSettings::default();
        for o in ops {
            s.apply(&SettingOp::parse(o).unwrap()).unwrap();
        }
        s
    }

    #[test]
    fn settings_reply_shapes() {
        let (g, r) = settings_from_reply(serde_json::json!({ "revision": 4, "settings": { "proton": "GE" } }));
        assert_eq!((g.proton.as_deref(), r), (Some("GE"), Some(4)));
        let (g, r) = settings_from_reply(serde_json::json!({ "proton": "GE" }));
        assert_eq!((g.proton.as_deref(), r), (Some("GE"), None));
    }

    #[test]
    fn unsaved_summary_names_keys() {
        let base = s(&["wrappers=gamemoderun"]);
        let staged = s(&["wrappers=gamemoderun,mangohud", "env.MANGOHUD=1"]);
        assert_eq!(op_keys(&ops_between(&base, &staged)), ["env.MANGOHUD", "wrappers"]);
        // Carried-over edits applied to newer saved settings keep what changed elsewhere.
        let newer = s(&["wrappers=gamemoderun", "proton=GE-2"]);
        let mut again = newer.clone();
        for op in ops_between(&base, &staged) {
            again.apply(&op).unwrap();
        }
        assert_eq!(again.proton.as_deref(), Some("GE-2"));
        assert_eq!(op_keys(&ops_between(&newer, &again)), ["env.MANGOHUD", "wrappers"]);
    }

    #[test]
    fn ops_turn_base_into_staged() {
        let base = s(&["proton=GE-1", "env.A=1", "env.B=2", "wrappers=gamemoderun", "args=-x"]);
        let staged = s(&["env.A=1", "env.B=3", "env.C=4", "dll.dxgi=n,b", "wrappers=gamemoderun,mangohud", "args=-x"]);
        let ops = ops_between(&base, &staged);
        let mut again = base.clone();
        for op in &ops {
            again.apply(op).unwrap();
        }
        assert_eq!(again, staged);
        assert!(ops.contains(&SettingOp::Unset { key: "proton".into() }));
        assert!(!ops.iter().any(|o| matches!(o, SettingOp::Set { key, .. } if key == "env.A")));
        assert!(ops_between(&staged, &staged).is_empty());
        let cmd = set_command(3539, &ops, Some("try mangohud"));
        assert!(cmd.starts_with("quiver set 3539 "));
        assert!(cmd.contains(" proton= "));
        assert!(cmd.contains("dll.dxgi=n,b"));
        assert!(cmd.ends_with(" -m 'try mangohud'"));
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(set_command(1, &[SettingOp::Set { key: "args".into(), value: "-W 3440".into() }], None), "quiver set 1 'args=-W 3440'");
    }

    #[test]
    fn diff_is_key_level() {
        let a = s(&["proton=GE-1", "env.A=1"]);
        let b = s(&["proton=GE-2", "env.A=1", "env.HDR=1"]);
        assert_eq!(
            diff(&a, &b),
            vec![
                DiffLine::Added("env.HDR".into(), "1".into()),
                DiffLine::Removed("proton".into(), "GE-1".into()),
                DiffLine::Added("proton".into(), "GE-2".into()),
            ]
        );
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn logs_keep_the_tail() {
        let (l, t) = log_lines(b"a\nb\n", 100);
        assert_eq!((l.len(), t), (2, false));
        let big = "line one\nline two\nline three\n".repeat(10);
        let (l, t) = log_lines(big.as_bytes(), 40);
        assert!(t && l.len() <= 4 && l.iter().all(|x| x.starts_with("line")));
        assert_eq!(split_wrappers("gamemoderun, mangohud,,x"), ["gamemoderun", "mangohud", "x"]);
    }
}
