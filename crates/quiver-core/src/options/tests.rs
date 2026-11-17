use super::*;
use crate::mods::Package;
use crate::modversions::NewVersion;
use serde_json::json;
use quiver_api::Game;

/// The shipped aco_dlss.ini (a trimmed copy of the mod's, with its comments).
const ACO_INI: &str = "; aco-dlss configuration. Re-read about once a second while the game runs (except render_scale).\r\n\
[upscaler]\r\n\
enabled=1\r\n\
; DLSS render preset: M (DLSS 4.5, performance), L (DLSS 4.5, ultra performance), K (DLSS 4 transformer)\r\n\
preset=M\r\n\
render_scale=0\r\n\
toggle_key=F10\r\n\
\r\n\
[fg]\r\n\
mode=d3d12\r\n\
enabled=1\r\n\
toggle_key=F9\r\n\
present_thread=1\r\n\
skip=\r\n\
\r\n\
[world]\r\n\
; Ambient population multiplier\r\n\
npc_density=1\r\n\
npc_distance=1\r\n\
unknown_future_key=7\r\n";

fn folder(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    for (p, c) in files {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    root.to_owned()
}

fn game(t: &Path, product_id: u32) -> Game {
    Game { product_id, name: "Game".into(), install_dir: t.join("install"), prefix: t.join("pfx"), exe: "game.exe".into(), args: None, language: None }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    t: PathBuf,
    g: Game,
    store: ModStore,
    mod_id: String,
    profile: String,
}

/// AC Origins with the DLSS mod (a plain folder, matched by Quiver's catalog) in a profile.
fn aco() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().to_owned();
    let g = game(&t, 3539);
    folder(&g.install_dir, &[("ACOrigins.exe", "e")]);
    std::fs::create_dir_all(&g.prefix).unwrap();
    let store = ModStore::new(&t.join("data"), 3539).with_game(&g, None);
    let src = folder(&t.join("dlss"), &[("aco_dlss.ini", ACO_INI), ("d3d11.dll", "dll")]);
    let l = store.install(&Package::Dir { path: src, name: Some("DLSS".into()) }, "cli").unwrap().layer;
    let p = store.create_profile("DLSS", std::slice::from_ref(&l.id), "cli", None).unwrap();
    Fixture { _tmp: tmp, t, g, store, mod_id: l.id, profile: p.profile.id }
}

fn set(f: &Fixture, ops: Vec<OptionOp>) -> (Applied, OptionsRevision) {
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let cur = current(&f.store, &f.profile).unwrap();
    let a = apply_ops(&ctx, &cur.values, &cur.raw, &ops).unwrap();
    let r = append(&f.store, &f.profile, a.values.clone(), a.raw.clone(), "cli", None, None, a.changes.clone()).unwrap();
    (a, r)
}

fn op(id: &str, v: Value) -> OptionOp {
    OptionOp::Set { surface: "DLSS".into(), id: id.into(), value: v }
}

#[test]
fn catalog_schemas_parse_and_lint_clean() {
    assert!(OPTIONS_CATALOG.len() >= 2);
    for (id, text) in OPTIONS_CATALOG {
        let s = schema::parse(text, false).unwrap_or_else(|e| panic!("options/{id}.toml: {e:#}"));
        let errors: Vec<String> = schema::lint(&s).into_iter().filter(|f| f.severity == quiver_api::options::Severity::Error).map(|f| f.to_string()).collect();
        assert!(errors.is_empty(), "options/{id}.toml: {errors:#?}");
        assert!(s.matches.is_some() && s.audited.is_some(), "options/{id}.toml needs [match] and audited");
    }
}

#[test]
fn surfaces_resolve_from_the_catalog_with_file_defaults() {
    let f = aco();
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    assert_eq!(ctx.surfaces.len(), 1, "{:?}", ctx.problems);
    let s = &ctx.surfaces[0];
    assert_eq!(s.key, format!("{}/aco-dlss", f.mod_id));
    assert_eq!(s.resolved.source, SchemaSource::Catalog);
    let st = states(&ctx, &OptionsRevision::default(), &RunInfo::default(), None);
    let o = &st[0].options;
    assert_eq!(o["upscaler.preset"].value, json!("M"));
    assert_eq!(o["upscaler.enabled"].value, json!(true));
    assert_eq!(o["world.npc_density"].value, json!(1.0));
    assert_eq!(o["world.npc_density"].from, ValueSource::Default);
    // `when`: fg options show only with mode=d3d12.
    assert!(o["fg.enabled"].visible);
    // Undescribed keys show as Other keys, with a guessed type.
    let raw = st[0].raw.iter().find(|r| r.binding == "ini:world/unknown_future_key").expect("raw key");
    assert_eq!((raw.text.as_str(), raw.guess.ty), ("7", quiver_api::options::OptionType::Int));
    // The surface's metrics are listed by full id.
    assert!(st[0].metrics.contains(&"aco-dlss.fg.state".to_owned()));
    // Vanilla doesn't stack the mod: no surfaces.
    assert!(context(&f.store, None).unwrap().surfaces.is_empty());
}

#[test]
fn values_are_validated_and_stored_as_changes() {
    let f = aco();
    let (a, r) = set(&f, vec![op("world.npc_density", json!("4")), op("upscaler.preset", json!("k")), op("world.npc_density", json!(900)), op("nope.x", json!(1))]);
    assert_eq!(r.revision, 1);
    let outcomes: Vec<&Option<Outcome>> = a.outcomes.iter().map(|o| &o.2).collect();
    assert!(outcomes[0].is_none() && outcomes[1].is_none(), "{outcomes:?}");
    assert!(matches!(&outcomes[2], Some(Outcome::Rejected { reason }) if reason.contains("above the maximum 100")), "{outcomes:?}");
    assert!(matches!(&outcomes[3], Some(Outcome::Rejected { reason }) if reason.contains("no option nope.x")));
    // Only what changed is stored, typed.
    let key = format!("{}/aco-dlss", f.mod_id);
    assert_eq!(r.values[&key], BTreeMap::from([("upscaler.preset".to_owned(), json!("K")), ("world.npc_density".to_owned(), json!(4.0))]));
    assert!(r.changes.iter().any(|c| c.contains("world.npc_density 1 × → 4 ×")), "{:?}", r.changes);
    // The same value again is unchanged; a reset deletes the value.
    let (a, _) = set(&f, vec![op("world.npc_density", json!(4)), OptionOp::Reset { surface: "aco-dlss".into(), id: "upscaler.preset".into() }]);
    assert_eq!(a.outcomes[0].2, Some(Outcome::Unchanged));
    assert!(a.outcomes[1].2.is_none());
    let cur = current(&f.store, &f.profile).unwrap();
    assert!(!cur.values[&key].contains_key("upscaler.preset"));
    // Presets are one change; raw keys go under `raw`.
    let (a, r) = set(&f, vec![OptionOp::Preset { surface: "DLSS".into(), id: "crowds".into() }, OptionOp::Raw { surface: "DLSS".into(), binding: "world/unknown_future_key".into(), value: Some("9".into()) }]);
    assert!(a.changes[0].contains("preset Busy cities"), "{:?}", a.changes);
    assert_eq!(r.values[&key]["world.npc_density"], json!(5.0));
    assert_eq!(r.raw[&key]["ini:world/unknown_future_key"], "9");
    // A described key can't be set raw.
    let (a, _) = set(&f, vec![OptionOp::Raw { surface: "DLSS".into(), binding: "world/npc_density".into(), value: Some("2".into()) }]);
    assert!(matches!(&a.outcomes[0].2, Some(Outcome::Rejected { reason }) if reason.contains("is option world.npc_density")));
    assert_eq!(history(&f.store, &f.profile).unwrap().len(), 4);
}

#[test]
fn launches_render_the_shipped_file_keeping_comments_order_and_unknown_keys() {
    let f = aco();
    set(&f, vec![op("world.npc_density", json!(4)), op("fg.toggle_key", json!("f8")), OptionOp::Raw { surface: "DLSS".into(), binding: "world/unknown_future_key".into(), value: Some("9".into()) }]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    let lo = prepare_launch(&ctx, &mut plan).unwrap();
    assert_eq!(lo.revision, 1);
    let upper = f.store.profile_dir(&f.profile).join("upper/aco_dlss.ini");
    let text = std::fs::read_to_string(&upper).unwrap();
    // Byte for byte the shipped file except the three values.
    let expect = ACO_INI.replace("npc_density=1", "npc_density=4").replace("toggle_key=F9", "toggle_key=F8").replace("unknown_future_key=7", "unknown_future_key=9");
    assert_eq!(text, expect);
    assert_eq!(lo.has[&ctx.surfaces[0].key]["world.npc_density"], json!(4.0));
    // Again: no import (it's what Quiver wrote).
    assert!(import_external(&ctx, None, "test").unwrap().is_none());
    // Nothing set any more: the shipped file shows through again.
    let key = ctx.surfaces[0].key.clone();
    set(&f, vec![OptionOp::ResetAll { surface: key }]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    assert!(!upper.exists());
}

#[test]
fn hand_edits_are_imported_as_undoable_revisions() {
    let f = aco();
    // Before options existed, the person edited the copy in the profile's upper.
    let upper = f.store.profile_dir(&f.profile).join("upper/aco_dlss.ini");
    folder(upper.parent().unwrap(), &[("aco_dlss.ini", &ACO_INI.replace("npc_density=1", "npc_density=3").replace("unknown_future_key=7", "unknown_future_key=8"))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let r = import_external(&ctx, None, "at launch").unwrap().expect("imported");
    assert_eq!(r.source, "file");
    let key = &ctx.surfaces[0].key;
    assert_eq!(r.values[key]["world.npc_density"], json!(3.0));
    assert_eq!(r.raw[key]["ini:world/unknown_future_key"], "8");
    // Rendering then keeps it; a second look finds nothing new.
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    assert!(std::fs::read_to_string(&upper).unwrap().contains("npc_density=3"));
    assert!(import_external(&ctx, None, "again").unwrap().is_none());
    // A later hand edit is compared with what Quiver wrote.
    std::fs::write(&upper, std::fs::read_to_string(&upper).unwrap().replace("npc_density=3", "npc_density=2.5")).unwrap();
    let r = import_external(&ctx, None, "after the session").unwrap().unwrap();
    assert_eq!(r.changes.len(), 1, "{:?}", r.changes);
    assert_eq!(r.values[key]["world.npc_density"], json!(2.5));
}

#[test]
fn live_rewrite_through_a_running_process_root() {
    let f = aco();
    // A stand-in game process: its /proc/<pid>/root is `/`, so the "namespace" path of the
    // install is the install itself.
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    let target = f.g.install_dir.join("aco_dlss.ini");
    std::fs::write(&target, ACO_INI).unwrap();
    let (_, r) = set(&f, vec![op("world.npc_density", json!(5))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let s = &ctx.surfaces[0];
    assert!(has_live_file(s));
    let session: Values = [(s.key.clone(), BTreeMap::from([("fg.enabled".to_owned(), json!(false))]))].into();
    let files = write_live(&ctx, s, &r.values, &r.raw, Some(&session), pid).unwrap();
    assert_eq!(files, ["aco_dlss.ini"]);
    let text = std::fs::read_to_string(&target).unwrap();
    assert_eq!(text, ACO_INI.replace("npc_density=1", "npc_density=5").replace("[fg]\r\nmode=d3d12\r\nenabled=1", "[fg]\r\nmode=d3d12\r\nenabled=0"));
    // No temp file left behind; the write is recorded, so it isn't imported back.
    assert!(std::fs::read_dir(&f.g.install_dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains("quiver-tmp")));
    assert!(load_record(&f.store).files.values().any(|r| r.written.get("ini:world/npc_density").map(String::as_str) == Some("5")));
    // A process that's gone can't be written through.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(write_live(&ctx, s, &r.values, &r.raw, None, pid).is_err());
    let _ = &f.t;
}

#[test]
fn outcomes_say_what_happened() {
    let f = aco();
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let cur = current(&f.store, &f.profile).unwrap();
    let ops = vec![op("world.npc_density", json!(3)), op("fg.mode", json!("off")), op("fg.present_thread", json!("0")), op("world.npc_distance", json!("lots"))];
    let a = apply_ops(&ctx, &cur.values, &cur.raw, &ops).unwrap();
    let key = ctx.surfaces[0].key.clone();
    let kinds = |o: &[OpOutcome]| o.iter().map(|x| x.outcome.clone()).collect::<Vec<_>>();
    // Not running: everything is next launch.
    let o = outcomes(&ctx, &a, false, &BTreeMap::new());
    assert_eq!(o[0].outcome, Outcome::SavedRestart { running: false });
    assert!(matches!(o[3].outcome, Outcome::Rejected { .. }));
    // Running, file rewritten: live for live options, at/restart for the others.
    let o = outcomes(&ctx, &a, true, &BTreeMap::from([(key.clone(), Ok(LiveChannel::File))]));
    let k = kinds(&o);
    assert_eq!(k[0], Outcome::AppliedLive { channel: LiveChannel::File, latency: Some("about a minute".into()) });
    assert_eq!(k[1], Outcome::SavedRestart { running: true });
    assert!(matches!(&k[2], Outcome::SavedAt { at } if at.contains("swapchain")), "{k:?}");
    // Running, but the write failed: never "applied".
    let o = outcomes(&ctx, &a, true, &BTreeMap::from([(key, Err("gone".into()))]));
    assert_eq!(o[0].outcome, Outcome::SavedPending { detail: "gone".into() });
}

#[test]
fn schemas_travel_with_versions_and_values_carry_over() {
    let f = aco();
    set(&f, vec![op("world.npc_density", json!(4)), op("fg.mode", json!("off"))]);
    // v2 ships its own schema (at the plain folder's root; it isn't laid over the install),
    // where fg.mode lost "off" and npc_density was renamed.
    let schema = "format = 1\nsurface = \"aco-dlss\"\ntitle = \"DLSS\"\n[file.ini]\npath = \"aco_dlss.ini\"\nowner = \"quiver\"\nreload = \"poll\"\n\
                  [option.\"fg.mode\"]\ntype = \"enum\"\nvalues = [{ value = \"d3d12\" }]\n\
                  [option.\"world.crowd\"]\ntype = \"float\"\nkey = \"npc_density\"\nsection = \"world\"\nrenamed_from = [\"world.npc_density\"]\nmin = 1\nmax = 5\napply = \"live\"\n";
    let v2 = folder(&f.t.join("v2"), &[("aco_dlss.ini", ACO_INI), (quiver_api::options::SCHEMA_FILE, schema)]);
    let c = f.store.add_version("DLSS", &NewVersion { path: Some(v2), ..Default::default() }, "cli").unwrap();
    assert!(!f.store.layer_dir(&c.version.id).join(quiver_api::options::SCHEMA_FILE).exists(), "not an install file");
    assert!(c.warnings.iter().any(|w| w.contains("schema shipped with v2")), "{:?}", c.warnings);
    assert!(c.warnings.iter().any(|w| w.contains("fg.mode=off is dormant")), "{:?}", c.warnings);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let s = &ctx.surfaces[0];
    assert_eq!((s.resolved.source, s.key.clone()), (SchemaSource::Author, format!("{}/aco-dlss", f.mod_id)), "same key across versions");
    let st = state(&ctx, s, &current(&f.store, &f.profile).unwrap(), &RunInfo::default(), None);
    assert_eq!(st.options["world.crowd"].value, json!(4.0), "renamed_from maps the stored value");
    assert!(st.options["fg.mode"].dormant.is_some());
    assert_eq!(st.options["fg.mode"].value, json!("d3d12"), "dormant: the default is used");
    // v3 without a schema carries v2's forward.
    let v3 = folder(&f.t.join("v3"), &[("aco_dlss.ini", ACO_INI)]);
    let c = f.store.add_version("DLSS", &NewVersion { path: Some(v3), ..Default::default() }, "cli").unwrap();
    assert!(c.warnings.iter().any(|w| w.contains("carried forward")), "{:?}", c.warnings);
    assert!(version_schema_path(&f.store, &c.version.id).is_file());
    // A broken schema is refused before anything is added.
    let bad = folder(&f.t.join("v4"), &[("aco_dlss.ini", ACO_INI), (quiver_api::options::SCHEMA_FILE, "format = 1\nsurface = \"x\"\n[option.a]\ntype = \"nope\"\n")]);
    assert!(f.store.add_version("DLSS", &NewVersion { path: Some(bad), ..Default::default() }, "cli").is_err());
    assert_eq!(f.store.find_mod("DLSS").unwrap().versions.len(), 3);
}

#[test]
fn local_schemas_and_inference() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let g = game(t, 77);
    folder(&g.install_dir, &[("game.exe", "e")]);
    let store = ModStore::new(&t.join("data"), 77).with_game(&g, None);
    let src = folder(&t.join("m"), &[("mod.ini", "[a]\n; on | off\nmode=on\nlevel=3\n")]);
    let l = store.install(&Package::Dir { path: src, name: Some("Thing".into()) }, "cli").unwrap().layer;
    let rec = store.find_mod("Thing").unwrap();
    assert!(resolve_schema(&store, &rec, &l).is_none(), "nothing matches: no surface");
    let text = std::fs::read_to_string(store.layer_dir(&l.id).join("mod.ini")).unwrap();
    let guessed = infer::to_toml(&infer::infer_schema("thing", "Thing", "mod.ini", &text));
    let (s, rev) = set_local_schema(&store, &rec.id, &guessed, "agent", Some("first cut".into())).unwrap();
    assert_eq!((s.options.len(), rev), (2, 1));
    let r = resolve_schema(&store, &rec, &l).unwrap().unwrap();
    assert_eq!(r.source, SchemaSource::Local);
    assert!(r.note.contains("local, by agent (r1)"), "{}", r.note);
    assert!(set_local_schema(&store, &rec.id, "format = 1\nsurface = \"Bad Id\"\n", "agent", None).is_err());
    // Program-owned (inferred files default to it): patched in the upper, not rendered.
    let p = store.create_profile("P", std::slice::from_ref(&l.id), "cli", None).unwrap();
    let ctx = context(&store, Some(&p.profile.id)).unwrap();
    let cur = current(&store, &p.profile.id).unwrap();
    let a = apply_ops(&ctx, &cur.values, &cur.raw, &[OptionOp::Set { surface: "thing".into(), id: "a.level".into(), value: json!(5) }]).unwrap();
    append(&store, &p.profile.id, a.values, a.raw, "cli", None, None, a.changes).unwrap();
    let ctx = context(&store, Some(&p.profile.id)).unwrap();
    let mut plan = store.plan(Some(&p.profile.id)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    let upper = store.profile_dir(&p.profile.id).join("upper/mod.ini");
    assert_eq!(std::fs::read_to_string(&upper).unwrap(), "[a]\n; on | off\nmode=on\nlevel=5\n");
    // The program rewrites the key itself: imported as the game's change.
    std::fs::write(&upper, "[a]\n; on | off\nmode=on\nlevel=2\n").unwrap();
    let r = import_external(&ctx, None, "after the session").unwrap().unwrap();
    assert_eq!(r.source, "game");
    assert_eq!(r.values[&ctx.surfaces[0].key]["a.level"], json!(2));
}

#[test]
fn prefix_files_go_through_config_patches_and_the_profile_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let g = game(t, 1651);
    folder(&g.install_dir, &[("game.exe", "e")]);
    let cfg = "drive_c/users/steamuser/Documents/My Games/UnrealEngine3/ACCGame/ACCGame/Config/PCConsole-ACCSystemSettings.ini";
    let fixture = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mods-v2/PCConsole-ACCSystemSettings.ini")).unwrap();
    folder(&g.prefix, &[(cfg, &fixture)]);
    let store = ModStore::new(&t.join("data"), 1651).with_game(&g, None);
    let af = store.install(&Package::Builtin("acc-china-anisotropy-16".into()), "cli").unwrap().layer;
    let p = store.create_profile("AF", std::slice::from_ref(&af.id), "cli", None).unwrap().profile.id;
    let ctx = context(&store, Some(&p)).unwrap();
    assert_eq!(ctx.surfaces.len(), 1, "{:?}", ctx.problems);
    let st = state(&ctx, &ctx.surfaces[0], &OptionsRevision::default(), &RunInfo::default(), None);
    let af_opt = &st.options["SystemSettings.MaxAnisotropy"];
    // The mod's patch sets 16 over the file's 4.
    assert_eq!(af_opt.value, json!("16"));
    assert!(matches!(&af_opt.from, ValueSource::ModPatch { mod_id } if mod_id == &af.id));
    assert_eq!(st.options["SystemSettings.UseVsync"].value, json!(true));
    // The profile overrides it: one reconcile pass, the profile's value last.
    let cur = current(&store, &p).unwrap();
    let a = apply_ops(&ctx, &cur.values, &cur.raw, &[OptionOp::Set { surface: "Texture filtering".into(), id: "SystemSettings.MaxAnisotropy".into(), value: json!("8") }, OptionOp::Set { surface: "acc-china-graphics".into(), id: "SystemSettings.UseVsync".into(), value: json!("off") }]).unwrap();
    append(&store, &p, a.values, a.raw, "cli", None, None, a.changes).unwrap();
    let ctx = context(&store, Some(&p)).unwrap();
    let mut plan = store.plan(Some(&p)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    plan.apply_patches().unwrap();
    let text = std::fs::read_to_string(g.prefix.join(cfg)).unwrap();
    assert_eq!(text, fixture.replace("MaxAnisotropy=4\r\n", "MaxAnisotropy=8\r\n").replace("UseVsync=True\r\n", "UseVsync=False\r\n"), "only those values changed");
    // The game's menu changes it back: imported, attributed to the game.
    std::fs::write(g.prefix.join(cfg), text.replace("MaxAnisotropy=8", "MaxAnisotropy=4")).unwrap();
    let r = import_external(&ctx, None, "after the session").unwrap().unwrap();
    assert_eq!(r.source, "game");
    assert_eq!(r.values[&ctx.surfaces[0].key]["SystemSettings.MaxAnisotropy"], json!("4"));
}

#[test]
fn key_bindings_normalize_and_conflict() {
    let f = aco();
    set(&f, vec![op("upscaler.toggle_key", json!("F12"))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let b = key_bindings(&ctx, &current(&f.store, &f.profile).unwrap());
    assert!(b.iter().any(|k| k.key == "F9" && k.id == "fg.toggle_key"));
    let c = keys::conflicts(&b);
    assert!(c.iter().any(|c| c.key == "F12"), "DLSS F12 vs Steam's screenshot key: {c:?}");
}

#[test]
fn static_metrics_for_the_perf_catalog() {
    let f = aco();
    let m = static_metrics(&f.store).unwrap();
    assert_eq!(m.len(), 1);
    assert!(m[0].2.iter().any(|d| d.id == "aco-dlss.fg.state" && d.kind == quiver_api::options::MetricKind::State));
}

#[test]
fn imports_compare_with_the_version_the_copy_was_rendered_from() {
    let f = aco();
    set(&f, vec![op("fg.toggle_key", json!("f8")), op("world.npc_density", json!(3))]);
    let upper = f.store.profile_dir(&f.profile).join("upper/aco_dlss.ini");
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    let key = ctx.surfaces[0].key.clone();
    // Edited back to the shipped value by hand: the pin goes, the version's default shows through.
    std::fs::write(&upper, std::fs::read_to_string(&upper).unwrap().replace("npc_density=3", "npc_density=1")).unwrap();
    let r = import_external(&ctx, None, "while the game ran").unwrap().expect("imported");
    assert!(!r.values[&key].contains_key("world.npc_density"), "{:?}", r.values);
    assert!(import_external(&ctx, None, "again").unwrap().is_none());
    // v2 ships other values. The copy rendered from v1 isn't a hand edit of v2's values.
    let v2 = folder(&f.t.join("v2"), &[("aco_dlss.ini", &ACO_INI.replace("npc_distance=1", "npc_distance=2"))]);
    f.store.add_version("DLSS", &NewVersion { path: Some(v2), ..Default::default() }, "cli").unwrap();
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    assert!(import_external(&ctx, None, "at launch").unwrap().is_none(), "nothing pinned from v1's text");
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    let text = std::fs::read_to_string(&upper).unwrap();
    assert!(text.contains("npc_distance=2") && text.contains("toggle_key=F8"), "{text}");
    // A value the schema doesn't know yet is recorded once, not on every look.
    std::fs::write(&upper, text.replace("mode=d3d12", "mode=future")).unwrap();
    let r = import_external(&ctx, None, "while the game ran").unwrap().expect("recorded");
    assert!(r.changes[0].contains("doesn't fit"), "{:?}", r.changes);
    assert!(import_external(&ctx, None, "while the game ran").unwrap().is_none());
}

#[test]
fn shipped_values_outside_the_schema_are_never_rewritten() {
    let f = aco();
    // v2 ships values the catalog's ranges don't cover (a newer mod than the catalog).
    let shipped = ACO_INI.replace("npc_density=1", "npc_density=400").replace("mode=d3d12", "mode=future");
    let v2 = folder(&f.t.join("v2"), &[("aco_dlss.ini", &shipped)]);
    f.store.add_version("DLSS", &NewVersion { path: Some(v2), ..Default::default() }, "cli").unwrap();
    // Another option is set, so the file is rendered.
    set(&f, vec![op("fg.toggle_key", json!("f8"))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    let upper = f.store.profile_dir(&f.profile).join("upper/aco_dlss.ini");
    assert_eq!(std::fs::read_to_string(&upper).unwrap(), shipped.replace("toggle_key=F9", "toggle_key=F8"), "only the set value changes");
    // And the rendered copy isn't imported back as an edit.
    assert!(import_external(&ctx, None, "after the session").unwrap().is_none());
}

/// Set Quiver Tweaks values at the global scope (design/TWEAKS.md).
fn tweak_global(f: &Fixture, ops: &[(&str, Value)]) {
    let data = f.store.data_dir().to_owned();
    let at = crate::tweaks::At::Global(&data);
    let cur = at.current().unwrap();
    let ops: Vec<OptionOp> = ops.iter().map(|(id, v)| OptionOp::Set { surface: "quiver".into(), id: (*id).into(), value: v.clone() }).collect();
    let a = crate::tweaks::apply_ops(crate::tweaks::schema(), at.kind(), &cur, &Default::default(), &ops);
    assert!(a.outcomes.iter().all(|o| o.1.is_none()), "{:?}", a.outcomes);
    at.append(a.values, a.prefer, "cli", None, a.changes).unwrap();
}

#[test]
fn quiver_tweaks_flow_into_a_provider_s_option_below_the_profile_s_value() {
    use quiver_api::tweaks::{Planned, ScopeKind};
    let f = aco();
    tweak_global(&f, &[("fps.cap", json!(160)), ("dlss.sr.model", json!("latest")), ("dlss.fg.frames", json!("4x")), ("tex.anisotropy", json!("16"))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let s = &ctx.surfaces[0];
    let rev = current(&f.store, &f.profile).unwrap();
    let st = state(&ctx, s, &rev, &RunInfo::default(), None);
    let cap = &st.options["reflex.fps_limit"];
    assert_eq!(cap.value, json!(160.0));
    assert!(matches!(&cap.from, ValueSource::Tweak { tweak, scope: ScopeKind::Global, revision: 1 } if tweak == "fps.cap"), "{:?}", cap.from);
    assert_eq!(st.options["upscaler.preset"].value, json!("default"));
    // The launch writes the bound values into the mod's file.
    let mut plan = f.store.plan(Some(&f.profile)).unwrap();
    prepare_launch(&ctx, &mut plan).unwrap();
    let text = std::fs::read_to_string(f.store.profile_dir(&f.profile).join("upper/aco_dlss.ini")).unwrap();
    assert!(text.contains("preset=default") && text.contains("fps_limit=160"), "{text}");
    // Quiver's own mechanisms stand down; the mod's FG blocks multi-frame overrides; the rest applies.
    let caps = quiver_api::tweaks::Capabilities {
        gpu: Some(quiver_api::tweaks::Gpu { vendor: "nvidia".into(), name: "NVIDIA GeForce RTX 5070".into(), rtx_series: Some(50), arch: None }),
        proton: Some(quiver_api::tweaks::ProtonCaps { nvapi: Some("v0.9.3".into()), dxvk: Some("v3.1.1".into()), ..Default::default() }),
        route: "quiver".into(),
        api: "unknown".into(),
        ..Default::default()
    };
    let game = quiver_api::GameRef::ubisoft(3539);
    let c = crate::tweaks::plan_for(f.store.data_dir(), &game, Some(&ctx), caps.clone(), &Default::default(), None).unwrap();
    assert!(matches!(&c.plan.entry("fps.cap").unwrap().planned, Planned::Provided { by, .. } if by == "DLSS"));
    assert!(matches!(&c.plan.entry("dlss.fg.frames").unwrap().planned, Planned::Blocked { reason } if reason.starts_with("DLSS mod generates")), "{:?}", c.plan.entry("dlss.fg.frames"));
    assert!(matches!(&c.plan.entry("tex.anisotropy").unwrap().planned, Planned::Apply { mech, .. } if mech == "dxvk"));
    assert_eq!(c.plan.env["DXVK_CONFIG"], "d3d11.samplerAnisotropy = 16; d3d9.samplerAnisotropy = 16");
    assert_eq!(c.plan.revisions.global, 1);
    // The profile's own DLSS value wins over the tweak.
    set(&f, vec![op("reflex.fps_limit", json!(120))]);
    let ctx = context(&f.store, Some(&f.profile)).unwrap();
    let rev = current(&f.store, &f.profile).unwrap();
    let st = state(&ctx, &ctx.surfaces[0], &rev, &RunInfo::default(), None);
    assert_eq!(st.options["reflex.fps_limit"].value, json!(120.0));
    assert_eq!(st.options["reflex.fps_limit"].default, json!(160.0), "the tweak is the default layer under it");
    let c = crate::tweaks::plan_for(f.store.data_dir(), &game, Some(&ctx), caps, &Default::default(), None).unwrap();
    assert!(matches!(&c.plan.entry("fps.cap").unwrap().planned, Planned::Provided { value: Some(v), .. } if v == &json!(120.0)));
}
