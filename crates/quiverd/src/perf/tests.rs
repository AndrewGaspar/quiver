//! The hub end to end, with a fake layer (a ring writer in the session dir), fake client
//! connections (a channel per connection, as the socket loop gives the hub) and the mod
//! registry's sink.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio::sync::mpsc;
use quiver_api::{GameRef, Reply};
use quiver_api::mods::ModSdkConnection;
use quiver_api::perf::{Captures, HudCell, HudScope, HudScopeRef, PerfEvent, Presenter, Stat};
use quiver_core::modsdk::{DeclaredMetric, ModMetricsSink, Sample};
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::descriptor::{CheckedValue, MetricDescriptor, MetricKind, RoleClaim, StateValue};
use quiver_perf::layout::{DRAW_ERR_NO_IMAGE, LayerStatus};
use quiver_perf::record::LayerPresent;
use quiver_perf::session::SessionDir;

use super::*;

struct Env {
    _data: tempfile::TempDir,
    _shm: tempfile::TempDir,
    hub: Arc<PerfHub>,
}

fn env() -> Env {
    let data = tempfile::tempdir().unwrap();
    let shm = tempfile::Builder::new().prefix("quiverd-hub-test-").tempdir_in("/dev/shm").unwrap();
    let hub = PerfHub::new(data.path().to_owned(), shm.path().to_owned());
    Env { _data: data, _shm: shm, hub }
}

fn conn(id: u64) -> (ConnCtx, mpsc::UnboundedReceiver<Reply>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ConnCtx { id, tx }, rx)
}

fn perf_events(rx: &mut mpsc::UnboundedReceiver<Reply>) -> Vec<PerfEvent> {
    let mut v = Vec::new();
    while let Ok(r) = rx.try_recv() {
        if let Reply::Perf(e) = r {
            v.push(e);
        }
    }
    v
}

/// A fake Sight layer: a ring in the session dir, 120 presents a second for `secs`
/// seconds ending now, and a status block.
fn fake_layer(dir: &SessionDir, secs: i64) -> quiver_perf::ring::RingWriter<quiver_perf::shm::Mapping> {
    let w = dir.create_layer_ring(std::process::id(), [5; 16]).unwrap();
    let n = secs * 120;
    let end = mono_raw_ns();
    for i in 0..n {
        let t = end - (n - i) * 8_333_333;
        w.push(&LayerPresent { swapchain: 1, seq: i as u64 + 1, entry_ns: t, return_ns: t + 100_000, ..Default::default() }.to_record());
    }
    w.set_layer_status(&fake_status());
    w.heartbeat(mono_raw_ns());
    w
}

fn fake_status() -> LayerStatus {
    let mut s = LayerStatus::ZERO;
    s.extent_w = 2560;
    s.extent_h = 1600;
    s.swapchain = 1;
    s.vendor_id = 0x10de;
    s.draw_error = DRAW_ERR_NO_IMAGE;
    LayerStatus::set_str(&mut s.engine_name, "DXVK");
    s
}

fn dev_session(hub: &PerfHub, product: u32) -> (String, SessionDir) {
    let id = quiver_core::modsdk::new_session_id();
    let b = hub.begin(&id, GameRef::ubisoft(product), None, None, None, &BTreeMap::new(), &[], true).unwrap();
    let dir = SessionDir::open(std::path::Path::new(&b.plan.env.get("QUIVER_PERF_DIR").cloned().unwrap_or_default())).ok();
    // A dev session always has a dir, even with the HUD off for the product.
    let dir = dir.unwrap_or_else(|| hub.lock().sessions[&id].dir.clone().unwrap());
    (id, dir)
}

#[test]
fn watch_sees_a_synthetic_layer_ring() {
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 3539);
    let _w = fake_layer(&dir, 3);
    let mut watched = HashMap::new();
    drain_once(&e.hub, &mut watched);
    let (c, mut rx) = conn(1);
    let sub = e.hub.subscribe(&c, None, Some(GameRef::ubisoft(3539)), vec!["fps".into(), "frametime".into(), "fps.p1low".into(), "gpu.*".into()], 500, 1000, vec![]).unwrap();
    e.hub.publish(mono_raw_ns());
    let ev = perf_events(&mut rx);
    let u = ev.iter().find_map(|e| match e {
        PerfEvent::Update(u) => Some(u),
        _ => None,
    });
    let u = u.expect("an update");
    assert_eq!((u.sub, u.session.as_str(), u.product_id), (sub, sid.as_str(), 3539));
    let fps = u.values["fps"][&Stat::Mean].value.unwrap();
    assert!((fps - 120.0).abs() < 1.0, "{fps}");
    assert_eq!(u.values["frametime"][&Stat::Mean].text, "8.3");
    assert!(u.values.contains_key("gpu.load"), "globs expand");
    // Producers: the layer is connected, with its records counted.
    let info = e.hub.sessions().into_iter().find(|s| s.session == sid).unwrap();
    let layer = info.producers.iter().find(|p| p.id == "layer").unwrap();
    assert!(layer.connected && layer.ring == "ok" && layer.records >= 359, "{layer:?}");
    // The next publish within `every_ms` sends nothing; unsubscribe stops it for good.
    e.hub.publish(mono_raw_ns());
    assert!(perf_events(&mut rx).iter().all(|e| !matches!(e, PerfEvent::Update(_))));
    e.hub.unsubscribe(1, sub).unwrap();
    assert!(e.hub.unsubscribe(1, sub).is_err());
    // Catalog and status.
    let cat = e.hub.catalog(Some(&sid), None).unwrap();
    let fps = cat.iter().find(|m| m.descriptor.id == "fps").unwrap();
    assert_eq!(fps.source.as_deref(), Some("present.frame"));
    let st = e.hub.status(Some(&sid), None).unwrap();
    let l = st.layer.unwrap();
    assert_eq!((l.extent, l.engine.as_str(), l.draw_error.as_str()), (Some([2560, 1600]), "DXVK", "no_image"));
    assert!(st.image.is_some(), "the dev session has a hud.image");
    let summary = e.hub.end(&sid).unwrap();
    // Presents from before the session's start (− 1 s) are outside its window: the last
    // second's of the three.
    assert!(summary.frames > 100 && summary.frames < 130, "{}", summary.frames);
    assert!((summary.fps.unwrap().mean - 120.0).abs() < 1.0);
    assert!(!dir.path().exists(), "the session dir is removed");
}

#[test]
fn hud_settings_reach_a_running_game() {
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 7);
    let _w = fake_layer(&dir, 2);
    drain_once(&e.hub, &mut HashMap::new());
    let (sight, mut rx) = conn(9);
    let m = e.hub.attach(&sight, &sid).unwrap();
    assert_eq!(m.anchor, quiver_api::perf::Anchor::TopLeft);
    perf_events(&mut rx);
    // The Performance tab moves it: the running game's model follows, without a relaunch.
    let set = HudScope { anchor: Some(quiver_api::perf::Anchor::BottomRight), ..Default::default() };
    e.hub.hud.apply(&HudScopeRef::game(7), &set, &[], "test", None).unwrap();
    e.hub.settings_changed(&HudScopeRef::game(7), &set);
    let models: Vec<_> = perf_events(&mut rx).into_iter().filter_map(|e| if let PerfEvent::HudModel(m) = e { Some(m) } else { None }).collect();
    assert_eq!(models.last().map(|m| m.anchor), Some(quiver_api::perf::Anchor::BottomRight), "{models:?}");
    assert!(!models.last().unwrap().on, "shown/hidden stays the session's own");
    // Another game's settings leave it alone.
    let other = HudScope { anchor: Some(quiver_api::perf::Anchor::TopRight), ..Default::default() };
    e.hub.hud.apply(&HudScopeRef::game(8), &other, &[], "test", None).unwrap();
    e.hub.settings_changed(&HudScopeRef::game(8), &other);
    assert!(perf_events(&mut rx).iter().all(|e| !matches!(e, PerfEvent::HudModel(_))));
    e.hub.disconnect(9);
    e.hub.end(&sid);
}

#[test]
fn hud_attach_toggle_next_with_a_fake_renderer() {
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 7);
    let _w = fake_layer(&dir, 2);
    drain_once(&e.hub, &mut HashMap::new());
    let (sight, mut rx) = conn(9);
    let m = e.hub.attach(&sight, &sid).unwrap();
    assert_eq!((m.rev, m.layout.as_str(), m.on, m.presenter), (1, "compact", false, Presenter::Game));
    assert_eq!(m.extent, Some([2560, 1600]));
    assert_eq!(m.image.as_deref(), Some(dir.file(&quiver_perf::session::FileKind::HudImage).as_path()));
    assert!(m.rows[0].cells.iter().any(|c| matches!(c, HudCell::Value { metric, .. } if metric == "fps")));
    perf_events(&mut rx);
    // Shift+F3: shown; remembered for the game.
    let st = e.hub.toggle(None, None).unwrap();
    assert!(st.on && st.renderer_attached);
    let models: Vec<_> = perf_events(&mut rx).into_iter().filter_map(|e| if let PerfEvent::HudModel(m) = e { Some(m) } else { None }).collect();
    assert_eq!(models.len(), 1);
    assert!(models[0].on && models[0].rev == 2);
    // Ctrl+Shift+F3: next preset.
    assert_eq!(e.hub.next(Some(&sid), None).unwrap().layout, "standard");
    assert_eq!(e.hub.hud.for_launch(&GameRef::ubisoft(7), None).layout, "standard");
    assert!(e.hub.hud.for_launch(&GameRef::ubisoft(7), None).on);
    // Nothing changed: no model.
    perf_events(&mut rx);
    e.hub.publish(mono_raw_ns());
    assert!(perf_events(&mut rx).iter().all(|e| !matches!(e, PerfEvent::HudModel(_))));
    // The renderer goes away: no more models; ending the session tells nobody.
    e.hub.disconnect(9);
    assert!(!e.hub.status(None, Some(GameRef::ubisoft(7))).unwrap().renderer_attached);
    e.hub.end(&sid);
}

/// The layer's key counters (status block) become toggles and preset steps, as the CLI's
/// would, remembered for the game; the first value read is a baseline.
#[test]
fn in_game_keys_from_the_layer_status() {
    use quiver_perf::layout::{KEYS_ACTIVE, KEYS_NO_DISPLAY, LAYER_WAYLAND_SURFACE};
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 8);
    let w = fake_layer(&dir, 1);
    let status = std::cell::RefCell::new(fake_status());
    let set = |f: &dyn Fn(&mut LayerStatus)| {
        f(&mut status.borrow_mut());
        assert!(w.set_layer_status(&status.borrow()));
    };
    // Presses from before quiverd read the ring aren't replayed.
    set(&|s| {
        s.keys = KEYS_ACTIVE;
        s.key_toggles = u16::MAX;
        s.key_nexts = 2;
    });
    let mut watched = HashMap::new();
    drain_once(&e.hub, &mut watched);
    let st = e.hub.status(Some(&sid), None).unwrap();
    assert!(!st.on && st.layout == "compact");
    let l = st.layer.unwrap();
    assert_eq!((l.keys.as_str(), l.key_toggles, l.key_nexts), ("active", 65535, 2));
    let k = e.hub.keybinds();
    assert_eq!(k.handler, "layer");
    assert!(!k.applied);
    assert_eq!(k.games.len(), 1);
    assert_eq!((k.games[0].session.as_str(), k.games[0].keys.as_str(), k.games[0].pid), (sid.as_str(), "active", std::process::id()));

    // Shift+F3 in the game (the u16 counter wraps): shown, and remembered like
    // `quiver hud toggle`.
    set(&|s| s.key_toggles = 0);
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    assert!(e.hub.hud.for_launch(&GameRef::ubisoft(8), None).on);
    // Nothing new: nothing happens.
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    // Ctrl+Shift+F3: the next preset (and only that: still shown).
    set(&|s| s.key_nexts = 3);
    drain_once(&e.hub, &mut watched);
    let st = e.hub.status(Some(&sid), None).unwrap();
    assert!(st.on);
    assert_eq!(st.layout, "standard");
    assert_eq!(e.hub.hud.for_launch(&GameRef::ubisoft(8), None).layout, "standard");
    // Shift+F3 twice between two drains: hidden and shown again.
    set(&|s| s.key_toggles = 2);
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    set(&|s| s.key_toggles = 3);
    drain_once(&e.hub, &mut watched);
    assert!(!e.hub.status(Some(&sid), None).unwrap().on);
    assert!(!e.hub.hud.for_launch(&GameRef::ubisoft(8), None).on);

    // Why a layer can't read keys shows in the status.
    set(&|s| s.keys = KEYS_NO_DISPLAY);
    drain_once(&e.hub, &mut watched);
    assert_eq!(e.hub.status(Some(&sid), None).unwrap().layer.unwrap().keys, "no_display");
    set(&|s| {
        s.keys = KEYS_ACTIVE;
        s.flags |= LAYER_WAYLAND_SURFACE;
    });
    drain_once(&e.hub, &mut watched);
    assert_eq!(e.hub.keybinds().games[0].keys, "wayland");
    // A layer with Wayland keys reading them on the game's connection.
    set(&|s| s.keys = KEYS_ACTIVE | quiver_perf::layout::KEYS_WL_ACTIVE << quiver_perf::layout::KEYS_WL_SHIFT);
    drain_once(&e.hub, &mut watched);
    assert_eq!(e.hub.keybinds().games[0].keys, "active_wayland");
    // Presses read from Wayland count the same way.
    set(&|s| s.key_toggles = 4);
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    e.hub.end(&sid);
}

/// Every layer polls the same X keymap, so a launcher with the layer sees the game's presses
/// too: only the layer on the largest swapchain counts for the session.
#[test]
fn in_game_keys_count_once_with_two_layers() {
    use quiver_perf::layout::KEYS_ACTIVE;
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 9);
    let game = fake_layer(&dir, 1);
    // A small launcher window in another (live) process: pid 1 is always there.
    let launcher = dir.create_layer_ring(1, [6; 16]).unwrap();
    launcher.heartbeat(mono_raw_ns());
    let status = |w: &quiver_perf::ring::RingWriter<quiver_perf::shm::Mapping>, area: (u32, u32), toggles: u16| {
        let mut s = fake_status();
        (s.extent_w, s.extent_h) = area;
        s.keys = KEYS_ACTIVE;
        s.key_toggles = toggles;
        assert!(w.set_layer_status(&s));
    };
    status(&game, (2560, 1600), 0);
    status(&launcher, (1280, 720), 0);
    let mut watched = HashMap::new();
    drain_once(&e.hub, &mut watched);
    assert_eq!(e.hub.lock().sessions[&sid].key_layer().map(|l| l.pid), Some(std::process::id()));
    // Both see Shift+F3: one toggle.
    status(&game, (2560, 1600), 1);
    status(&launcher, (1280, 720), 1);
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    // The launcher alone (its own window focused, say) doesn't count.
    status(&launcher, (1280, 720), 2);
    drain_once(&e.hub, &mut watched);
    assert!(e.hub.status(Some(&sid), None).unwrap().on);
    e.hub.end(&sid);
}

#[test]
fn every_launch_gets_the_layer_with_the_hud_hidden() {
    let e = env();
    let id = quiver_core::modsdk::new_session_id();
    let b = e.hub.begin(&id, GameRef::ubisoft(12), None, None, None, &BTreeMap::new(), &[], false).unwrap();
    assert_eq!(b.plan.env.get("QUIVER_SIGHT").map(String::as_str), Some("1"), "the layer by default: {:?}", b.plan.env);
    assert!(b.plan.layer_off.is_none() && b.plan.hud.as_ref().is_some_and(|h| h.enabled && !h.on));
    let info = e.hub.sessions().into_iter().find(|s| s.session == id).unwrap();
    assert!(info.hud_enabled && !info.hud_on && info.dir.is_some() && info.layer_off.is_none(), "{info:?}");
    let st = e.hub.status(Some(&id), None).unwrap();
    assert!(st.enabled && !st.on && st.layer_off.is_none());
    // quiver-sight can attach; the model says hidden, and while hidden it's rebuilt rarely.
    let (c, _rx) = conn(1);
    let m = e.hub.attach(&c, &id).unwrap();
    assert!(!m.on);
    let now = mono_raw_ns();
    e.hub.publish(now);
    let next = e.hub.lock().sessions[&id].next_model_ns;
    assert!(next - now >= 1_900_000_000, "hidden: not at the 2 Hz rate ({} ms)", (next - now) / 1_000_000);
    // The keys' path (`hud_keys`) works with no per-game step: Shift+F3 shows it.
    e.hub.hud_keys(&id, 1, 0);
    assert!(e.hub.status(Some(&id), None).unwrap().on);
    assert!(e.hub.sessions().into_iter().find(|s| s.session == id).unwrap().hud_on);
    e.hub.end(&id);
}

#[test]
fn anti_cheat_keeps_the_layer_out_unless_the_game_puts_it_in() {
    let e = env();
    let install = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(install.path().join("EasyAntiCheat")).unwrap();
    let begin = |game: GameRef, env: &BTreeMap<String, String>| {
        let id = quiver_core::modsdk::new_session_id();
        let b = e.hub.begin(&id, game, None, None, Some(install.path()), env, &[], false).unwrap();
        (id, b.plan)
    };
    let (id, plan) = begin(GameRef::ubisoft(21), &BTreeMap::new());
    assert!(plan.env.is_empty(), "{:?}", plan.env);
    let why = plan.layer_off.clone().unwrap();
    assert!(why.contains("anti-cheat: EasyAntiCheat (EasyAntiCheat/)") && why.contains("quiver hud enable 21"), "{why}");
    assert!(plan.notes.iter().any(|n| n.contains("no Sight layer")), "{:?}", plan.notes);
    let info = e.hub.sessions().into_iter().find(|s| s.session == id).unwrap();
    assert!(!info.hud_enabled && info.layer_off.as_deref() == Some(why.as_str()));
    let err = e.hub.toggle(Some(&id), None).unwrap_err().to_string();
    assert!(err.contains("anti-cheat"), "{err}");
    assert!(e.hub.status(Some(&id), None).unwrap().presenter_reason.contains("anti-cheat"));
    assert!(e.hub.lock().sessions[&id].anti_cheat);
    e.hub.end(&id);
    // On for every game doesn't count for it; on for the game does.
    e.hub.hud.apply(&HudScopeRef::global(), &HudScope { enabled: Some(true), ..Default::default() }, &[], "test", None).unwrap();
    let (id, plan) = begin(GameRef::ubisoft(21), &BTreeMap::new());
    assert!(plan.layer_off.is_some());
    e.hub.end(&id);
    e.hub.hud.apply(&HudScopeRef::game(21), &HudScope { enabled: Some(true), ..Default::default() }, &[], "test", None).unwrap();
    let (id, plan) = begin(GameRef::ubisoft(21), &BTreeMap::new());
    assert!(plan.layer_off.is_none() && plan.env.contains_key("QUIVER_SIGHT"), "{plan:?}");
    e.hub.end(&id);
    // Steam's anti-cheat runtimes say the same, without looking at the folder.
    let id = quiver_core::modsdk::new_session_id();
    let steam_env: BTreeMap<String, String> = [("PROTON_BATTLEYE_RUNTIME".to_string(), "/s/BattlEye Runtime".to_string())].into();
    let b = e.hub.begin(&id, GameRef::steam(359550), None, None, None, &steam_env, &[], false).unwrap();
    assert!(b.plan.layer_off.as_deref().is_some_and(|w| w.contains("BattlEye")), "{:?}", b.plan.layer_off);
    e.hub.end(&id);
}

#[test]
fn layer_off_for_a_game_changes_nothing_and_on_sets_up_the_launch() {
    let e = env();
    e.hub.hud.apply(&HudScopeRef::game(11), &HudScope { enabled: Some(false), ..Default::default() }, &[], "test", None).unwrap();
    let id = quiver_core::modsdk::new_session_id();
    let b = e.hub.begin(&id, GameRef::ubisoft(11), None, Some(std::path::Path::new("/p")), None, &BTreeMap::new(), &["mangohud".into()], false).unwrap();
    assert!(b.plan.env.is_empty(), "off for the game: {:?}", b.plan.env);
    assert_eq!(b.plan.layer_off.as_deref(), Some("turned off for this game (quiver hud enable 11)"));
    assert!(e.hub.lock().sessions[&id].dir.is_none());
    let (c, _rx) = conn(1);
    assert!(e.hub.attach(&c, &id).is_err());
    assert!(e.hub.toggle(None, Some(GameRef::ubisoft(11))).is_err());
    // System metrics still run, so the session still gets a (sensor-only) summary.
    e.hub.with_session(Some(&id), 11, |s| s.store.push_num("cpu.load", mono_raw_ns(), 40.0));
    assert!(e.hub.end(&id).unwrap().cpu_load.is_some());
    // Back on for the game, hidden in captures.
    e.hub.hud.apply(&HudScopeRef::game(11), &HudScope { enabled: Some(true), captures: Some(Captures::Hidden), ..Default::default() }, &[], "test", None).unwrap();
    let id = quiver_core::modsdk::new_session_id();
    let env_in: BTreeMap<String, String> = [("OBS_VKCAPTURE".to_string(), "1".to_string())].into();
    let b = e.hub.begin(&id, GameRef::ubisoft(11), None, None, None, &env_in, &["mangohud".into()], false).unwrap();
    let dir = std::path::PathBuf::from(&b.plan.env["QUIVER_PERF_DIR"]);
    assert!(dir.join("hud.image").is_file());
    assert_eq!(b.plan.env["QUIVER_SESSION"], id);
    assert_eq!(b.plan.env["QUIVER_SIGHT"], "1");
    assert_eq!(b.plan.env["MANGOHUD_CONFIG"], "read_cfg,toggle_logging=Shift_L+F7");
    assert!(b.plan.env["VK_INSTANCE_LAYERS"].ends_with("VK_LAYER_OBS_vkcapture_64:VK_LAYER_QUIVER_sight"));
    // A launch that never produced anything records no summary.
    assert!(e.hub.end(&id).is_none());
    assert!(!dir.exists());
}

fn conn_info(session: &str, product: u32) -> ModSdkConnection {
    ModSdkConnection { conn: 42, product_id: product, surface: "aco-dlss".into(), sdk: "test".into(), proto: 2, session: Some(session.into()), ..Default::default() }
}

#[test]
fn mod_metrics_through_the_registry_sink() {
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 3539);
    let _w = fake_layer(&dir, 2);
    drain_once(&e.hub, &mut HashMap::new());
    let sink = mods::PerfModSink::new(&e.hub);
    let m = conn_info(&sid, 3539);
    let mut real = MetricDescriptor::new("aco-dlss.frames.real", MetricKind::Counter);
    real.roles = vec![RoleClaim { role: catalog::ROLE_BASE_COUNT.into(), flags: vec![], on: None }];
    let mut fg = MetricDescriptor::new("aco-dlss.fg.state", MetricKind::State);
    fg.values = vec![StateValue { value: "off".into(), label: None, level: Some(quiver_api::perf::Level::Dim) }, StateValue { value: "on".into(), label: None, level: None }];
    fg.roles = vec![RoleClaim { role: "fg.state".into(), flags: vec![], on: None }];
    let mut up = MetricDescriptor::new("aco-dlss.upscaler", MetricKind::Text);
    up.roles = vec![RoleClaim { role: "upscaler".into(), flags: vec![], on: None }];
    // Not its namespace: ignored even if the registry let it through.
    let other = MetricDescriptor::new("other.thing", MetricKind::Gauge);
    let declared: Vec<DeclaredMetric> = [real, fg, up, other].into_iter().enumerate().map(|(i, d)| DeclaredMetric { index: i as u16 + 1, id: d.id.clone(), def: d }).collect();
    sink.declared(&m, &declared);
    let s = |i: usize, value| Sample { index: declared[i].index, id: declared[i].id.clone(), kind: declared[i].def.kind, value };
    sink.samples(&m, 0, &[s(0, CheckedValue::Counter(0)), s(1, CheckedValue::State(1)), s(2, CheckedValue::Text("DLSS K".into()))]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    sink.samples(&m, 0, &[s(0, CheckedValue::Counter(18))]);
    let cat = e.hub.catalog(Some(&sid), None).unwrap();
    // Generic roles are never a mod's (§3.4): the claim is listed as a conflict, the metric
    // stays under the mod's namespace, and the generic metrics don't take its values.
    assert!(cat.iter().any(|x| x.descriptor.id == "aco-dlss.frames.real" && x.roles.is_empty() && x.conflicts == [catalog::ROLE_BASE_COUNT]));
    assert!(!cat.iter().any(|x| x.descriptor.id == "other.thing"));
    let base = cat.iter().find(|x| x.descriptor.id == "fps.base").unwrap();
    assert_eq!(base.source.as_deref(), Some("fps"));
    let v = e.hub.with_session(Some(&sid), 3539, |s| s.store.value("aco-dlss.frames.real", Stat::Rate, registry::WINDOW_1S, mono_raw_ns())).flatten();
    let rate = v.and_then(|r| r.num()).unwrap();
    assert!((40.0..=80.0).contains(&rate), "{rate}");
    assert_eq!(cat.iter().find(|x| x.descriptor.id == "upscaler").unwrap().last.as_ref().unwrap().text, "—");
    assert_eq!(cat.iter().find(|x| x.descriptor.id == "aco-dlss.upscaler").unwrap().last.as_ref().unwrap().text, "DLSS K");
    // A mark with a QPC time far outside the session is dropped and the clock flagged.
    sink.mark(&m, &declared[1], 1, None, Some("begin"));
    let info = e.hub.sessions().into_iter().find(|s| s.session == sid).unwrap();
    let p = info.producers.iter().find(|p| p.id == "mod:aco-dlss").unwrap();
    assert_eq!(p.clock, "suspect");
    // Gone with its last connection.
    sink.disconnected(&m);
    let cat = e.hub.catalog(Some(&sid), None).unwrap();
    assert_eq!(cat.iter().find(|x| x.descriptor.id == "aco-dlss.fg.state").unwrap().state, quiver_api::perf::MetricState::Gone);
    e.hub.end(&sid);
}

/// The layer's Reflex block, end to end: a status block with sleep mode, counters and
/// latency sums → the `nvidia` producer → fps.base, fg.ratio, latency.pc and the Full row.
#[test]
fn reflex_from_the_layer_status_block() {
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 3539);
    let w = fake_layer(&dir, 2);
    let mut watched = HashMap::new();
    let mut st = fake_status();
    st.reflex_flags = quiver_perf::layout::REFLEX_MODE_SET
        | quiver_perf::layout::REFLEX_LOW_LATENCY
        | quiver_perf::layout::REFLEX_SLEEPING
        | quiver_perf::layout::REFLEX_COUNTING_SLEEP;
    st.reflex_min_interval_us = 6250;
    // Two status writes 600 ms apart: 36 frames, 72 presents, 36 latency samples of 18 ms.
    for (frames, lat_frames) in [(100u64, 100u64), (136, 136)] {
        st.app_frames = frames;
        st.app_presents = frames * 2;
        st.pc_latency_frames = lat_frames;
        st.pc_latency_sum_us = lat_frames * 18_000;
        w.set_layer_status(&st);
        drain_once(&e.hub, &mut watched);
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    let now = mono_raw_ns();
    let v = |id: &str| e.hub.with_session(Some(&sid), 3539, |s| s.store.formatted(id, Stat::Mean, registry::WINDOW_1S, now)).flatten().map(|v| v.text);
    assert_eq!(v("fg.ratio").as_deref(), Some("2.0"));
    assert_eq!(v("latency.pc").as_deref(), Some("18.0"));
    assert_eq!(v("nv.reflex.cap").as_deref(), Some("160"));
    let cat = e.hub.catalog(Some(&sid), None).unwrap();
    assert_eq!(cat.iter().find(|x| x.descriptor.id == "fps.base").unwrap().source.as_deref(), Some("nv.frames.app"));
    assert!(cat.iter().any(|x| x.descriptor.id == "nv.reflex.mode" && x.last.as_ref().is_some_and(|l| l.text == "on")));
    e.hub.end(&sid);
}

#[test]
fn static_catalog_and_session_picking() {
    let e = env();
    let cat = e.hub.catalog(None, Some(GameRef::ubisoft(1))).unwrap();
    assert!(cat.iter().any(|m| m.descriptor.id == "fps.p1low"));
    assert!(cat.iter().any(|m| m.descriptor.id == "gpu.load"));
    assert!(e.hub.status(None, None).is_err());
    let (a, _) = dev_session(&e.hub, 1);
    assert_eq!(e.hub.status(None, None).unwrap().session, a, "the only session");
    let (b, _) = dev_session(&e.hub, 2);
    assert!(e.hub.status(None, None).is_err(), "two sessions, none focused");
    assert_eq!(e.hub.status(None, Some(GameRef::ubisoft(2))).unwrap().session, b);
    let (c, _rx) = conn(1);
    for _ in 0..8 {
        e.hub.subscribe(&c, Some(&a), None, vec![], 100, 1000, vec![]).unwrap();
    }
    assert!(e.hub.subscribe(&c, Some(&a), None, vec![], 100, 1000, vec![]).is_err(), "≤ 8 per connection");
    e.hub.end(&a);
    e.hub.end(&b);
}

/// Shift+F2 read by the layer becomes a panel toggle broadcast to `quiver-sight` for the
/// session; the CLI's commands go out the same way; reports show in the status.
#[test]
fn panel_key_and_commands() {
    use quiver_api::sight::{Backdrop, PanelAction, PanelReport, PanelSection};
    use quiver_perf::layout::KEYS_ACTIVE;
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 10);
    let w = fake_layer(&dir, 1);
    let mut events = e.hub.broadcast.subscribe();
    let mut s = fake_status();
    s.keys = KEYS_ACTIVE;
    s.key_panels = 7;
    assert!(w.set_layer_status(&s));
    let mut watched = HashMap::new();
    drain_once(&e.hub, &mut watched);
    let panel = |ev: &mut tokio::sync::broadcast::Receiver<PerfEvent>| {
        std::iter::from_fn(|| ev.try_recv().ok()).find_map(|e| match e {
            PerfEvent::Panel(c) => Some(c),
            _ => None,
        })
    };
    assert!(panel(&mut events).is_none(), "the first value read is a baseline");
    s.key_panels = 8;
    assert!(w.set_layer_status(&s));
    drain_once(&e.hub, &mut watched);
    let c = panel(&mut events).expect("Shift+F2 toggles the panel");
    assert_eq!((c.action, c.session.as_deref(), c.by.as_str()), (PanelAction::Toggle, Some(sid.as_str()), "keys"));
    // It didn't touch the HUD.
    assert!(!e.hub.status(Some(&sid), None).unwrap().on);
    // The CLI: open on a section, the only session; then close (no session needed).
    let o = e.hub.panel_command(PanelAction::Open, None, Some(PanelSection::Mods), "cli").unwrap();
    assert!(o.seq > c.seq);
    assert_eq!((o.session.as_deref(), o.section), (Some(sid.as_str()), Some(PanelSection::Mods)));
    assert!(e.hub.panel_command(PanelAction::Toggle, Some("s-nope"), None, "cli").is_err());
    let cl = e.hub.panel_command(PanelAction::Close, None, None, "cli").unwrap();
    assert_eq!(cl.session, None);
    let st = e.hub.panel_status();
    assert_eq!(st.last_command.map(|c| c.seq), Some(cl.seq));
    assert_eq!(st.keys.iter().find(|k| k.session == sid).map(|k| (k.keys.as_str(), k.presses)), Some(("active", 1)));
    e.hub.panel_report(PanelReport { open: true, session: Some(sid.clone()), backdrop: Backdrop::Dim, backdrop_detail: "blur is off".into(), ..Default::default() });
    assert_eq!(e.hub.panel_status().panel.map(|p| (p.open, p.backdrop)), Some((true, Backdrop::Dim)));
    e.hub.end(&sid);
    assert_eq!(e.hub.panel_status().panel.map(|p| p.open), Some(false), "the session ended");
    assert!(e.hub.panel_command(PanelAction::Toggle, None, None, "cli").is_err(), "no game running");
}

#[test]
fn lab_metrics_never_land_in_the_persons_session() {
    let e = env();
    let id = quiver_core::modsdk::new_session_id();
    e.hub.begin(&id, GameRef::ubisoft(3539), None, None, None, &BTreeMap::new(), &[], false).unwrap();
    // The person's session for the game takes metrics with no session or an unknown one…
    assert!(e.hub.with_session(None, 3539, |_| ()).is_some());
    assert!(e.hub.with_session(Some("s-unknown"), 3539, |_| ()).is_some());
    // …never a lab instance's.
    assert!(e.hub.with_session(Some("lab-t1-r1"), 3539, |_| ()).is_none());
}

#[test]
fn stereo_from_the_setting_and_a_live_mod() {
    use quiver_perf::stereo::{Setting, Stereo};
    let e = env();
    let (sid, dir) = dev_session(&e.hub, 7);
    // The launch's sight.stereo = hsbs, as the layer reports it from its environment.
    let w = fake_layer(&dir, 2);
    let mut st = fake_status();
    (st.extent_w, st.extent_h, st.stereo_setting) = (1920, 1080, Setting::Fixed(Stereo::Hsbs).code());
    w.set_layer_status(&st);
    drain_once(&e.hub, &mut HashMap::new());
    let (sight, mut rx) = conn(9);
    let m = e.hub.attach(&sight, &sid).unwrap();
    let s = m.stereo.clone().unwrap();
    assert_eq!((s.layout.as_str(), s.source.as_str(), s.detail.as_str()), ("hsbs", "setting", "QUIVER_SIGHT_STEREO=hsbs"));
    assert!((m.text_px - 1080.0 * 0.016).abs() < 1e-3, "hsbs: one eye is laid out 1080 tall");
    perf_events(&mut rx);
    // Torchbearer says sbs through the SDK: it wins while its channel is up, at once.
    let sink = mods::PerfModSink::new(&e.hub);
    let mut c = ModSdkConnection { conn: 7, product_id: 7, surface: "torchbearer".into(), sdk: "test".into(), proto: 3, session: Some(sid.clone()), ..Default::default() };
    c.stereo = Some("sbs".into());
    sink.stereo(&c);
    e.hub.publish(mono_raw_ns());
    let models: Vec<_> = perf_events(&mut rx).into_iter().filter_map(|e| if let PerfEvent::HudModel(m) = e { Some(m) } else { None }).collect();
    let s = models.last().and_then(|m| m.stereo.clone()).unwrap();
    assert_eq!((s.layout.as_str(), s.source.as_str(), s.detail.as_str(), s.setting.as_str()), ("sbs", "mod", "torchbearer (SDK)", "hsbs"));
    assert_eq!(e.hub.status(Some(&sid), None).unwrap().stereo.unwrap().layout, "sbs");
    // Another connection's withdrawal doesn't touch it; its own disconnect does.
    sink.stereo(&ModSdkConnection { conn: 8, stereo: None, ..c.clone() });
    assert_eq!(e.hub.status(Some(&sid), None).unwrap().stereo.unwrap().source, "mod");
    sink.disconnected(&c);
    let s = e.hub.status(Some(&sid), None).unwrap().stereo.unwrap();
    assert_eq!((s.layout.as_str(), s.source.as_str()), ("hsbs", "setting"));
    assert_eq!(s.line(), "hsbs (setting: QUIVER_SIGHT_STEREO=hsbs)");
    e.hub.disconnect(9);
    e.hub.end(&sid);
}

/// An Epic game's mod (Torchbearer in Alan Wake Remastered): its SDK channel names the game,
/// not a Ubisoft product, and its stereo word reaches the session by its id: the HUD model and
/// the in-game panel's keys both carry it, with no `sight.stereo` setting in the launch.
#[test]
fn an_epic_mods_stereo_reaches_the_hud_and_the_panel() {
    let e = env();
    let awr = GameRef::parse("epic:7315f614b0504081bcdceef138075c9c").unwrap();
    let sid = quiver_core::modsdk::new_session_id();
    let b = e.hub.begin(&sid, awr, None, None, None, &BTreeMap::new(), &[], true).unwrap();
    let dir = SessionDir::open(std::path::Path::new(&b.plan.env.get("QUIVER_PERF_DIR").cloned().unwrap_or_default()))
        .ok()
        .unwrap_or_else(|| e.hub.lock().sessions[&sid].dir.clone().unwrap());
    // The layer: a 1920x1080 swapchain, no stereo setting (auto).
    let w = fake_layer(&dir, 2);
    let mut st = fake_status();
    (st.extent_w, st.extent_h) = (1920, 1080);
    w.set_layer_status(&st);
    drain_once(&e.hub, &mut HashMap::new());
    let (sight, mut rx) = conn(9);
    let m = e.hub.attach(&sight, &sid).unwrap();
    assert_eq!(m.stereo.as_ref().unwrap().layout, "mono");
    perf_events(&mut rx);
    let sink = mods::PerfModSink::new(&e.hub);
    let c = ModSdkConnection {
        conn: 3,
        product_id: 0,
        game: Some(awr),
        surface: "torchbearer".into(),
        sdk: "quiver_mod.h 3.0".into(),
        proto: 3,
        session: Some(sid.clone()),
        stereo: Some("hsbs".into()),
        ..Default::default()
    };
    sink.stereo(&c);
    e.hub.publish(mono_raw_ns());
    let models: Vec<_> = perf_events(&mut rx).into_iter().filter_map(|e| if let PerfEvent::HudModel(m) = e { Some(m) } else { None }).collect();
    let s = models.last().and_then(|m| m.stereo.clone()).expect("a HUD model with the mod's layout");
    assert_eq!((s.layout.as_str(), s.source.as_str(), s.detail.as_str(), s.setting.as_str()), ("hsbs", "mod", "torchbearer (SDK)", "auto"));
    let keys = e.hub.panel_status().keys;
    let k = keys.iter().find(|k| k.session == sid).expect("the session's panel keys");
    assert_eq!(k.stereo.as_ref().map(|s| s.layout.as_str()), Some("hsbs"), "the in-game panel is laid out per eye too");
    assert_eq!(e.hub.status(Some(&sid), None).unwrap().stereo.unwrap().line(), "hsbs (mod: torchbearer (SDK); setting auto)");
    sink.disconnected(&c);
    assert_eq!(e.hub.status(Some(&sid), None).unwrap().stereo.unwrap().layout, "mono");
    e.hub.disconnect(9);
    e.hub.end(&sid);
}
