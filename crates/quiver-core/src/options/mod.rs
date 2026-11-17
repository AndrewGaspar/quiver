//! Mod options on disk (design/MOD-CONFIG.md; wire types in `quiver_api::options`).
//!
//! ```text
//! <data>/mods/<product>/layers/<version>.options.toml   the schema a version shipped (author, a)
//! <data>/mods/<product>/options/<mod id>.toml           a local schema for an installed mod (c)
//! <data>/games/<product>/options/schemas/<mod id>.jsonl its revisions
//! <data>/games/<product>/options/<profile id>.jsonl     a profile's values, whole-state revisions
//! <data>/games/<product>/options/sessions/<s>.json      a play session's live changes (revert, crashes)
//! <data>/mods/<product>/options-applied.json            files Quiver rendered: shipped and written hashes
//! <data>/mods/<product>/profiles/<id>/options-patches.json  keys patched in program-owned install files
//! ```
//!
//! **Schemas** come, per surface and as a whole, from the first of: the version's own
//! `quiver-options.toml` (carried forward to versions without one), Quiver's catalog
//! (`options/*.toml`, matched by product, mod id, shipped files and hashes), a local schema.
//! Keys a schema doesn't describe are "Other keys", with guessed types ([`infer`]).
//!
//! **Values** are a profile's delta over the defaults, keyed by `<mod id>/<surface>` so they
//! carry across versions. Resolution, lowest first: the default (the shipped file's value for
//! `owner = "quiver"` files, else the schema's `default`, else the file's own value), a mod's
//! config patch on the same key, the profile's value, a session-only value.
//!
//! **Files.** `owner = "quiver"` install files are rendered at every launch from the shipped
//! file of the resolved version plus the values, into the profile's install upper (a copy
//! with no values is removed so new versions' files show through). Other files are patched
//! with `confpatch` semantics (the file's own values recorded; keys no longer wanted go back):
//! prefix files in the same reconcile pass as the mods' config patches, program-owned
//! install files against the profile's upper. Everything is written before the launch's
//! namespace is mounted; while it runs, files with `reload` are rewritten *through* the
//! namespace (`/proc/<pid>/root`), never under the live mount. Edits made outside Quiver
//! (by hand, or by the program in its own menu) are imported as revisions first.

pub mod infer;
pub mod keys;
pub mod schema;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use quiver_api::mods::{ConfigPatch, Layer, ModRecord, PatchFormat, Profile, VANILLA, mods_of};
use quiver_api::options::{
    ActionInfo, Apply, DormantValue, FileDef, FileFormat, FileOwner, FileRoot, GroupState, Level, LiveChannel, LiveInfo, MetricDecl, OpOutcome,
    OptionDef, OptionOp, OptionState, OptionsRevision, Outcome, PageState, Pending, PresetInfo, RawKey, RawValues, Reload, Schema, SchemaSource,
    SessionOptions, SurfaceFile, SurfaceKind, SurfaceState, ValueSource, Values, same_value, scalar_text,
};

use crate::confpatch::{self, Ini, View, Wanted};
use crate::mods::{ModStore, Plan};
use crate::overlay::OverlaySpec;
use crate::settings::{append_jsonl, read_jsonl};
use schema::Binding;

include!(concat!(env!("OUT_DIR"), "/options_catalog.rs"));

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

fn games_dir(store: &ModStore) -> PathBuf {
    crate::paths::game_dir_in(store.data_dir(), &quiver_api::GameRef::ubisoft(store.product_id())).join("options")
}

fn mods_dir(store: &ModStore) -> PathBuf {
    crate::paths::mods_dir_in(store.data_dir(), &quiver_api::GameRef::ubisoft(store.product_id()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("path")?)?;
    let tmp = path.with_file_name(format!(".{}.quiver-tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, bytes)?;
    if let Ok(m) = path.metadata() {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

// -- values ---------------------------------------------------------------------------------

fn values_path(store: &ModStore, profile: &str) -> PathBuf {
    games_dir(store).join(format!("{profile}.jsonl"))
}

/// A profile's options history (oldest first; deleted profiles keep theirs).
pub fn history(store: &ModStore, profile: &str) -> Result<Vec<OptionsRevision>> {
    read_jsonl(&values_path(store, profile))
}

/// The current values (revision 0, empty, when there is no history).
pub fn current(store: &ModStore, profile: &str) -> Result<OptionsRevision> {
    Ok(history(store, profile)?.pop().unwrap_or_default())
}

/// Record a new whole-state revision.
#[allow(clippy::too_many_arguments)]
pub fn append(
    store: &ModStore,
    profile: &str,
    values: Values,
    raw: RawValues,
    source: &str,
    session: Option<String>,
    message: Option<String>,
    changes: Vec<String>,
) -> Result<OptionsRevision> {
    let prev = current(store, profile)?;
    let mut values = values;
    values.retain(|_, m| !m.is_empty());
    let mut raw = raw;
    raw.retain(|_, m| !m.is_empty());
    let rev = OptionsRevision { revision: prev.revision + 1, at: chrono::Utc::now(), source: source.to_owned(), session, message, changes, values, raw };
    append_jsonl(&values_path(store, profile), &rev)?;
    Ok(rev)
}

fn session_path(store: &ModStore, session: &str) -> PathBuf {
    games_dir(store).join("sessions").join(format!("{session}.json"))
}

pub fn save_session(store: &ModStore, s: &SessionOptions) -> Result<()> {
    write_atomic(&session_path(store, &s.session), &serde_json::to_vec_pretty(s)?)
}

pub fn load_session(store: &ModStore, session: &str) -> Result<SessionOptions> {
    if session.is_empty() || session.contains(['/', '.']) {
        bail!("{session:?} isn't a session id");
    }
    let text = std::fs::read(session_path(store, session)).with_context(|| format!("no options record for session {session} of this game"))?;
    Ok(serde_json::from_slice(&text)?)
}

/// The game's session records, newest first (unreadable ones skipped).
pub fn sessions(store: &ModStore) -> Vec<SessionOptions> {
    let dir = games_dir(store).join("sessions");
    let mut out: Vec<SessionOptions> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started));
    out
}

/// Files in a version's folder that look like settings (INI-like text): what `options
/// infer` can read. Install-relative, `/`-separated, sorted; at most `max`.
pub fn config_files(dir: &Path, max: usize) -> Vec<String> {
    const EXT: &[&str] = &["ini", "cfg", "conf", "txt", "toml", "json", "xml", "yaml", "yml", "properties"];
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
            } else if meta.len() <= 1 << 20
                && p.extension().and_then(|x| x.to_str()).is_some_and(|x| EXT.contains(&x.to_ascii_lowercase().as_str()))
                && let Ok(rel) = p.strip_prefix(dir)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort_by_key(|p| (p.matches('/').count(), p.to_ascii_lowercase()));
    out.truncate(max);
    out
}

// -- schemas --------------------------------------------------------------------------------

/// Where a version's own schema is kept (beside its metadata).
pub fn version_schema_path(store: &ModStore, version: &str) -> PathBuf {
    store.layer_meta(version).with_extension("options.toml")
}

fn version_schema_json(store: &ModStore, version: &str) -> PathBuf {
    store.layer_meta(version).with_extension("options.json")
}

/// The schema file in a folder, if it has one.
pub fn schema_in(dir: &Path) -> Option<PathBuf> {
    [quiver_api::options::SCHEMA_FILE, quiver_api::options::SCHEMA_FILE_JSON].iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

/// Keep `src` (a `quiver-options.toml` or `.json`) as `version`'s schema. It's checked first:
/// a schema that doesn't parse or has errors is refused.
pub fn adopt_schema(store: &ModStore, version: &str, src: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(src).with_context(|| format!("reading {}", src.display()))?;
    let json = src.extension().is_some_and(|e| e == "json");
    let s = schema::parse(&text, json).with_context(|| src.display().to_string())?;
    schema::check(&s).with_context(|| src.display().to_string())?;
    let dst = if json { version_schema_json(store, version) } else { version_schema_path(store, version) };
    write_atomic(&dst, text.as_bytes())?;
    Ok(vec![format!("options: schema '{}' ({} options)", s.surface, s.options.len())])
}

/// A version without a schema of its own carries the previous version's forward.
pub fn carry_schema(store: &ModStore, from: &str, to: &str) -> Result<bool> {
    for (a, b) in [(version_schema_path(store, from), version_schema_path(store, to)), (version_schema_json(store, from), version_schema_json(store, to))] {
        if a.is_file() && !b.exists() && !version_schema_path(store, to).exists() && !version_schema_json(store, to).exists() {
            std::fs::copy(&a, &b)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn author_schema(store: &ModStore, version: &str) -> Option<(Result<Schema>, String)> {
    for (p, json) in [(version_schema_path(store, version), false), (version_schema_json(store, version), true)] {
        if let Ok(text) = std::fs::read_to_string(&p) {
            return Some((schema::parse(&text, json), text));
        }
    }
    None
}

/// Catalog schemas, parsed (a broken one is reported by the unit test, skipped here).
pub fn catalog() -> Vec<(&'static str, Schema, &'static str)> {
    OPTIONS_CATALOG.iter().filter_map(|(id, text)| schema::parse(text, false).ok().map(|s| (*id, s, *text))).collect()
}

/// Whether a catalog schema's `[match]` takes this version (its files are in `dir`).
pub fn catalog_matches(s: &Schema, product_id: u32, layer: &Layer, dir: &Path) -> bool {
    let Some(m) = &s.matches else { return false };
    if !m.products.is_empty() && !m.products.contains(&product_id) {
        return false;
    }
    if !m.mods.is_empty() && !layer.mod_id().is_some_and(|id| m.mods.iter().any(|x| x.eq_ignore_ascii_case(id))) {
        return false;
    }
    if m.mods.is_empty() && m.files.is_empty() && m.sha256.is_empty() {
        return false;
    }
    if !m.files.iter().all(|f| dir.join(f).is_file()) {
        return false;
    }
    m.sha256.iter().all(|(f, hashes)| std::fs::read(dir.join(f)).is_ok_and(|b| hashes.iter().any(|h| h.eq_ignore_ascii_case(&sha256_hex(&b)))))
}

fn local_path(store: &ModStore, mod_id: &str) -> PathBuf {
    mods_dir(store).join("options").join(format!("{mod_id}.toml"))
}

fn local_history_path(store: &ModStore, mod_id: &str) -> PathBuf {
    games_dir(store).join("schemas").join(format!("{mod_id}.jsonl"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSchemaRevision {
    pub revision: u32,
    pub at: chrono::DateTime<chrono::Utc>,
    pub source: String,
    #[serde(default)]
    pub message: Option<String>,
    pub text: String,
}

/// Write a local schema for a mod (checked first), as a new revision. Returns the revision.
pub fn set_local_schema(store: &ModStore, mod_id: &str, text: &str, source: &str, message: Option<String>) -> Result<(Schema, u32)> {
    let s = schema::parse(text, false)?;
    schema::check(&s)?;
    let hist: Vec<LocalSchemaRevision> = read_jsonl(&local_history_path(store, mod_id))?;
    let revision = hist.last().map_or(0, |r| r.revision) + 1;
    append_jsonl(&local_history_path(store, mod_id), &LocalSchemaRevision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, text: text.to_owned() })?;
    write_atomic(&local_path(store, mod_id), text.as_bytes())?;
    Ok((s, revision))
}

/// A version's schema as resolved (author > catalog > local).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub schema: Schema,
    pub source: SchemaSource,
    pub note: String,
    pub text: Option<String>,
    pub revision: Option<u32>,
}

/// The schema for `layer` (a version of `rec`), if any source has one; `Err` for a schema
/// that is there but broken (reported, never silently replaced by a lower source).
pub fn resolve_schema(store: &ModStore, rec: &ModRecord, layer: &Layer) -> Option<Result<Resolved>> {
    let label = rec.label_of(&layer.id);
    if let Some((parsed, text)) = author_schema(store, &layer.id) {
        return Some(parsed.map(|schema| Resolved { schema, source: SchemaSource::Author, note: format!("schema shipped with {label}"), text: Some(text), revision: None }));
    }
    let dir = store.layer_dir(&layer.id);
    if let Some((id, schema, text)) = catalog().into_iter().find(|(_, s, _)| catalog_matches(s, store.product_id(), layer, &dir)) {
        return Some(Ok(Resolved { schema, source: SchemaSource::Catalog, note: format!("Quiver catalog (options/{id}.toml)"), text: Some(text.to_owned()), revision: None }));
    }
    let local = local_path(store, &rec.id);
    if let Ok(text) = std::fs::read_to_string(&local) {
        let hist: Vec<LocalSchemaRevision> = read_jsonl(&local_history_path(store, &rec.id)).unwrap_or_default();
        let last = hist.last();
        let note = match last {
            Some(r) => format!("local, by {} (r{})", r.source, r.revision),
            None => "local".into(),
        };
        return Some(schema::parse(&text, false).map(|schema| Resolved { schema, source: SchemaSource::Local, note, text: Some(text), revision: last.map(|r| r.revision) }));
    }
    None
}

/// Static metric descriptors of every installed mod version with a schema, for the perf
/// catalog (SIGHT-HUD §3.11: `PerfCatalog { product_id }` without a running session). Each
/// entry: (surface key, version id, descriptors with full ids).
pub fn static_metrics(store: &ModStore) -> Result<Vec<(String, String, Vec<MetricDecl>)>> {
    let layers = store.layers()?;
    let mut out = Vec::new();
    for rec in mods_of(&layers, &store.records()?) {
        let Some(layer) = layers.iter().find(|l| l.id == rec.current) else { continue };
        if let Some(Ok(r)) = resolve_schema(store, &rec, layer)
            && !r.schema.metrics.is_empty()
        {
            out.push((format!("{}/{}", rec.id, r.schema.surface), layer.id.clone(), r.schema.metrics.values().cloned().collect()));
        }
    }
    Ok(out)
}

/// Where the perf catalog (another crate) receives static declarations when it wants them
/// pushed rather than pulled with [`static_metrics`].
pub trait PerfCatalogSink: Send + Sync {
    fn declare_static(&self, product_id: u32, surface_key: &str, version: &str, metrics: &[MetricDecl]);
}

// -- a profile's surfaces ---------------------------------------------------------------------

/// One file of a surface, located for a profile.
#[derive(Debug, Clone)]
pub struct FileLoc {
    pub id: String,
    pub def: FileDef,
    /// Install-relative (`root = "install"`) or prefix-relative path (pattern for prefixes).
    pub rel: String,
    /// The file the defaults come from.
    pub source: Option<PathBuf>,
    /// Where Quiver writes it (install: the profile's upper; prefix: the view's file).
    pub write: PathBuf,
    pub scope: String,
}

impl FileLoc {
    pub fn install(&self) -> bool {
        self.def.root == FileRoot::Install
    }

    /// Rendered from the shipped file + values (else patched).
    pub fn rendered(&self) -> bool {
        self.def.owner == FileOwner::Quiver && self.install() && self.def.format == FileFormat::Ini
    }

    /// Rewritten live through the running namespace.
    pub fn live(&self) -> bool {
        self.rendered() && self.def.reload != Reload::None
    }
}

#[derive(Debug, Clone)]
pub struct Surface {
    pub key: String,
    pub mod_id: String,
    pub mod_name: String,
    pub version: String,
    pub label: String,
    pub resolved: Resolved,
    pub files: Vec<FileLoc>,
}

impl Surface {
    pub fn schema(&self) -> &Schema {
        &self.resolved.schema
    }

    pub fn title(&self) -> String {
        self.schema().title.clone().unwrap_or_else(|| self.mod_name.clone())
    }

    pub fn file(&self, id: &str) -> Option<&FileLoc> {
        self.files.iter().find(|f| f.id == id)
    }

    /// Whether `query` names it: key, surface id, mod id or name.
    pub fn is(&self, query: &str) -> bool {
        let q = query.trim();
        self.key == q || self.schema().surface.eq_ignore_ascii_case(q) || self.mod_id == q || self.mod_name.eq_ignore_ascii_case(q) || self.title().eq_ignore_ascii_case(q)
    }
}

/// A profile (or vanilla) and the surfaces of its stack.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub store: ModStore,
    pub profile: Option<Profile>,
    /// `vanilla` for vanilla.
    pub profile_id: String,
    pub install: PathBuf,
    /// The install upper of the profile's launches.
    pub upper: PathBuf,
    /// Layer folders with install files, topmost first.
    pub lowers: Vec<PathBuf>,
    pub view: View,
    /// The stack's config patches (MODS-V2), in stack order.
    pub patches: Vec<Wanted>,
    pub surfaces: Vec<Surface>,
    /// Broken schemas, in words.
    pub problems: Vec<String>,
}

/// The surfaces of `profile` (an id or name; `None`/`vanilla`: vanilla). The store needs
/// the game ([`ModStore::with_game`]).
pub fn context(store: &ModStore, profile: Option<&str>) -> Result<Ctx> {
    let plan = store.plan(profile)?;
    let install = store.install_dir().context("options need the installed game")?.to_owned();
    let profile = plan.api.profile.clone();
    let profile_id = profile.as_ref().map_or_else(|| VANILLA.to_owned(), |p| p.id.clone());
    let layers = store.layers()?;
    let records = mods_of(&layers, &store.records()?);
    let stack: Vec<&Layer> = plan.api.mods.iter().filter_map(|id| layers.iter().find(|l| &l.id == id)).collect();
    let lowers: Vec<PathBuf> = stack.iter().rev().filter(|l| l.has_files()).map(|l| store.layer_dir(&l.id)).collect();
    let upper = store.profile_dir(&profile_id).join("upper");
    let mut surfaces = Vec::new();
    let mut problems = Vec::new();
    for (layer, r) in stack.iter().zip(&plan.api.resolved) {
        let Some(rec) = records.iter().find(|m| m.id == r.mod_id) else { continue };
        match resolve_schema(store, rec, layer) {
            None => {}
            Some(Err(e)) => problems.push(format!("{} {}: options schema: {e:#}", rec.name, r.label)),
            Some(Ok(resolved)) => {
                let key = format!("{}/{}", rec.id, resolved.schema.surface);
                if surfaces.iter().any(|s: &Surface| s.key == key) {
                    continue;
                }
                let files = resolved
                    .schema
                    .files
                    .iter()
                    .map(|(id, def)| locate(def, id, &install, &lowers, &upper, &plan.view, &profile_id))
                    .collect();
                surfaces.push(Surface { key, mod_id: rec.id.clone(), mod_name: rec.name.clone(), version: layer.id.clone(), label: r.label.clone(), resolved, files });
            }
        }
    }
    Ok(Ctx { store: store.clone(), profile, profile_id, install, upper, lowers, view: plan.view.clone(), patches: plan.patches.clone(), surfaces, problems })
}

fn locate(def: &FileDef, id: &str, install: &Path, lowers: &[PathBuf], upper: &Path, view: &View, profile: &str) -> FileLoc {
    let rel = match def.root {
        FileRoot::Install | FileRoot::Prefix => def.path.clone(),
        FileRoot::Docs => format!("{}/{}", FileRoot::DOCS, def.path),
    };
    match def.root {
        FileRoot::Install => {
            let shipped = lowers.iter().map(|l| l.join(&rel)).chain([install.join(&rel)]).find(|p| p.is_file());
            let source = if def.owner == FileOwner::Program && upper.join(&rel).is_file() { Some(upper.join(&rel)) } else { shipped };
            FileLoc { id: id.to_owned(), def: def.clone(), write: upper.join(&rel), source, scope: format!("install:{profile}"), rel }
        }
        _ => {
            let found = view.glob(&rel).into_iter().next();
            let source = found.as_ref().and_then(|r| view.locate(r));
            let write = view.write_path(found.as_deref().unwrap_or(&rel));
            FileLoc { id: id.to_owned(), def: def.clone(), scope: view.scope(found.as_deref().unwrap_or(&rel)), source, write, rel }
        }
    }
}

fn read_text(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok()?;
    confpatch::decode_text(&bytes).ok().map(|(t, _)| t)
}

/// One option of a surface, evaluated.
#[derive(Debug, Clone)]
pub struct Eval {
    pub def: OptionDef,
    pub binding: Option<Binding>,
    pub default: Value,
    /// The file's own text didn't fit.
    pub invalid: Option<String>,
    pub patch: Option<(String, Value)>,
    /// The stored value: normalized, or why it's dormant.
    pub stored: Option<Result<Value, (Value, String)>>,
    pub session: Option<Value>,
}

impl Eval {
    pub fn effective(&self) -> Value {
        if let Some(v) = &self.session {
            return v.clone();
        }
        if let Some(Ok(v)) = &self.stored {
            return v.clone();
        }
        self.base()
    }

    /// Without the profile's and the session's values.
    pub fn base(&self) -> Value {
        self.patch.as_ref().map_or_else(|| self.default.clone(), |p| p.1.clone())
    }

    /// The value the profile (or session) makes Quiver write, if any.
    pub fn wanted(&self) -> Option<Value> {
        self.session.clone().or_else(|| self.stored.as_ref().and_then(|s| s.as_ref().ok().cloned()))
    }
}

/// A surface evaluated for some values.
#[derive(Debug, Clone)]
pub struct SurfaceEval {
    pub options: BTreeMap<String, Eval>,
    /// The parsed default text of each file (by file id).
    pub texts: BTreeMap<String, Option<String>>,
    pub dormant: Vec<DormantValue>,
    pub raw: BTreeMap<String, String>,
}

/// The stored values for a surface, with `renamed_from` applied.
fn stored_for(s: &Surface, values: &Values) -> BTreeMap<String, Value> {
    let mut m = values.get(&s.key).cloned().unwrap_or_default();
    for (id, d) in &s.schema().options {
        for old in &d.renamed_from {
            if !m.contains_key(id)
                && let Some(v) = m.remove(old)
            {
                m.insert(id.clone(), v);
            }
        }
    }
    m
}

fn glob_match(pattern: &str, rel: &str) -> bool {
    let (p, r): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), rel.split('/').collect());
    p.len() == r.len() && p.iter().zip(&r).all(|(a, b)| confpatch::wildcard(a, b))
}

pub fn eval(ctx: &Ctx, s: &Surface, values: &Values, raw: &RawValues, session: Option<&Values>) -> SurfaceEval {
    let mut texts = BTreeMap::new();
    let mut inis = BTreeMap::new();
    for f in &s.files {
        let t = f.source.as_deref().and_then(read_text);
        if let Some(t) = &t {
            inis.insert(f.id.clone(), Ini::parse(t));
        }
        texts.insert(f.id.clone(), t);
    }
    let stored = stored_for(s, values);
    let sess = session.and_then(|v| v.get(&s.key)).cloned().unwrap_or_default();
    let mut options = BTreeMap::new();
    for (id, d) in &s.schema().options {
        let binding = schema::binding(s.schema(), id, d).ok();
        let file_text = binding.as_ref().and_then(|b| inis.get(&b.file).and_then(|i| i.get(&b.section, &b.key).last().cloned()));
        let (default, invalid) = match &file_text {
            Some(t) => match d.parse_text(t).and_then(|v| d.validate(&v).map(|_| v)) {
                Ok(v) => (v, None),
                Err(e) => (Value::String(t.clone()), Some(format!("the file has {t:?}: {e}"))),
            },
            None => (d.default.clone().unwrap_or(Value::Null), None),
        };
        // A mod's config patch on the same key (prefix files).
        let patch = binding.as_ref().and_then(|b| {
            let f = s.file(&b.file)?;
            if f.install() {
                return None;
            }
            ctx.patches
                .iter()
                .rev()
                .filter(|w| w.patch.format == PatchFormat::Ini && glob_match(&w.patch.file, &f.rel) && w.patch.section.eq_ignore_ascii_case(&b.section))
                .find_map(|w| w.patch.set.iter().find(|(k, _)| k.eq_ignore_ascii_case(&b.key)).map(|(_, v)| (w.mod_id.clone(), v.clone())))
                .and_then(|(m, v)| d.parse_text(&v).ok().map(|v| (m, v)))
        });
        let stored = stored.get(id).map(|v| d.normalize(v).map_err(|e| (v.clone(), e)));
        let session = sess.get(id).cloned();
        options.insert(id.clone(), Eval { def: d.clone(), binding, default, invalid, patch, stored, session });
    }
    let dormant = stored
        .iter()
        .filter_map(|(id, v)| match options.get(id) {
            None => Some(DormantValue { id: id.clone(), value: v.clone(), reason: format!("{} {} has no option {id}", s.mod_name, s.label) }),
            Some(Eval { stored: Some(Err((_, e))), .. }) => Some(DormantValue { id: id.clone(), value: v.clone(), reason: format!("{} {}: {e}; using the default", s.mod_name, s.label) }),
            _ => None,
        })
        .collect();
    SurfaceEval { options, texts, dormant, raw: raw.get(&s.key).cloned().unwrap_or_default() }
}

/// What the running session knows, for [`states`].
#[derive(Default)]
pub struct RunInfo<'a> {
    pub session: Option<String>,
    /// A process to write through exists (file channel).
    pub running: bool,
    /// Surface keys with a connected SDK channel.
    pub sdk: BTreeSet<String>,
    /// What the running program has: surface key → id → value.
    pub has: Option<&'a Values>,
    /// Session-only values (`Live::Only`).
    pub session_values: Option<&'a Values>,
}

fn level_ok(d: &OptionDef, level: Option<Level>) -> bool {
    level.is_none_or(|l| d.level <= l)
}

/// Every surface of a profile as clients render it (C2).
pub fn states(ctx: &Ctx, rev: &OptionsRevision, run: &RunInfo<'_>, level: Option<Level>) -> Vec<SurfaceState> {
    ctx.surfaces.iter().map(|s| state(ctx, s, rev, run, level)).collect()
}

pub fn state(ctx: &Ctx, s: &Surface, rev: &OptionsRevision, run: &RunInfo<'_>, level: Option<Level>) -> SurfaceState {
    let ev = eval(ctx, s, &rev.values, &rev.raw, run.session_values);
    let sch = s.schema();
    let file_live = s.files.iter().any(FileLoc::live);
    let sdk = run.sdk.contains(&s.key);
    let channel = if sdk { LiveChannel::Sdk } else if file_live { LiveChannel::File } else { LiveChannel::None };
    let live = LiveInfo { channel, connected: sdk || (file_live && run.running), session: run.session.clone() };
    let changed_by = |id: &str| rev_source(ctx, &s.key, id, rev);
    let effective: BTreeMap<String, Value> = ev.options.iter().map(|(id, e)| (id.clone(), e.effective())).collect();
    let get = |id: &str| effective.get(id).cloned();
    let mut options = BTreeMap::new();
    for (id, e) in &ev.options {
        if !level_ok(&e.def, level) {
            continue;
        }
        let value = e.effective();
        let from = if let Some(v) = &e.session {
            let _ = v;
            ValueSource::Session { source: "session".into() }
        } else if matches!(e.stored, Some(Ok(_))) {
            changed_by(id)
        } else if let Some((m, _)) = &e.patch {
            ValueSource::ModPatch { mod_id: m.clone() }
        } else {
            ValueSource::Default
        };
        let pending = match run.has.and_then(|h| h.get(&s.key)).map(|h| h.get(id).cloned().unwrap_or_else(|| e.base())) {
            Some(has) if !same_value(&has, &value) => match e.def.apply {
                Apply::Live if live.connected => Pending::None,
                Apply::Live => Pending::Reread,
                Apply::At => Pending::At { at: e.def.at.clone().unwrap_or_default() },
                Apply::Restart | Apply::Launch => Pending::Restart,
            },
            _ => Pending::None,
        };
        let dormant = match &e.stored {
            Some(Err((v, why))) => Some(format!("{} is kept but not applied: {why}", scalar_text(v))),
            _ => None,
        };
        options.insert(
            id.clone(),
            OptionState {
                text: e.def.to_text(&value),
                visible: e.def.visible(&get),
                def: e.def.clone(),
                default: e.base(),
                value,
                from,
                pending,
                dormant,
                invalid: e.invalid.clone(),
            },
        );
    }
    // Pages: as declared, then "Other" for the rest.
    let mut placed = BTreeSet::new();
    let mut pages = Vec::new();
    for p in &sch.pages {
        let mut groups = Vec::new();
        let keep = |ids: &[String], placed: &mut BTreeSet<String>| -> Vec<String> {
            ids.iter().filter(|i| options.contains_key(*i)).inspect(|i| {
                placed.insert((*i).clone());
            }).cloned().collect()
        };
        let top = keep(&p.options, &mut placed);
        if !top.is_empty() {
            groups.push(GroupState { title: String::new(), options: top });
        }
        for g in &p.groups {
            let ids = keep(&g.options, &mut placed);
            if !ids.is_empty() {
                groups.push(GroupState { title: g.title.clone(), options: ids });
            }
        }
        if !groups.is_empty() {
            pages.push(PageState { id: p.id.clone(), title: p.title.clone(), groups });
        }
    }
    let rest: Vec<String> = options.keys().filter(|k| !placed.contains(*k)).cloned().collect();
    if !rest.is_empty() {
        pages.push(PageState { id: "other".into(), title: "Other".into(), groups: vec![GroupState { title: String::new(), options: rest }] });
    }
    let actions = sch
        .actions
        .iter()
        .map(|(id, a)| {
            let hotkey = a.hotkey_option.as_ref().and_then(|k| ev.options.get(k)).and_then(|e| keys::normalize_option(&e.def, &e.def.to_text(&e.effective())));
            (id.clone(), ActionInfo { label: a.label.clone(), hotkey })
        })
        .collect();
    let (undescribed, mut lint) = schema::lint_files(sch, &ev.texts);
    lint.retain(|f| f.severity != quiver_api::options::Severity::Note);
    let raw = raw_keys(s, &ev, &undescribed, rev);
    let surface_files = s
        .files
        .iter()
        .map(|f| SurfaceFile { id: f.id.clone(), def: f.def.clone(), rel: f.rel.clone(), source: f.source.clone(), write: Some(f.write.clone()), scope: f.scope.clone() })
        .collect();
    SurfaceState {
        key: s.key.clone(),
        surface: sch.surface.clone(),
        title: s.title(),
        help: sch.help.clone(),
        kind: SurfaceKind::Mod,
        mod_id: Some(s.mod_id.clone()),
        version: Some(s.version.clone()),
        version_label: Some(s.label.clone()),
        schema_source: s.resolved.source,
        schema_note: s.resolved.note.clone(),
        files: surface_files,
        live,
        pages,
        options,
        presets: sch.presets.iter().map(|p| PresetInfo { id: p.id.clone(), label: p.label.clone(), help: p.help.clone(), set: p.set.clone() }).collect(),
        metrics: sch.metrics.values().map(|m| m.id.clone()).collect(),
        actions,
        raw,
        dormant: ev.dormant.clone(),
        lint: lint.iter().map(|f| f.to_string()).collect(),
    }
}

fn raw_keys(s: &Surface, ev: &SurfaceEval, undescribed: &[String], rev: &OptionsRevision) -> Vec<RawKey> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for b in undescribed.iter().chain(ev.raw.keys()) {
        let Ok(binding) = Binding::parse_raw(b, None) else { continue };
        if !seen.insert(binding.raw_key().to_lowercase()) {
            continue;
        }
        let entries = ev.texts.get(&binding.file).cloned().flatten().map(|t| infer::ini_entries(&t)).unwrap_or_default();
        let entry = entries.iter().find(|(sec, k, _, _)| sec.eq_ignore_ascii_case(&binding.section) && k.eq_ignore_ascii_case(&binding.key));
        let default = entry.map(|e| e.2.clone()).unwrap_or_default();
        let comment = entry.map(|e| e.3.clone()).unwrap_or_default();
        let set = ev.raw.iter().find(|(k, _)| k.eq_ignore_ascii_case(&binding.raw_key())).map(|(_, v)| v.clone());
        out.push(RawKey {
            text: set.clone().unwrap_or_else(|| default.clone()),
            guess: infer::guess(&binding.key, &default, &comment),
            changed: set.map(|_| ValueSource::Profile { revision: rev.revision, source: rev.source.clone() }),
            default,
            binding: binding.raw_key(),
        });
    }
    let _ = s;
    out
}

/// Which revision set an option to its current value, and who.
fn rev_source(ctx: &Ctx, key: &str, id: &str, rev: &OptionsRevision) -> ValueSource {
    let hist = history(&ctx.store, &ctx.profile_id).unwrap_or_default();
    let cur = rev.values.get(key).and_then(|m| m.get(id));
    let mut at = rev.clone();
    for r in hist.iter().rev() {
        if r.values.get(key).and_then(|m| m.get(id)) == cur {
            at = r.clone();
        } else {
            break;
        }
    }
    ValueSource::Profile { revision: at.revision, source: at.source }
}

// -- changes ----------------------------------------------------------------------------------

/// One op applied: (surface key, option id or raw binding, its outcome if already known,
/// value before, value after).
pub type OpResult = (String, String, Option<Outcome>, Option<Value>, Option<Value>);

/// The result of applying ops to a set of values (nothing written yet).
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub values: Values,
    pub raw: RawValues,
    /// One per op, outcome `Unchanged` or `Rejected` so far, else filled in by the caller
    /// (`saved_*`/`applied_live`): `None` here.
    pub outcomes: Vec<OpResult>,
    pub changes: Vec<String>,
    /// Options changed that have `danger` (labels and sentences).
    pub dangers: Vec<String>,
}

fn find_surface<'a>(ctx: &'a Ctx, q: &str) -> Result<&'a Surface> {
    let found: Vec<&Surface> = ctx.surfaces.iter().filter(|s| s.is(q)).collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "no options surface '{q}' in {} (surfaces: {})",
            ctx.profile.as_ref().map_or("vanilla".to_owned(), |p| format!("profile '{}'", p.name)),
            if ctx.surfaces.is_empty() { "none".into() } else { ctx.surfaces.iter().map(|s| format!("{} ({})", s.title(), s.key)).collect::<Vec<_>>().join(", ") }
        ),
        _ => bail!("'{q}' names several surfaces; use a key: {}", found.iter().map(|s| s.key.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

pub fn surface<'a>(ctx: &'a Ctx, q: &str) -> Result<&'a Surface> {
    find_surface(ctx, q)
}

/// Apply `ops` to `base` (validated against each surface's schema).
pub fn apply_ops(ctx: &Ctx, base_values: &Values, base_raw: &RawValues, ops: &[OptionOp]) -> Result<Applied> {
    let mut a = Applied { values: base_values.clone(), raw: base_raw.clone(), ..Default::default() };
    for op in ops {
        let surface_q = match op {
            OptionOp::Set { surface, .. } | OptionOp::Reset { surface, .. } | OptionOp::Preset { surface, .. } | OptionOp::Raw { surface, .. } | OptionOp::ResetAll { surface } => surface,
        };
        let s = match find_surface(ctx, surface_q) {
            Ok(s) => s,
            Err(e) => {
                let id = match op {
                    OptionOp::Set { id, .. } | OptionOp::Reset { id, .. } | OptionOp::Preset { id, .. } => id.clone(),
                    OptionOp::Raw { binding, .. } => binding.clone(),
                    OptionOp::ResetAll { .. } => String::new(),
                };
                a.outcomes.push((surface_q.clone(), id, Some(Outcome::Rejected { reason: format!("{e:#}") }), None, None));
                continue;
            }
        };
        let ev = eval(ctx, s, &a.values, &a.raw, None);
        let title = s.title();
        let set_one = |a: &mut Applied, id: &str, v: Option<&Value>| {
            let Some(e) = ev.options.get(id) else {
                let near: Vec<&String> = ev.options.keys().filter(|k| k.contains(id) || id.contains(k.rsplit('.').next().unwrap_or(k))).take(4).collect();
                let hint = if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) };
                a.outcomes.push((s.key.clone(), id.to_owned(), Some(Outcome::Rejected { reason: format!("{title} has no option {id}{hint}") }), None, None));
                return;
            };
            let before = e.effective();
            let new = match v {
                Some(v) => match e.def.normalize(v) {
                    Ok(n) => Some(n),
                    Err(why) => {
                        a.outcomes.push((s.key.clone(), id.to_owned(), Some(Outcome::Rejected { reason: format!("{title} {id}: {why}") }), Some(before), None));
                        return;
                    }
                },
                None => None,
            };
            let after = new.clone().unwrap_or_else(|| e.base());
            let m = a.values.entry(s.key.clone()).or_default();
            let had = m.get(id).cloned();
            // A value equal to the default is stored all the same (it pins the choice
            // against a later version's new default); reset deletes it.
            match &new {
                Some(n) => {
                    m.insert(id.to_owned(), n.clone());
                }
                None => {
                    m.remove(id);
                }
            }
            if had.as_ref().zip(new.as_ref()).is_some_and(|(x, y)| same_value(x, y)) || (had.is_none() && new.is_none()) {
                a.outcomes.push((s.key.clone(), id.to_owned(), Some(Outcome::Unchanged), Some(before), Some(after)));
                return;
            }
            if let Some(d) = &e.def.danger {
                a.dangers.push(format!("{title} {id}: {d}"));
            }
            a.changes.push(format!(
                "{title} {id} {} → {}{}",
                e.def.display(&before),
                e.def.display(&after),
                if new.is_none() { " (reset)" } else { "" }
            ));
            a.outcomes.push((s.key.clone(), id.to_owned(), None, Some(before), Some(after)));
        };
        match op {
            OptionOp::Set { id, value, .. } => set_one(&mut a, id, Some(value)),
            OptionOp::Reset { id, .. } => set_one(&mut a, id, None),
            OptionOp::ResetAll { .. } => {
                let ids: Vec<String> = stored_for(s, &a.values).keys().cloned().collect();
                for id in ids {
                    if ev.options.contains_key(&id) {
                        set_one(&mut a, &id, None);
                    }
                }
                if let Some(r) = a.raw.remove(&s.key) {
                    a.changes.extend(r.keys().map(|b| format!("{title} {b} reset")));
                }
            }
            OptionOp::Preset { id, .. } => match s.schema().presets.iter().find(|p| p.id == *id || p.label.eq_ignore_ascii_case(id)) {
                None => a.outcomes.push((s.key.clone(), id.clone(), Some(Outcome::Rejected { reason: format!("{title} has no preset {id}") }), None, None)),
                Some(p) => {
                    let start = a.changes.len();
                    for (oid, v) in &p.set {
                        set_one(&mut a, oid, Some(v));
                    }
                    if a.changes.len() > start {
                        a.changes.insert(start, format!("{title} preset {}", p.label));
                    }
                }
            },
            OptionOp::Raw { binding, value, .. } => {
                let default_file = (s.schema().files.len() == 1).then(|| s.schema().files.keys().next().cloned()).flatten();
                let b = match Binding::parse_raw(binding, default_file.as_deref()) {
                    Ok(b) if s.schema().files.contains_key(&b.file) => b,
                    Ok(b) => {
                        a.outcomes.push((s.key.clone(), binding.clone(), Some(Outcome::Rejected { reason: format!("{title} has no file {}", b.file) }), None, None));
                        continue;
                    }
                    Err(e) => {
                        a.outcomes.push((s.key.clone(), binding.clone(), Some(Outcome::Rejected { reason: format!("{e:#}") }), None, None));
                        continue;
                    }
                };
                if let Some(id) = ev.options.iter().find(|(_, e)| e.binding.as_ref().is_some_and(|x| x.raw_key().eq_ignore_ascii_case(&b.raw_key()))).map(|(id, _)| id.clone()) {
                    a.outcomes.push((s.key.clone(), binding.clone(), Some(Outcome::Rejected { reason: format!("{} is option {id}; set that instead", b.raw_key()) }), None, None));
                    continue;
                }
                if value.as_deref().is_some_and(|v| v.contains(['\n', '\r'])) {
                    a.outcomes.push((s.key.clone(), binding.clone(), Some(Outcome::Rejected { reason: "a value can't contain a line break".into() }), None, None));
                    continue;
                }
                let m = a.raw.entry(s.key.clone()).or_default();
                let old = m.iter().find(|(k, _)| k.eq_ignore_ascii_case(&b.raw_key())).map(|(k, v)| (k.clone(), v.clone()));
                if let Some((k, _)) = &old {
                    m.remove(k);
                }
                if let Some(v) = value {
                    m.insert(b.raw_key(), v.clone());
                }
                if old.as_ref().map(|o| &o.1) == value.as_ref() {
                    a.outcomes.push((s.key.clone(), b.raw_key(), Some(Outcome::Unchanged), None, None));
                } else {
                    a.changes.push(format!("{title} {} {} → {}", b.raw_key(), old.map_or("(file's)".into(), |o| o.1), value.clone().unwrap_or_else(|| "(file's)".into())));
                    a.outcomes.push((s.key.clone(), b.raw_key(), None, None, value.clone().map(Value::String)));
                }
            }
        }
    }
    Ok(a)
}

/// Fill in the outcome of each changed op from its option's `apply` and what reached the
/// running game. `delivered`: surface key → the live channel that took it (file written,
/// SDK acked), or why not.
pub fn outcomes(ctx: &Ctx, a: &Applied, running: bool, delivered: &BTreeMap<String, Result<LiveChannel, String>>) -> Vec<OpOutcome> {
    a.outcomes
        .iter()
        .map(|(key, id, done, before, after)| {
            let outcome = match done {
                Some(o) => o.clone(),
                None => {
                    let def = ctx.surfaces.iter().find(|s| &s.key == key).and_then(|s| s.schema().options.get(id));
                    let apply = def.map_or(Apply::Restart, |d| d.apply);
                    match (running, delivered.get(key), apply) {
                        (false, _, _) => Outcome::SavedRestart { running: false },
                        (true, _, Apply::Restart | Apply::Launch) => Outcome::SavedRestart { running: true },
                        (true, Some(Ok(_)), Apply::At) => Outcome::SavedAt { at: def.and_then(|d| d.at.clone()).unwrap_or_default() },
                        (true, Some(Ok(ch)), Apply::Live) => Outcome::AppliedLive { channel: *ch, latency: def.and_then(|d| d.latency.clone()) },
                        (true, Some(Err(why)), Apply::Live | Apply::At) => Outcome::SavedPending { detail: why.clone() },
                        (true, None, Apply::At) => Outcome::SavedRestart { running: true },
                        (true, None, Apply::Live) => Outcome::SavedPending { detail: "the running game can't be reached (no live channel for this file); applies next launch".into() },
                    }
                }
            };
            OpOutcome { surface: key.clone(), id: id.clone(), outcome, before: before.clone(), after: after.clone() }
        })
        .collect()
}

// -- rendering and patching --------------------------------------------------------------------

/// The text Quiver writes for a rendered file: `base` (the shipped file) with the surface's
/// wanted values and raw values for it. `None`: nothing to change (no values).
pub fn render(s: &Surface, f: &FileLoc, ev: &SurfaceEval) -> Option<(String, BTreeMap<String, String>)> {
    let base = ev.texts.get(&f.id).cloned().flatten().unwrap_or_default();
    let mut ini = Ini::parse(&base);
    let mut written = BTreeMap::new();
    for e in ev.options.values() {
        let (Some(b), Some(v)) = (&e.binding, e.wanted()) else { continue };
        if b.file != f.id {
            continue;
        }
        let text = e.def.to_text(&v);
        ini.set(&b.section, &b.key, std::slice::from_ref(&text));
        written.insert(b.raw_key(), text);
    }
    for (k, v) in &ev.raw {
        let Ok(b) = Binding::parse_raw(k, None) else { continue };
        if b.file == f.id {
            ini.set(&b.section, &b.key, std::slice::from_ref(v));
            written.insert(b.raw_key(), v.clone());
        }
    }
    let _ = s;
    (!written.is_empty()).then(|| (ini.render(), written))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RenderRecord {
    #[serde(default)]
    files: BTreeMap<String, Rendered>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Rendered {
    shipped_sha: String,
    written_sha: String,
    written: BTreeMap<String, String>,
    at: chrono::DateTime<chrono::Utc>,
}

fn record_path(store: &ModStore) -> PathBuf {
    mods_dir(store).join("options-applied.json")
}

fn load_record(store: &ModStore) -> RenderRecord {
    std::fs::read(record_path(store)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_record(store: &ModStore, r: &RenderRecord) -> Result<()> {
    write_atomic(&record_path(store), &serde_json::to_vec_pretty(r)?)
}

fn record_key(f: &FileLoc) -> String {
    format!("{}|{}", f.scope, f.rel)
}

fn patches_record(ctx: &Ctx) -> PathBuf {
    ctx.store.profile_dir(&ctx.profile_id).join("options-patches.json")
}

/// The confpatch wants for a surface's patched (not rendered) files.
fn wants(s: &Surface, ev: &SurfaceEval, install: bool) -> Vec<Wanted> {
    let mut by_file: BTreeMap<(String, String), BTreeMap<String, String>> = BTreeMap::new();
    for e in ev.options.values() {
        let (Some(b), Some(v)) = (&e.binding, e.wanted()) else { continue };
        let Some(f) = s.file(&b.file) else { continue };
        if f.rendered() || f.install() != install || f.def.format != FileFormat::Ini {
            continue;
        }
        by_file.entry((f.rel.clone(), b.section.clone())).or_default().insert(b.key.clone(), e.def.to_text(&v));
    }
    for (k, v) in &ev.raw {
        let Ok(b) = Binding::parse_raw(k, None) else { continue };
        let Some(f) = s.file(&b.file) else { continue };
        if !f.rendered() && f.install() == install {
            by_file.entry((f.rel.clone(), b.section.clone())).or_default().insert(b.key.clone(), v.clone());
        }
    }
    by_file
        .into_iter()
        .map(|((file, section), set)| Wanted { mod_id: format!("options:{}", s.key), patch: ConfigPatch { file, format: PatchFormat::Ini, section, set, ..Default::default() } })
        .collect()
}

/// What preparing a launch did.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// The options revision the launch runs.
    pub revision: u32,
    /// What the program starts with: surface key → id → effective value.
    pub has: Values,
    pub lines: Vec<String>,
    /// A revision recording edits made outside Quiver, if there were any.
    pub imported: Option<OptionsRevision>,
}

/// Prepare a launch of `ctx`'s profile: import outside edits, render `owner = "quiver"`
/// files into the upper, patch program-owned install files, and add the prefix files' values
/// to the plan's config patches (applied with the mods' in one pass). Before the namespace
/// is mounted. Vanilla gets an install mount when it has install-file values.
pub fn prepare_launch(ctx: &Ctx, plan: &mut Plan) -> Result<LaunchOptions> {
    let mut out = LaunchOptions { imported: import_external(ctx, None, "at launch")?, ..Default::default() };
    if let Some(r) = &out.imported {
        out.lines.push(format!("options: imported r{} ({})", r.revision, r.changes.join("; ")));
    }
    let rev = current(&ctx.store, &ctx.profile_id)?;
    out.revision = rev.revision;
    let mut record = load_record(&ctx.store);
    let mut install_wants = Vec::new();
    for s in &ctx.surfaces {
        let ev = eval(ctx, s, &rev.values, &rev.raw, None);
        out.has.insert(s.key.clone(), ev.options.iter().map(|(id, e)| (id.clone(), e.effective())).collect());
        for f in s.files.iter().filter(|f| f.rendered()) {
            let key = record_key(f);
            match render(s, f, &ev) {
                Some((text, written)) => {
                    write_atomic(&f.write, text.as_bytes())?;
                    let shipped = ev.texts.get(&f.id).cloned().flatten().unwrap_or_default();
                    record.files.insert(key, Rendered { shipped_sha: sha256_hex(shipped.as_bytes()), written_sha: sha256_hex(text.as_bytes()), written: written.clone(), at: chrono::Utc::now() });
                    out.lines.push(format!("options: rendered {} ({} value(s), {})", f.rel, written.len(), s.title()));
                }
                None => {
                    // Nothing set: the shipped file shows through (our old copy goes).
                    if let Some(r) = record.files.remove(&key)
                        && std::fs::read(&f.write).is_ok_and(|b| sha256_hex(&b) == r.written_sha)
                    {
                        std::fs::remove_file(&f.write)?;
                        out.lines.push(format!("options: {} back to the shipped file", f.rel));
                    }
                }
            }
        }
        install_wants.extend(wants(s, &ev, true));
        plan.patches.extend(wants(s, &ev, false));
    }
    save_record(&ctx.store, &record)?;
    // Program-owned install files: copied up (as overlayfs would) and patched in the upper.
    for w in &install_wants {
        let dst = ctx.upper.join(&w.patch.file);
        if !dst.exists()
            && let Some(src) = ctx.lowers.iter().map(|l| l.join(&w.patch.file)).chain([ctx.install.join(&w.patch.file)]).find(|p| p.is_file())
        {
            std::fs::create_dir_all(dst.parent().context("path")?)?;
            std::fs::copy(src, &dst)?;
        }
    }
    let st = confpatch::reconcile(&patches_record(ctx), &View::prefix_only(&ctx.upper), &install_wants)?;
    out.lines.extend(crate::launcher::describe_patches(&st).into_iter().map(|l| format!("options: {l}")));
    // Vanilla with install-file values needs the install mounted, as fixes with files do.
    let wants_install = ctx.surfaces.iter().any(|s| s.files.iter().any(|f| f.install() && f.write.exists()));
    if ctx.profile.is_none() && wants_install && !plan.overlays.iter().any(|o| o.target == ctx.install) {
        let state = ctx.store.profile_dir(VANILLA);
        plan.overlays.push(OverlaySpec { target: ctx.install.clone(), lower: ctx.lowers.clone(), upper: state.join("upper"), work: state.join("work") });
    }
    Ok(out)
}

/// Import values that changed outside Quiver as one revision (§4 "When the game or the mod
/// writes the file itself"): rendered files whose copy differs from what Quiver wrote (a hand
/// edit), and program-owned keys with profile values the program has changed since Quiver
/// set them. `root`: read the install through it (a running namespace,
/// `/proc/<pid>/root`); `None`: the host paths. Last writer wins; every import is a revision.
pub fn import_external(ctx: &Ctx, root: Option<&Path>, when: &str) -> Result<Option<OptionsRevision>> {
    let rev = current(&ctx.store, &ctx.profile_id)?;
    let (mut values, mut raw) = (rev.values.clone(), rev.raw.clone());
    let mut changes = Vec::new();
    let mut sources = BTreeSet::new();
    let record = load_record(&ctx.store);
    let applied = confpatch::applied(&patches_record(ctx)).unwrap_or_default();
    let prefix_applied = confpatch::applied(&ctx.store.patches_record()).unwrap_or_default();
    for s in &ctx.surfaces {
        let ev = eval(ctx, s, &values, &raw, None);
        for f in &s.files {
            let path = match (root, f.install()) {
                (Some(r), true) => r.join(ctx.install.strip_prefix("/").unwrap_or(&ctx.install)).join(&f.rel),
                _ => f.write.clone(),
            };
            let Some(text) = read_text(&path) else { continue };
            let ini = Ini::parse(&text);
            if f.rendered() {
                let rec = record.files.get(&record_key(f));
                if rec.is_some_and(|r| r.written_sha == sha256_hex(text.as_bytes())) {
                    continue;
                }
                let shipped_text = ev.texts.get(&f.id).cloned().flatten().unwrap_or_default();
                let shipped = Ini::parse(&shipped_text);
                // The copy was rendered from an older version's file: compare with that one, so
                // the new version's own values aren't mistaken for hand edits (and don't get
                // pinned over the new version). `Some(None)`: that file is gone.
                let rendered_from: Option<Option<Ini>> = rec.filter(|r| r.shipped_sha != sha256_hex(shipped_text.as_bytes())).map(|r| older_base(ctx, f, &r.shipped_sha));
                for (sec, key) in schema::ini_keys(&ini) {
                    let b = Binding { file: f.id.clone(), section: sec.clone(), key: key.clone() };
                    let now = ini.get(&sec, &key).last().cloned().unwrap_or_default();
                    let written = rec.and_then(|r| r.written.iter().find(|(k, _)| k.eq_ignore_ascii_case(&b.raw_key())).map(|(_, v)| v.clone()));
                    let base = match &rendered_from {
                        None => shipped.get(&sec, &key).last().cloned(),
                        Some(Some(old)) => old.get(&sec, &key).last().cloned(),
                        Some(None) => None,
                    };
                    if written.is_none() && matches!(rendered_from, Some(None)) {
                        // Can't tell an edit from the old version's text: the new version wins.
                        continue;
                    }
                    if written.as_ref().or(base.as_ref()) == Some(&now) {
                        continue;
                    }
                    // Edited back to the shipped value: that's "the version's default", not a pin.
                    let to_default = base.as_deref() == Some(now.as_str());
                    import_key(s, &ev, &b, &now, &mut values, &mut raw, &mut changes, true, to_default);
                    sources.insert("file");
                }
            } else {
                let scope_rel = if f.install() { ("prefix".to_owned(), f.rel.clone()) } else { (f.scope.clone(), f.rel.clone()) };
                let list = if f.install() { &applied } else { &prefix_applied };
                for e in ev.options.values() {
                    let (Some(b), Some(v)) = (&e.binding, e.wanted()) else { continue };
                    if b.file != f.id {
                        continue;
                    }
                    let want = e.def.to_text(&v);
                    let was_set = list.iter().any(|k| {
                        k.scope == scope_rel.0 && glob_match(&scope_rel.1, &k.file) && k.target.eq_ignore_ascii_case(&b.section) && k.key.eq_ignore_ascii_case(&b.key) && k.applied.as_deref() == Some(want.as_str())
                    });
                    let Some(now) = ini.get(&b.section, &b.key).last().cloned() else { continue };
                    if was_set && now != want {
                        import_key(s, &ev, b, &now, &mut values, &mut raw, &mut changes, false, false);
                        sources.insert("game");
                    }
                }
            }
        }
    }
    if changes.is_empty() {
        return Ok(None);
    }
    let (source, what) = if sources.contains("game") { ("game", "Changed outside Quiver (the game or its own menu)") } else { ("file", "Edited by hand") };
    Ok(Some(append(&ctx.store, &ctx.profile_id, values, raw, source, None, Some(format!("{what}; found {when}")), changes)?))
}

/// The older version's copy of `f` whose text hashes to `sha` (any installed layer).
fn older_base(ctx: &Ctx, f: &FileLoc, sha: &str) -> Option<Ini> {
    let dirs = std::fs::read_dir(ctx.store.layers_dir()).ok()?;
    dirs.flatten().map(|d| d.path().join(&f.rel)).filter_map(|p| read_text(&p)).find(|t| sha256_hex(t.as_bytes()) == sha).map(|t| Ini::parse(&t))
}

/// `to_default`: the file has the shipped value again, so a stored value is dropped (the
/// version's default shows through) rather than `now` being pinned.
#[allow(clippy::too_many_arguments)]
fn import_key(s: &Surface, ev: &SurfaceEval, b: &Binding, now: &str, values: &mut Values, raw: &mut RawValues, changes: &mut Vec<String>, raw_ok: bool, to_default: bool) {
    let title = s.title();
    match ev.options.iter().find(|(_, e)| e.binding.as_ref().is_some_and(|x| x.raw_key().eq_ignore_ascii_case(&b.raw_key()))) {
        Some((id, e)) if to_default => {
            let stored = values.get_mut(&s.key).and_then(|m| m.remove(id));
            if let Some(v) = stored {
                changes.push(format!("{title} {id} {} → the version's default ({now})", e.def.display(&v)));
            }
        }
        None if to_default => {
            if raw.get_mut(&s.key).and_then(|m| m.remove(&b.raw_key())).is_some() {
                changes.push(format!("{title} {} → the version's default ({now})", b.raw_key()));
            }
        }
        Some((id, e)) => {
            let before = e.effective();
            match e.def.parse_text(now).and_then(|v| e.def.validate(&v).map(|_| v)) {
                Ok(v) => {
                    if same_value(&v, &before) {
                        return;
                    }
                    changes.push(format!("{title} {id} {} → {}", e.def.display(&before), e.def.display(&v)));
                    values.entry(s.key.clone()).or_default().insert(id.clone(), v);
                }
                Err(err) => {
                    // Recorded as a fact, kept dormant (once: the file keeps differing).
                    if values.get(&s.key).and_then(|m| m.get(id)) == Some(&Value::String(now.to_owned())) {
                        return;
                    }
                    changes.push(format!("{title} {id} → {now:?} (doesn't fit: {err})"));
                    values.entry(s.key.clone()).or_default().insert(id.clone(), Value::String(now.to_owned()));
                }
            }
        }
        None if raw_ok => {
            let m = raw.entry(s.key.clone()).or_default();
            if m.get(&b.raw_key()).map(String::as_str) != Some(now) {
                changes.push(format!("{title} {} → {now}", b.raw_key()));
                m.insert(b.raw_key(), now.to_owned());
            }
        }
        None => {}
    }
}

// -- live ---------------------------------------------------------------------------------------

/// Rewrite a surface's `reload` files inside a running game's namespace, through
/// `/proc/<pid>/root` (temp file + rename in the same folder, so the mod never reads half a
/// file). `values`/`raw` are what the running game should have (saved values plus session
/// ones). Returns the files written; records what was written so it isn't imported back.
pub fn write_live(ctx: &Ctx, s: &Surface, values: &Values, raw: &RawValues, session: Option<&Values>, pid: u32) -> Result<Vec<String>> {
    write_live_at(ctx, s, values, raw, session, &PathBuf::from(format!("/proc/{pid}/root")))
}

/// [`write_live`] through any root (tests use `/`).
pub fn write_live_at(ctx: &Ctx, s: &Surface, values: &Values, raw: &RawValues, session: Option<&Values>, root: &Path) -> Result<Vec<String>> {
    let ev = eval(ctx, s, values, raw, session);
    let mut record = load_record(&ctx.store);
    let mut out = Vec::new();
    for f in s.files.iter().filter(|f| f.live()) {
        let base = ev.texts.get(&f.id).cloned().flatten().unwrap_or_default();
        let (text, written) = render(s, f, &ev).unwrap_or_else(|| (base.clone(), BTreeMap::new()));
        let inside = root.join(ctx.install.strip_prefix("/").unwrap_or(&ctx.install)).join(&f.rel);
        if !inside.parent().is_some_and(Path::is_dir) {
            bail!("{} isn't visible in the running game ({})", f.rel, inside.display());
        }
        write_atomic(&inside, text.as_bytes()).with_context(|| format!("rewriting {} in the running game", f.rel))?;
        record.files.insert(record_key(f), Rendered { shipped_sha: sha256_hex(base.as_bytes()), written_sha: sha256_hex(text.as_bytes()), written, at: chrono::Utc::now() });
        out.push(f.rel.clone());
    }
    save_record(&ctx.store, &record)?;
    Ok(out)
}

/// Any live-rewritable file in a surface.
pub fn has_live_file(s: &Surface) -> bool {
    s.files.iter().any(FileLoc::live)
}

// -- keys ----------------------------------------------------------------------------------------

/// Every binding in play for a profile (C7).
pub fn key_bindings(ctx: &Ctx, rev: &OptionsRevision) -> Vec<quiver_api::options::KeyBinding> {
    let mut out = Vec::new();
    for s in &ctx.surfaces {
        let ev = eval(ctx, s, &rev.values, &rev.raw, None);
        for (id, e) in &ev.options {
            if e.def.ty != quiver_api::options::OptionType::Key {
                continue;
            }
            let text = e.def.to_text(&e.effective());
            if let Some(key) = keys::normalize_option(&e.def, &text) {
                out.push(quiver_api::options::KeyBinding { key, raw: text, owner: s.title(), surface: s.key.clone(), id: id.clone(), label: e.def.label_or(id).to_owned(), kind: "option".into() });
            }
        }
    }
    out.extend(keys::known_bindings());
    out
}

// -- versions -----------------------------------------------------------------------------------

/// Lint a version's schema against its files (for `version add`): findings in words.
pub fn lint_version(store: &ModStore, rec: &ModRecord, layer: &Layer) -> Vec<String> {
    let Some(r) = resolve_schema(store, rec, layer) else { return Vec::new() };
    let r = match r {
        Ok(r) => r,
        Err(e) => return vec![format!("options schema: {e:#}")],
    };
    let dir = store.layer_dir(&layer.id);
    let texts: BTreeMap<String, Option<String>> = r.schema.files.iter().filter(|(_, f)| f.root == FileRoot::Install).map(|(id, f)| (id.clone(), read_text(&dir.join(&f.path)))).collect();
    let (undescribed, findings) = schema::lint_files(&r.schema, &texts);
    let mut out = vec![format!("options: {} ({} options, {} keys undescribed)", r.note, r.schema.options.len(), undescribed.len())];
    out.extend(findings.iter().filter(|f| f.severity != quiver_api::options::Severity::Note).map(|f| format!("options: {f}")));
    out
}

/// Profiles' stored values the version's schema doesn't take (dormant), in words.
pub fn dormant_in_profiles(store: &ModStore, rec: &ModRecord, layer: &Layer) -> Vec<String> {
    let Some(Ok(r)) = resolve_schema(store, rec, layer) else { return Vec::new() };
    let key = format!("{}/{}", rec.id, r.schema.surface);
    let mut out = Vec::new();
    let mut ids: Vec<String> = store.profiles().unwrap_or_default().into_iter().map(|(p, _)| p.id).collect();
    ids.push(VANILLA.into());
    for pid in ids {
        let Ok(cur) = current(store, &pid) else { continue };
        for (id, v) in cur.values.get(&key).into_iter().flatten() {
            let why = match r.schema.options.get(id) {
                None if !r.schema.options.values().any(|d| d.renamed_from.contains(id)) => Some(format!("{} has no option {id}", rec.label_of(&layer.id))),
                None => None,
                Some(d) => d.normalize(v).err(),
            };
            if let Some(why) = why {
                out.push(format!("profile {pid}: {id}={} is dormant ({why})", scalar_text(v)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
