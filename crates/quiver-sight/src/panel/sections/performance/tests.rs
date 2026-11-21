use super::*;
use crate::demo::{sample_panel, sample_perf, standard_layout};
use crate::panel::state::{HudState, SessionView};
use quiver_api::perf::Presenter;

fn data() -> PanelData {
    sample_panel()
}

fn section() -> PerfSection {
    PerfSection { data: sample_perf(), ..Default::default() }
}

fn sent(o: &Outcome) -> Vec<Req> {
    match o {
        Outcome::Done(acts) => acts.iter().filter_map(|a| if let Act::Send(r) = a { Some(r.clone()) } else { None }).collect(),
        Outcome::Pass => vec![],
    }
}

fn editing() -> (PerfSection, PanelData) {
    let d = data();
    let mut p = section();
    p.opening = Some("standard".into());
    let (note, _) = p.reply(Reply::Layout(Ok(standard_layout())), &d);
    assert!(note.is_none());
    (p, d)
}

#[test]
fn metrics_are_grouped_by_who_produces_them() {
    let g = groups(sample_perf().catalog.ready().unwrap());
    let titles: Vec<&str> = g.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(titles, ["Sight layer", "pharos mod", "System · quiverd"]);
    let layer: Vec<&str> = g[0].entries.iter().map(|e| e.id.as_str()).collect();
    // The graph's series and the canonical numbers; never the producer metric under one.
    assert_eq!(layer[0], GRAPH_PRESENTED);
    assert!(layer.contains(&"fps") && layer.contains(&"fps.base") && layer.contains(&GRAPH_RENDERED));
    assert!(!layer.contains(&"nv.latency.pc") && !layer.contains(&"latency.split.sim"));
    // The split goes with its producer (the mod, here), as a stacked bar.
    let m = &g[1];
    assert_eq!(m.note, "SDK · connected");
    assert!(m.entries.iter().any(|e| e.id == SPLIT_LATENCY && e.kind == "stacked bar"));
    assert!(m.entries.iter().any(|e| e.id == "pharos.fg" && e.kind == "state"));
    // Kinds and short labels for new cells.
    let gpu = g[2].entries.iter().find(|e| e.id == "gpu.load").unwrap();
    assert_eq!((gpu.kind, gpu.short.as_deref()), ("number", Some("GPU")));
    // Metrics without a value in this session aren't listed.
    assert!(!g.iter().flat_map(|g| &g.entries).any(|e| e.id == "bat.level"));
    assert_eq!(entries(&g).len(), g.iter().map(|g| g.entries.len()).sum::<usize>());
}

#[test]
fn toggling_a_metric_adds_or_removes_its_cells() {
    let mut l = standard_layout();
    let rows = l.rows.len();
    let e = |id: &str, short: Option<&str>| Entry { id: id.into(), label: id.into(), kind: "number", short: short.map(String::from) };
    assert!(shows(&l, "fps") && shows(&l, GRAPH_RENDERED) && !shows(&l, "gpu.vram.used"));
    // Added on a row of its own at the end, with its short label.
    assert_eq!(toggle(&mut l, &e("gpu.vram.used", Some("VRAM"))), (vec![], true));
    assert_eq!(l.rows.len(), rows + 1);
    assert_eq!(l.rows.last().unwrap()[0], json!({ "metric": "gpu.vram.used", "short": "VRAM" }));
    // Removed from wherever it is; a row left with nothing to show goes.
    let (gone, added) = toggle(&mut l, &e("latency.pc", None));
    assert!(!added && gone.is_empty(), "the latency row keeps Reflex's tag: {gone:?}");
    assert!(!shows(&l, "latency.pc"));
    let (gone, _) = toggle(&mut l, &e("nv.reflex.mode", None));
    assert_eq!(gone, [3], "the latency row had nothing left");
    // The rendered series comes out of the graph and goes back into it.
    toggle(&mut l, &Entry { id: GRAPH_RENDERED.into(), label: String::new(), kind: "graph", short: None });
    assert!(shows(&l, GRAPH_PRESENTED) && !shows(&l, GRAPH_RENDERED));
    toggle(&mut l, &Entry { id: GRAPH_RENDERED.into(), label: String::new(), kind: "graph", short: None });
    assert!(shows(&l, GRAPH_RENDERED));
    // The split.
    let split = Entry { id: SPLIT_LATENCY.into(), label: String::new(), kind: "stacked bar", short: None };
    toggle(&mut l, &split);
    assert!(!shows(&l, SPLIT_LATENCY));
    toggle(&mut l, &split);
    assert_eq!(l.rows.last().unwrap()[0]["parts"].as_array().unwrap().len(), 4);
}

#[test]
fn rows_move_and_a_preset_saves_as_a_user_layout() {
    let mut l = standard_layout();
    let first = l.rows[0].clone();
    assert_eq!(move_row(&mut l, 0, -1), 0, "the top stays");
    assert_eq!(move_row(&mut l, 0, 1), 1);
    assert_eq!(l.rows[1], first);
    let last = l.rows.len() - 1;
    assert_eq!(move_row(&mut l, last, 1), last, "the bottom stays");
    assert_eq!(save_as("standard", "Standard", true), ("standard-edited".into(), "Standard (edited)".into()));
    assert_eq!(save_as("mine", "Mine", false), ("mine".into(), "Mine".into()));
}

#[test]
fn the_editor_is_driven_with_the_pad() {
    let (mut p, d) = editing();
    let e = p.editor.as_ref().unwrap();
    assert_eq!((e.base.as_str(), e.zone, e.title()), ("standard", Zone::Metrics, "Standard".into()));
    // The DLSS profile runs: changes go to its scope by default.
    assert_eq!(e.applies, Applies::Profile);
    // A on the first metric (the presented graph, shown): it goes, and a preview is asked.
    let o = p.input(Input::Activate, &d, &json!(0));
    assert!(matches!(sent(&o)[..], [Req::Preview(_)]));
    let e = p.editor.as_ref().unwrap();
    assert!(e.edited && !shows(&e.layout, GRAPH_PRESENTED));
    assert_eq!(e.title(), "Standard (edited)");
    // ▶ to the rows; A picks the first up, ▼ moves it, A puts it down.
    p.input(Input::Right, &d, &json!(0));
    assert_eq!(p.editor.as_ref().unwrap().zone, Zone::Preview);
    let before = p.editor.as_ref().unwrap().layout.rows.clone();
    p.input(Input::Activate, &d, &json!(0));
    assert!(p.editor.as_ref().unwrap().moving.is_some());
    assert_eq!(p.hints(&d)[0].1, "Move the row");
    p.input(Input::Down, &d, &json!(0));
    p.input(Input::Down, &d, &json!(0));
    let e = p.editor.as_ref().unwrap();
    assert_eq!(e.row, 2);
    assert_eq!(e.layout.rows[2], before[0]);
    // B puts it back where it was.
    p.input(Input::Back, &d, &json!(0));
    let e = p.editor.as_ref().unwrap();
    assert!(e.moving.is_none() && e.layout.rows == before && e.zone == Zone::Preview);
    // Picked up and put down: it stays moved.
    p.input(Input::Activate, &d, &json!(0));
    p.input(Input::Up, &d, &json!(0));
    let o = p.input(Input::Activate, &d, &json!(0));
    assert!(matches!(sent(&o)[..], [Req::Preview(_)]));
    assert_eq!(p.editor.as_ref().unwrap().layout.rows[1], before[2]);
    // ▶ to the settings: ◀▶ change the focused one at the editor's scope.
    p.input(Input::Right, &d, &json!(0));
    assert_eq!(p.editor.as_ref().unwrap().zone, Zone::Settings);
    p.input(Input::Down, &d, &json!(0));
    let o = p.input(Input::Right, &d, &json!(0));
    let scope = HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(3539)), profile: Some("p-dlss".into()) };
    assert_eq!(sent(&o), [Req::Set { scope: scope.clone(), set: HudScope { anchor: Some(Anchor::Top), ..Default::default() } }]);
    // Applies to: Profile → Game asks for the game's settings.
    p.editor.as_mut().unwrap().setting = SETTINGS.iter().position(|s| *s == Setting::AppliesTo).unwrap();
    let o = p.input(Input::Right, &d, &json!(0));
    assert_eq!(sent(&o), [Req::Settings(HudScopeRef::of(quiver_api::GameRef::ubisoft(3539)))]);
    assert_eq!(p.editor.as_ref().unwrap().applies, Applies::Game);
    // Y saves the edited preset as a user layout and selects it at the editor's scope.
    let o = p.input(Input::Menu, &d, &json!(0));
    match &sent(&o)[..] {
        [Req::Save { layout, scope }] => {
            assert_eq!((layout.id.as_str(), layout.name.as_str(), layout.builtin), ("standard-edited", "Standard (edited)", false));
            assert_eq!(scope, &HudScopeRef::of(quiver_api::GameRef::ubisoft(3539)));
        }
        other => panic!("{other:?}"),
    }
    let info = HudLayoutInfo { id: "standard-edited".into(), name: "Standard (edited)".into(), builtin: false, path: None, error: None };
    let view = d.hud.ready().unwrap().settings.clone();
    let (note, reload) = p.reply(Reply::Saved(Ok((info, view))), &d);
    assert!(reload && note.unwrap().0.contains("Standard (edited)"));
    let e = p.editor.as_ref().unwrap();
    assert!(!e.edited && !e.builtin && e.base == "standard-edited");
    assert_eq!(save_as(&e.base, &e.base_name, e.builtin).0, "standard-edited", "saved again over itself");
    // B: settings → rows → out of the editor.
    p.input(Input::Back, &d, &json!(0));
    assert_eq!(p.editor.as_ref().unwrap().zone, Zone::Preview);
    p.input(Input::Back, &d, &json!(0));
    assert!(p.editor.is_none());
    // LB/RB are the panel's.
    let (mut p, d) = editing();
    assert_eq!(p.input(Input::NextSection, &d, &json!(0)), Outcome::Pass);
}

#[test]
fn the_preview_follows_moved_rows() {
    let (mut p, _) = editing();
    let e = p.editor.as_mut().unwrap();
    let preview = HudPreview { model: crate::demo::sample_model("standard", 0, Presenter::Desktop, None), rows: (0..6).collect() };
    e.previewed(e.layout.clone(), preview);
    assert_eq!(e.model_rows(2), Some(2..3));
    // Rows moved locally map to where the last preview had them until the next one.
    move_row(&mut e.layout, 0, 1);
    e.perm.swap(0, 1);
    assert_eq!((e.model_rows(0), e.model_rows(1)), (Some(1..2), Some(0..1)));
    // A preview of another draft keeps the mapping; one of this draft resets it.
    let other = standard_layout();
    let p2 = e.preview.clone().unwrap().1;
    e.previewed(other, p2.clone());
    assert_eq!(e.model_rows(0), Some(1..2));
    e.previewed(e.layout.clone(), p2);
    assert_eq!(e.model_rows(0), Some(0..1));
}

#[test]
fn the_section_controls() {
    let d = data();
    let mut p = section();
    assert_eq!(p.ctls(&d), [Ctl::Cap, Ctl::Hud, Ctl::Edit, Ctl::Timings]);
    // The cap: ◀▶ step through off, 30, 60, 120, refresh − 3 (and a value of its own).
    let cap = json!(160.0);
    assert_eq!(cap_choices(&cap), [json!(0), json!(30), json!(60), json!(120), json!(160.0), json!("vrr")]);
    assert_eq!(p.input(Input::Right, &d, &cap), Outcome::Done(vec![Act::SetTweak(CAP_TWEAK.into(), json!("vrr"))]));
    assert_eq!(p.input(Input::Left, &d, &json!(0)), Outcome::Done(vec![]), "nothing left of off");
    assert_eq!(cap_label(&json!("vrr")), "VRR −3");
    assert_eq!(cap_label(&json!(0)), "Off");
    // The HUD: hidden or a preset, at the profile's scope; hidden toggles a shown HUD off.
    p.input(Input::Down, &d, &cap);
    assert_eq!(p.focused(&d), Some(Ctl::Hud));
    assert_eq!(p.hud_value(&d), "standard");
    let scope = HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(3539)), profile: Some("p-dlss".into()) };
    assert_eq!(sent(&p.input(Input::Right, &d, &cap)), [Req::Set { scope, set: HudScope { layout: Some("full".into()), on: Some(true), ..Default::default() } }]);
    assert_eq!(p.hud_value(&d), "full", "shown at once");
    p.hud_draft = None;
    p.focus = 1;
    let mut hidden = d.clone();
    if let Load::Ready(h) = &mut hidden.hud {
        h.status.as_mut().unwrap().layout = "fps".into();
    }
    assert_eq!(sent(&p.input(Input::Left, &hidden, &cap)), [Req::Toggle]);
    // A on Edit asks for the layout, the catalog and the scope's settings; the editor opens
    // when the layout comes.
    p.hud_draft = None;
    p.focus = 2;
    let o = p.input(Input::Activate, &d, &cap);
    assert!(matches!(&sent(&o)[..], [Req::Layout(id), Req::Catalog, Req::Settings(_)] if id == "standard"));
    assert!(p.editor.is_none());
    p.reply(Reply::Layout(Ok(standard_layout())), &d);
    assert!(p.editor.is_some());
    // X saves a log; B and ◀ on Edit are the panel's.
    let mut p = section();
    assert_eq!(sent(&p.input(Input::PlayOptions, &d, &cap)), [Req::Log]);
    assert_eq!(p.input(Input::Back, &d, &cap), Outcome::Pass);
    p.focus = 2;
    assert_eq!(p.input(Input::Left, &d, &cap), Outcome::Pass);
    assert_eq!(p.input(Input::Up, &d, &cap), Outcome::Done(vec![]));
    p.focus = 0;
    assert_eq!(p.input(Input::Up, &d, &cap), Outcome::Pass, "the top");
    // Reflex timings at the scope.
    p.focus = 3;
    assert!(matches!(&sent(&p.input(Input::Activate, &d, &cap))[..], [Req::Set { set, .. }] if set.reflex_timings == Some(true)));
}

#[test]
fn the_cap_says_who_enforces_it() {
    let d = data();
    let st = d.tweaks.ready().unwrap().options.get(CAP_TWEAK).unwrap();
    let reflex = PerfValue { value: Some(120.0), text: "120".into(), unit: "fps".into(), ..Default::default() };
    let s = cap_enforcer(Some(st), &json!(120), Some(&reflex));
    assert!(s.starts_with("Set by DLSS (reflex.fps_limit = 120)"), "{s}");
    assert!(s.contains("Reflex cap: 120 fps"), "{s}");
    assert!(cap_enforcer(Some(st), &json!(0), None).starts_with("No cap from Quiver"));
    let mut dxvk = st.clone();
    dxvk.tweak.as_mut().unwrap().planned = Some(Planned::Apply { mech: "dxvk".into(), writes: vec!["DXVK_CONFIG: dxgi.maxFrameRate = 60".into()], apply: Default::default() });
    assert_eq!(cap_enforcer(Some(&dxvk), &json!(60), None), "Set by DXVK's frame limiter, from the next launch.");
    assert_eq!(reflex_words(None), "Not used by this game");
    assert_eq!(reflex_words(Some(&PerfValue { text: "boost".into(), ..Default::default() })), "On + Boost");
}

#[test]
fn scopes_follow_the_running_profile() {
    let mut s = SessionView { game: quiver_api::GameRef::steam(870780), profile_id: Some("p-1".into()), ..Default::default() };
    assert_eq!(Applies::default_for(&s), Applies::Profile);
    assert_eq!(Applies::Profile.scope(&s), HudScopeRef { game: Some(s.game), profile: Some("p-1".into()) });
    assert_eq!(Applies::All.scope(&s), HudScopeRef::global());
    assert_eq!(Applies::choices(&s).len(), 3);
    s.profile_id = Some("vanilla".into());
    assert_eq!(Applies::default_for(&s), Applies::Game);
    assert_eq!(Applies::Profile.scope(&s), HudScopeRef::of(s.game), "no profile: the game's");
    assert_eq!(Applies::choices(&s), [Applies::Game, Applies::All]);
    // The editor's settings come from its scope once loaded, else the game's.
    let d = data();
    let mut p = section();
    assert_eq!(p.config(&d).layout, "standard");
    let mut v: HudSettingsView = d.hud.ready().unwrap().settings.clone();
    v.scope = HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(3539)), profile: Some("p-dlss".into()) };
    v.effective.scale = 0.75;
    p.data.scoped = Some(v);
    assert_eq!(p.config(&d).scale, 0.75);
    let _ = HudState::live;
}

#[test]
fn replies_say_what_came_of_it() {
    let d = data();
    let mut p = section();
    let saved = quiver_api::perf::PerfLogSaved { path: "/tmp/x/perf-logs/a.csv".into(), frames: 3600, seconds: 30.0 };
    let (note, reload) = p.reply(Reply::Log(Ok(saved)), &d);
    assert!(!reload && note.unwrap().0.starts_with("Saved 30 s (3600 frames): /tmp/x/perf-logs/a.csv"));
    let (note, _) = p.reply(Reply::Log(Err("no frames".into())), &d);
    assert_eq!(note.unwrap().1, Tone::Bad);
    let (note, reload) = p.reply(Reply::Set(Err("nope".into())), &d);
    assert!(reload && note.unwrap().0.contains("nope"));
    assert_eq!(tilde(std::path::Path::new("/nowhere/x")), "/nowhere/x");
}
