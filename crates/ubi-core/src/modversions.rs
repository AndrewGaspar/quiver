//! Mod versions: a mod is a stable identity with one or more versions (see
//! [`ubi_api::mods::ModRecord`]).
//!
//! ```text
//! <data>/mods/<product>/layers/<version-id>/       a version's files (never changed once added)
//! <data>/mods/<product>/layers/<version-id>.json   its metadata and manifest (a Layer)
//! <data>/games/<product>/layers/<version-id>.jsonl its deletions' history
//! <data>/games/<product>/mods/<mod-id>.jsonl       the mod's history: whole records, last is current
//! ```
//!
//! A layer that was never versioned is a mod with one version, and its id is the mod's id;
//! nothing is written for it until it gets a second version (or a label), when revision 1
//! records it as it was and revision 2 the change. So v1 and v2 stores read unchanged, and
//! a mod's id never changes. New versions get ids starting with `v`.
//!
//! Profiles stack mods by id and follow the current version; `MOD@VERSION` pins one
//! ([`ubi_api::mods::resolve_entry`]). Making a version current is how a mod is updated or
//! rolled back for every profile that tracks it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ubi_api::mods::{
    Layer, LayerChange, ModFiles, ModManifest, ModRecord, ModRevision, ModVersion, VersionChange, VersionMerge, mods_of, resolve_entry,
    split_entry, stack_entry,
};

use crate::mods::{ModStore, named};
use crate::settings::{append_jsonl, read_jsonl};

/// A version to add ([`ModStore::add_version`]).
#[derive(Debug, Clone, Default)]
pub struct NewVersion {
    /// A folder of files, or a package folder with a `quiver-mod.toml`; `None`: no files (a
    /// version that changes deletions or settings only).
    pub path: Option<PathBuf>,
    pub label: Option<String>,
    pub notes: Option<String>,
    /// Don't make it current (profiles that track the mod keep their version).
    pub no_update: bool,
    /// Added for profiles to pin and try; not current.
    pub prerelease: bool,
    /// Override the save compatibility it keeps from the current version: `Some(None)`
    /// for unknown.
    pub save_compatible: Option<Option<bool>>,
    /// More install paths to hide, on top of the current version's.
    pub deletes: Vec<String>,
    /// The version's options schema, when it isn't in the folder (`quiver-options.toml` at a
    /// plain folder's root or next to a package's `quiver-mod.toml` is found on its own).
    pub options: Option<PathBuf>,
}

/// `on`, `off`, `unknown` (or empty) to a save compatibility.
pub fn parse_save_compatible(v: &str) -> Result<Option<bool>> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(Some(true)),
        "off" | "false" | "no" | "0" => Ok(Some(false)),
        "" | "unknown" | "auto" => Ok(None),
        o => bail!("save_compatible: on, off or unknown, not '{o}'"),
    }
}

/// The next free `vN` label (N one more than the versions there are).
fn next_label(rec: &ModRecord) -> String {
    let mut n = rec.versions.len() + 1;
    loop {
        let l = format!("v{n}");
        if rec.version(&l).is_none() {
            return l;
        }
        n += 1;
    }
}

impl ModStore {
    fn mods_hist_dir(&self) -> PathBuf {
        self.data_dir().join("games").join(self.product_id().to_string()).join("mods")
    }

    pub(crate) fn mod_history_path(&self, id: &str) -> PathBuf {
        self.mods_hist_dir().join(format!("{id}.jsonl"))
    }

    /// Whether the mod has a history (it has been versioned, labelled or merged).
    pub(crate) fn versioned(&self, id: &str) -> bool {
        self.mod_history_path(id).is_file()
    }

    /// The stored records (versioned mods), merged or removed ones left out.
    pub(crate) fn records(&self) -> Result<Vec<ModRecord>> {
        let Ok(rd) = std::fs::read_dir(self.mods_hist_dir()) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in rd {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "jsonl")
                && let Some(last) = read_jsonl::<ModRevision>(&p)?.pop()
                && !last.deleted
            {
                out.push(last.record);
            }
        }
        Ok(out)
    }

    /// A mod's history by id or name (empty for a mod that was never versioned).
    pub fn mod_history_of(&self, query: &str) -> Result<(ModRecord, Vec<ModRevision>)> {
        let rec = self.find_mod(query)?;
        let hist = read_jsonl(&self.mod_history_path(&rec.id))?;
        Ok((rec, hist))
    }

    pub(crate) fn append_mod_revision(
        &self,
        rec: &ModRecord,
        deleted: bool,
        source: &str,
        message: Option<String>,
        changes: Vec<String>,
    ) -> Result<ModRevision> {
        let path = self.mod_history_path(&rec.id);
        let revision = read_jsonl::<ModRevision>(&path)?.last().map_or(0, |r| r.revision) + 1;
        let rev = ModRevision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, changes, deleted, record: rec.clone() };
        append_jsonl(&path, &rev)?;
        Ok(rev)
    }

    /// Make `old` into `new` as a revision of the mod's history; a mod without one first
    /// gets revision 1 recording `old` as it was. No change, no revision (returns the last).
    pub(crate) fn write_record(&self, old: &ModRecord, new: ModRecord, source: &str, message: Option<String>, changes: Vec<String>) -> Result<u32> {
        let path = self.mod_history_path(&old.id);
        let hist: Vec<ModRevision> = read_jsonl(&path)?;
        if hist.is_empty() {
            let first = old.versions.first();
            let msg = format!("versions: '{}' starts with one version, {} ({})", old.name, first.map_or("?", |v| v.label.as_str()), old.current);
            self.append_mod_revision(old, false, source, Some(msg), Vec::new())?;
        } else if new == *old {
            return Ok(hist.last().map_or(0, |r| r.revision));
        }
        Ok(self.append_mod_revision(&new, false, source, message, changes)?.revision)
    }

    /// A mod named without a version (versions are named separately).
    fn whole_mod(&self, query: &str) -> Result<(ModRecord, Vec<Layer>, Vec<ModRecord>)> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        if split_entry(query).1.is_some() {
            bail!("{query}: name the mod (without @VERSION)");
        }
        let (rec, pinned) = self.find_ref(&mods, &layers, query)?;
        if let Some(v) = pinned
            && v != rec.id
        {
            bail!("{query} is a version of '{}' ({}); name the mod", rec.name, rec.id);
        }
        Ok((rec, layers, mods))
    }

    /// Add a version to an installed mod (see [`NewVersion`]): its own folder, a copy of
    /// `path` (a package's manifest and files, or a plain folder that keeps the current
    /// version's manifest and deletions). It becomes current unless `no_update` or
    /// `prerelease`; the answer says which profiles now run it, and warns when a version that
    /// may change saves reaches a profile sharing vanilla's or the Default saves.
    pub fn add_version(&self, query: &str, opts: &NewVersion, source: &str) -> Result<VersionChange> {
        let (rec, layers, _) = self.whole_mod(query)?;
        let cur = layers.iter().find(|l| l.id == rec.current).with_context(|| format!("'{}' has lost its current version {}", rec.name, rec.current))?;
        let mut notes = Vec::new();
        let package = match &opts.path {
            Some(p) => crate::modpkg::read_package(&p.canonicalize().with_context(|| format!("{} not found", p.display()))?)?,
            None => None,
        };
        let (mut manifest, files, mut deletes, from_package) = match package {
            Some((m, files)) => {
                if let Some(old) = cur.mod_id().filter(|old| !old.eq_ignore_ascii_case(&m.id)) {
                    notes.push(format!("the package's id is {}, the mod's was {old}", m.id));
                }
                let deletes = m.files.delete.clone();
                (Some(m), files, deletes, true)
            }
            None => {
                // The same mod with new files: what it declares carries over.
                let m = cur.manifest.clone().map(|mut m| {
                    m.version = None;
                    m.builds.clear();
                    m.audited = None;
                    m.files = ModFiles::default();
                    m
                });
                (m, opts.path.clone(), cur.deletes.clone(), false)
            }
        };
        deletes.extend(opts.deletes.iter().cloned());
        if let Some(sc) = opts.save_compatible {
            let (id, name) = (rec.id.clone(), rec.name.clone());
            manifest.get_or_insert_with(|| ModManifest { id, name, ..Default::default() }).save_compatible = sc;
        }
        if let Some(m) = &manifest {
            crate::modpkg::validate(m)?;
        }
        let label = match opts.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => {
                if rec.version(l).is_some() {
                    bail!("'{}' already has a version '{l}' (labels are unique within a mod)", rec.name);
                }
                l.to_owned()
            }
            None => manifest
                .as_ref()
                .filter(|_| from_package)
                .and_then(|m| m.version.clone())
                .filter(|v| rec.version(v).is_none())
                .unwrap_or_else(|| next_label(&rec)),
        };
        if files.is_none() && deletes.is_empty() && manifest.is_none() {
            bail!("a version needs a folder of files, files to delete, or a manifest");
        }
        // Options: a schema named explicitly, or next to the package's manifest (a plain
        // folder's own is taken by `add_mod`). Checked before anything is added.
        let schema_file = opts.options.clone().or_else(|| if from_package { opts.path.as_deref().and_then(crate::options::schema_in) } else { None });
        if let Some(f) = &schema_file {
            let text = std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
            let s = crate::options::schema::parse(&text, f.extension().is_some_and(|e| e == "json")).with_context(|| f.display().to_string())?;
            crate::options::schema::check(&s).with_context(|| f.display().to_string())?;
        }
        let change = self.add_mod(&rec.name, files.as_deref(), &deletes, self.install_dir(), source, manifest, cur.builtin && !from_package, Some(&rec.id))?;
        let current = !opts.no_update && !opts.prerelease;
        let msg = opts.notes.clone().map(|n| format!("added {label}: {n}")).or(Some(format!("added {label}")));
        let (rec, revision) = match self.push_version(&rec, &change.layer, &label, opts.notes.clone(), current, opts.prerelease, source, msg) {
            Ok(x) => x,
            Err(e) => {
                // Don't leave a version no mod knows about.
                let _ = std::fs::remove_dir_all(self.layer_dir(&change.layer.id));
                let _ = std::fs::remove_file(self.layer_meta(&change.layer.id));
                let _ = std::fs::remove_file(self.layer_history_path(&change.layer.id));
                return Err(e);
            }
        };
        notes.extend(change.warnings);
        // Options: the version's own schema, else the previous version's carried forward;
        // linted against the new files, with profiles' values that go dormant.
        let vid = change.layer.id.clone();
        if let Some(f) = &schema_file {
            crate::options::adopt_schema(self, &vid, f)?;
        } else if crate::options::carry_schema(self, &cur.id, &vid)? {
            notes.push(format!("options: the schema of {} is carried forward", rec.label_of(&cur.id)));
        }
        notes.extend(crate::options::lint_version(self, &rec, &change.layer));
        notes.extend(crate::options::dormant_in_profiles(self, &rec, &change.layer));
        if !current {
            notes.push(format!(
                "{label} is not current{}: pin it (profile set PROFILE mods=…,'{}@{label}') or make it current (version use '{}' {label})",
                if opts.prerelease { " (pre-release)" } else { "" },
                rec.name,
                rec.name
            ));
        }
        self.version_change(rec, &change.layer.id, revision, notes)
    }

    /// Add `layer` (just created) to `rec` as version `label`, current if `current`.
    #[allow(clippy::too_many_arguments)]
    fn push_version(
        &self,
        rec: &ModRecord,
        layer: &Layer,
        label: &str,
        notes: Option<String>,
        current: bool,
        prerelease: bool,
        source: &str,
        message: Option<String>,
    ) -> Result<(ModRecord, u32)> {
        let mut new = rec.clone();
        new.versions.push(ModVersion {
            id: layer.id.clone(),
            label: label.to_owned(),
            added: layer.added,
            added_by: layer.added_by.clone(),
            notes,
            retired: false,
            prerelease,
        });
        let mut changes = vec![format!("added {label} ({})", layer.id)];
        if current {
            changes.push(format!("current: {} -> {label}", rec.label_of(&rec.current)));
            new.current = layer.id.clone();
        }
        let revision = self.write_record(rec, new.clone(), source, message, changes)?;
        Ok((new, revision))
    }

    /// Make `version` (id or label) the mod's current version, for every profile that
    /// tracks it.
    pub fn use_version(&self, query: &str, version: &str, source: &str, message: Option<String>) -> Result<VersionChange> {
        let (rec, _, _) = self.whole_mod(query)?;
        let v = rec.version(version).with_context(|| format!("'{}' has no version '{version}'", rec.name))?.clone();
        if v.retired {
            bail!("{} {} is retired; bring it back first (version unretire '{}' {})", rec.name, v.label, rec.name, v.label);
        }
        let mut new = rec.clone();
        new.current = v.id.clone();
        let mut changes = vec![format!("current: {} -> {}", rec.label_of(&rec.current), v.label)];
        if v.prerelease {
            new.versions.iter_mut().filter(|x| x.id == v.id).for_each(|x| x.prerelease = false);
            changes.push(format!("{} is no longer a pre-release", v.label));
        }
        let mut notes = Vec::new();
        let revision = if new == rec {
            notes.push(format!("{} is already the current version", v.label));
            0
        } else {
            self.write_record(&rec, new.clone(), source, message, changes)?
        };
        self.version_change(new, &v.id, revision, notes)
    }

    /// Retire a version (or bring it back): hidden from defaults, its files kept, pins to it
    /// still run it (with a note). The current version can't be retired.
    pub fn retire_version(&self, query: &str, version: &str, retired: bool, source: &str, message: Option<String>) -> Result<VersionChange> {
        let (rec, _, mods) = self.whole_mod(query)?;
        let v = rec.version(version).with_context(|| format!("'{}' has no version '{version}'", rec.name))?.clone();
        if retired && v.id == rec.current {
            bail!("{} {} is the current version; make another one current first (version use '{}' VERSION)", rec.name, v.label, rec.name);
        }
        let mut new = rec.clone();
        new.versions.iter_mut().filter(|x| x.id == v.id).for_each(|x| x.retired = retired);
        let mut notes = Vec::new();
        if retired {
            let pinned: Vec<String> =
                self.users_of(&mods, &rec.id)?.into_iter().filter(|(_, r)| r.pinned && r.version == v.id).map(|(p, _)| p.name).collect();
            if !pinned.is_empty() {
                notes.push(format!("still pinned to it (they keep running it): {}", pinned.join(", ")));
            }
        }
        let what = if retired { "retired" } else { "brought back" };
        let revision = self.write_record(&rec, new.clone(), source, message.or(Some(format!("{what} {}", v.label))), vec![format!("{what} {} ({})", v.label, v.id)])?;
        self.version_change(new, &v.id, revision, notes)
    }

    /// The answer to a version change: who runs what now, and save warnings for profiles
    /// that track the mod.
    fn version_change(&self, rec: ModRecord, vid: &str, revision: u32, mut warnings: Vec<String>) -> Result<VersionChange> {
        let layers = self.layers()?;
        let mods = mods_of(&layers, &self.records()?);
        let version = layers.iter().find(|l| l.id == vid).map(|l| named(l, &rec)).unwrap_or_default();
        let (mut tracking, mut pinned) = (Vec::new(), Vec::new());
        for (p, r) in self.users_of(&mods, &rec.id)? {
            if r.pinned {
                pinned.push(format!("{} ({})", p.name, r.label));
                continue;
            }
            tracking.push(p.name.clone());
            if let Ok(st) = self.stack(Some(&p)) {
                warnings.extend(st.warnings.iter().filter(|w| w.contains("could change saves")).map(|w| format!("profile '{}': {w}", p.name)));
                warnings.extend(st.problems.iter().map(|w| format!("profile '{}' won't launch: {w}", p.name)));
            }
        }
        Ok(VersionChange { record: rec, version, revision, tracking, pinned, warnings })
    }

    /// Make the mods `from` versions of `into` (see `ModsRequest::MergeVersions`).
    pub fn merge_versions(&self, into: &str, from: &[String], labels: &[String], dry_run: bool, source: &str) -> Result<VersionMerge> {
        let (target, layers, mods) = self.whole_mod(into)?;
        let mut sources: Vec<ModRecord> = Vec::new();
        for f in from {
            let (r, _, _) = self.whole_mod(f)?;
            if r.id == target.id || sources.iter().any(|s| s.id == r.id) {
                bail!("'{f}' is listed twice (or is the mod merged into)");
            }
            sources.push(r);
        }
        if sources.is_empty() {
            bail!("name at least one mod to merge into '{}'", target.name);
        }
        // Every version, oldest first; a label is its own if it was given (a versioned mod)
        // or came with a package's manifest, else it's numbered.
        let mut versions: Vec<(ModVersion, bool)> = Vec::new();
        for r in std::iter::once(&target).chain(&sources) {
            let own = self.versioned(&r.id);
            for v in &r.versions {
                let manifest_label = layers.iter().find(|l| l.id == v.id).and_then(|l| l.manifest.as_ref()).is_some_and(|m| m.version.is_some());
                versions.push((v.clone(), own || manifest_label));
            }
        }
        versions.sort_by(|a, b| a.0.added.cmp(&b.0.added).then_with(|| a.0.id.cmp(&b.0.id)));
        if !labels.is_empty() && labels.len() != versions.len() {
            bail!("{} labels given for {} versions", labels.len(), versions.len());
        }
        let mut seen: Vec<String> = Vec::new();
        for (i, (v, own)) in versions.iter_mut().enumerate() {
            if let Some(l) = labels.get(i) {
                v.label = l.trim().to_owned();
            } else if !*own {
                v.label = format!("v{}", i + 1);
            }
            if v.label.is_empty() || seen.iter().any(|s| s.eq_ignore_ascii_case(&v.label)) {
                if !labels.is_empty() {
                    bail!("labels must be unique and not empty ('{}')", v.label);
                }
                v.label = format!("{}-{}", v.label, i + 1);
            }
            seen.push(v.label.clone());
        }
        let versions: Vec<ModVersion> = versions.into_iter().map(|(v, _)| v).collect();
        let newest = versions.iter().rev().find(|v| !v.retired && !v.prerelease).or(versions.last()).context("no versions")?;
        let merged = ModRecord { id: target.id.clone(), name: target.name.clone(), current: newest.id.clone(), versions: versions.clone(), disabled: target.disabled };
        // The mods as they will be, to describe what each profile runs.
        let after: Vec<ModRecord> = mods
            .iter()
            .filter(|r| !sources.iter().any(|s| s.id == r.id))
            .map(|r| if r.id == target.id { merged.clone() } else { r.clone() })
            .collect();
        let members: Vec<&str> = std::iter::once(target.id.as_str()).chain(sources.iter().map(|s| s.id.as_str())).collect();
        let mut report = VersionMerge { dry_run, record: merged.clone(), ..Default::default() };
        let mut rewrites: Vec<(ubi_api::mods::Profile, String)> = Vec::new();
        for (p, _) in self.profiles()? {
            let mut entries = Vec::new();
            let mut hit: Option<String> = None;
            let mut said = Vec::new();
            for e in &p.layers {
                match resolve_entry(&mods, e) {
                    Some(r) if members.contains(&r.mod_id.as_str()) => {
                        if let Some(first) = &hit {
                            bail!("profile '{}' stacks both {first} and {}; take one of them out first", p.name, r.name);
                        }
                        hit = Some(r.name.clone());
                        // Nothing a launch runs changes: the newest tracks current, else a pin.
                        let new_e = if !r.pinned && r.version == merged.current { merged.id.clone() } else { stack_entry(&merged.id, Some(&r.version)) };
                        let now = resolve_entry(&after, &new_e).map_or_else(|| new_e.clone(), |x| x.describe());
                        if new_e != *e {
                            report.profiles.push((p.name.clone(), e.clone(), new_e.clone(), now.clone()));
                            said.push(format!("{} is now {now}", r.name));
                        }
                        entries.push(new_e);
                    }
                    _ => entries.push(e.clone()),
                }
            }
            if entries != p.layers {
                let mut q = p.clone();
                q.layers = entries;
                rewrites.push((q, format!("merge-versions: {}", said.join("; "))));
            }
        }
        let names = |r: &ModRecord| format!("{} ({})", r.name, r.id);
        report.notes.push(format!(
            "'{}' gets {} version(s): {}; current: {}",
            merged.name,
            versions.len(),
            versions.iter().map(|v| format!("{} ({})", v.label, v.id)).collect::<Vec<_>>().join(", "),
            merged.label_of(&merged.current)
        ));
        if dry_run {
            return Ok(report);
        }
        let changes = vec![
            format!("merged {}", sources.iter().map(names).collect::<Vec<_>>().join(", ")),
            format!("versions: {}", versions.iter().map(|v| format!("{} ({})", v.label, v.id)).collect::<Vec<_>>().join(", ")),
            format!("current: {}", merged.label_of(&merged.current)),
        ];
        self.write_record(&target, merged.clone(), source, Some(format!("merge-versions: {}", sources.iter().map(names).collect::<Vec<_>>().join(", "))), changes)?;
        for s in &sources {
            if self.versioned(&s.id) {
                self.append_mod_revision(s, true, source, Some(format!("merged into {}", names(&target))), Vec::new())?;
            }
            for v in &s.versions {
                if let Some(l) = layers.iter().find(|l| l.id == v.id) {
                    let label = merged.label_of(&v.id);
                    self.note_layer(l, source, format!("merged into {} as {label}", names(&target)))?;
                }
            }
        }
        for (p, msg) in rewrites {
            self.append(p, false, source, Some(msg))?;
        }
        Ok(report)
    }

    /// A note in a layer's own history (its deletions unchanged).
    fn note_layer(&self, l: &Layer, source: &str, change: String) -> Result<()> {
        let path = self.layer_history_path(&l.id);
        let hist: Vec<ubi_api::mods::LayerRevision> = read_jsonl(&path)?;
        let mut revision = hist.last().map_or(0, |r| r.revision);
        let mut add = |at, source: &str, message: Option<String>, changes: Vec<String>| -> Result<()> {
            revision += 1;
            let rev = ubi_api::mods::LayerRevision { revision, at, source: source.to_owned(), message, deletes: l.deletes.clone(), changes };
            append_jsonl(&path, &rev)
        };
        if hist.is_empty() {
            add(l.added, &l.added_by, Some("added".into()), Vec::new())?;
        }
        add(chrono::Utc::now(), source, None, vec![change])
    }

    /// Give an installed built-in mod the catalog's `manifest`: a new current version, or
    /// the version that has it already made current again.
    pub(crate) fn update_builtin(&self, rec: &ModRecord, layers: &[Layer], manifest: ModManifest, source: &str) -> Result<LayerChange> {
        let cur = layers.iter().find(|l| l.id == rec.current).with_context(|| format!("'{}' has lost its current version", rec.name))?;
        let layer_rev = |id: &str| self.layer_history(id).map(|h| h.last().map_or(0, |r| r.revision));
        if cur.manifest.as_ref() == Some(&manifest) {
            return Ok(LayerChange { layer: named(cur, rec), revision: layer_rev(&cur.id)?, warnings: vec!["already installed and up to date".into()] });
        }
        if let Some(v) = rec.versions.iter().find(|v| layers.iter().any(|l| l.id == v.id && l.manifest.as_ref() == Some(&manifest))) {
            let vc = self.use_version(&rec.id, &v.id, source, Some("the catalog's version again".into()))?;
            let revision = layer_rev(&v.id)?;
            return Ok(LayerChange { layer: vc.version, revision, warnings: vec![format!("{} is current again (it has the catalog's manifest)", v.label)] });
        }
        let from = cur.manifest.as_ref().and_then(|m| m.version.clone()).unwrap_or_else(|| "?".into());
        let to = manifest.version.clone().unwrap_or_else(|| "?".into());
        let label = manifest.version.clone().filter(|v| rec.version(v).is_none()).unwrap_or_else(|| next_label(rec));
        let deletes = manifest.files.delete.clone();
        let change = self.add_mod(&rec.name, None, &deletes, self.install_dir(), source, Some(manifest), true, Some(&rec.id))?;
        let (rec, _) = self.push_version(rec, &change.layer, &label, None, true, false, source, Some("updated from the catalog".into()))?;
        let mut warnings = change.warnings;
        warnings.push(format!("{}: manifest {from} -> {to}, as version {label} (current)", rec.name));
        Ok(LayerChange { layer: named(&change.layer, &rec), revision: change.revision, warnings })
    }

    /// Built-in mods the catalog has a newer manifest for get it as a new current version,
    /// unless someone made an older version current by hand (the newest isn't current).
    /// Returns what was (or, with `dry_run`, would be) updated.
    pub fn update_builtins(&self, dry_run: bool, source: &str) -> Result<Vec<String>> {
        let Ok(catalog) = crate::modpkg::catalog(self.product_id()) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for m in catalog {
            let layers = self.layers()?;
            let mods = mods_of(&layers, &self.records()?);
            let Some(rec) = self.builtin_mod(&mods, &layers, &m.id) else { continue };
            let newest = rec.versions.iter().filter(|v| !v.retired && !v.prerelease).max_by_key(|v| v.added);
            if newest.is_none_or(|v| v.id != rec.current) {
                continue;
            }
            if rec.versions.iter().any(|v| layers.iter().any(|l| l.id == v.id && l.manifest.as_ref() == Some(&m))) {
                continue;
            }
            let from = layers.iter().find(|l| l.id == rec.current).and_then(|l| l.manifest.as_ref()).and_then(|x| x.version.clone()).unwrap_or_else(|| "?".into());
            let line = format!("{} {from} -> {}", rec.name, m.version.as_deref().unwrap_or("?"));
            if !dry_run {
                self.update_builtin(rec, &layers, m, source)?;
            }
            out.push(line);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ubi_api::{Game, SettingOp};

    use super::*;
    use crate::mods::Package;

    fn ops(list: &[&str]) -> Vec<SettingOp> {
        list.iter().map(|s| SettingOp::parse(s).unwrap()).collect()
    }

    fn folder(root: &Path, files: &[(&str, &str)]) -> PathBuf {
        for (p, c) in files {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        root.to_owned()
    }

    fn game(t: &Path, product_id: u32) -> Game {
        Game {
            product_id,
            name: "Game".into(),
            install_dir: t.join("install"),
            prefix: t.join("pfx"),
            exe: "game.exe".into(),
            args: None,
            language: None,
        }
    }

    const STORE_V3: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mods-v2/store-v3");

    /// The live store for Origins before versions (personal paths and the account id
    /// replaced): DLSS, DLSS-v2 and DLSS-v3 as three mods (each marked save-compatible), the
    /// hand-made "Skip intros" layer and the built-in "Skip intro videos", the DLSS profile
    /// (r10) stacking DLSS-v3 and the built-in fix on the Default store, and the game's
    /// default profile set to it.
    fn v3_store(t: &Path) -> (ModStore, Game) {
        let data = t.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = std::process::Command::new("cp").arg("-a").arg(format!("{STORE_V3}/.")).arg(&data).output().unwrap();
        assert!(out.status.success());
        let layers = data.join("mods/3539/layers");
        for id in ["l9uv5ua", "lbi2b9z"] {
            folder(&layers.join(id), &[("d3d11.dll", id), ("nvngx_dlss.dll", "y"), ("aco_dlss.ini", "z")]);
        }
        folder(&layers.join("lplaao3"), &[("d3d11.dll", "v3"), ("nvngx_dlss.dll", "y"), ("nvngx_dlssg.dll", "g"), ("aco_dlss.ini", "z")]);
        for id in ["lzrticn", "mevkhrc"] {
            std::fs::create_dir_all(layers.join(id)).unwrap();
        }
        let g = game(t, 3539);
        folder(&g.install_dir, &[("ACOrigins.exe", "e")]);
        std::fs::create_dir_all(&g.prefix).unwrap();
        (ModStore::new(&data, 3539).with_game(&g, None), g)
    }

    #[test]
    fn origins_dlss_mods_merge_into_one_versioned_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = v3_store(tmp.path());
        // Before: five one-version mods; nothing is written to read them.
        let mods = store.mods().unwrap();
        let names: Vec<&str> = mods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["DLSS", "Skip intros", "DLSS-v2", "Skip intro videos", "DLSS-v3"]);
        assert!(mods.iter().all(|m| m.versions.len() == 1 && m.current == m.id));
        assert_eq!(mods[3].versions[0].label, "1", "the built-in's manifest version");
        assert!(!store.mod_history_path("l9uv5ua").exists());
        let before = store.plan(Some("DLSS")).unwrap();
        assert_eq!(before.api.mods, ["lplaao3", "mevkhrc"]);
        assert!(before.api.problems.is_empty(), "{:?}", before.api.problems);

        let from = ["DLSS-v2".to_owned(), "DLSS-v3".to_owned()];
        let dry = store.merge_versions("DLSS", &from, &[], true, "cli").unwrap();
        assert_eq!(dry.record.versions.iter().map(|v| (v.label.as_str(), v.id.as_str())).collect::<Vec<_>>(), [
            ("v1", "l9uv5ua"),
            ("v2", "lbi2b9z"),
            ("v3", "lplaao3")
        ]);
        assert_eq!(dry.record.current, "lplaao3");
        assert_eq!(dry.profiles, [("DLSS".to_owned(), "lplaao3".to_owned(), "l9uv5ua".to_owned(), "DLSS v3 (current)".to_owned())]);
        assert_eq!(store.history("ptkzxpv").unwrap().len(), 10, "a dry run changes nothing");
        assert!(!store.mod_history_path("l9uv5ua").exists());

        let r = store.merge_versions("DLSS", &from, &[], false, "cli").unwrap();
        assert_eq!(r.record, dry.record);
        let mods = store.mods().unwrap();
        assert_eq!(mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["DLSS", "Skip intros", "Skip intro videos"]);
        let (rec, hist) = store.mod_history_of("dlss").unwrap();
        assert_eq!(rec.id, "l9uv5ua", "the mod keeps the first one's id");
        assert_eq!(hist.len(), 2, "how it was, then the merge");
        assert!(hist[1].changes[0].contains("DLSS-v2 (lbi2b9z)") && hist[1].changes[0].contains("DLSS-v3 (lplaao3)"));
        // The profile now tracks DLSS, and runs exactly what it ran.
        let h = store.history("ptkzxpv").unwrap();
        assert_eq!(h.len(), 11);
        assert_eq!(h[10].profile.layers, ["l9uv5ua", "mevkhrc"]);
        assert!(h[10].message.as_deref().unwrap().contains("DLSS-v3 is now DLSS v3 (current)"), "{:?}", h[10].message);
        let after = store.plan(Some("DLSS")).unwrap();
        assert_eq!(after.api.mods, before.api.mods);
        assert_eq!(after.overlays[0].lower, before.overlays[0].lower);
        assert_eq!(after.api.resolved[0].describe(), "DLSS v3 (current)");
        assert_eq!(after.api.resolved[1].describe(), "Skip intro videos");
        // The merged versions' own histories say so; their files are untouched.
        let lh = store.layer_history("lbi2b9z").unwrap();
        assert_eq!(lh.last().unwrap().changes, ["merged into DLSS (l9uv5ua) as v2"]);
        assert_eq!(std::fs::read_to_string(store.layer_dir("lbi2b9z").join("d3d11.dll")).unwrap(), "lbi2b9z");
        // Old names and ids still find the versions.
        assert_eq!(store.layer("DLSS-v2").unwrap().id, "lbi2b9z");
        assert_eq!(store.layer("DLSS@v1").unwrap().id, "l9uv5ua");
        assert_eq!(store.layer("DLSS").unwrap().id, "lplaao3", "the mod: its current version");
        // Restoring a revision from before the merge pins the version it ran.
        store.restore_profile("DLSS", 10, "cli").unwrap();
        let p = store.plan(Some("DLSS")).unwrap();
        assert_eq!(p.api.mods, before.api.mods);
        assert_eq!(p.api.resolved[0].describe(), "DLSS v3 (pinned, current)");
        store.restore_profile("DLSS", 11, "cli").unwrap();
        // Merging again: nothing left to merge.
        assert!(store.merge_versions("DLSS", &["DLSS-v2".into()], &[], true, "cli").unwrap_err().to_string().contains("version of 'DLSS'"));

        // Shipping a new build is one command: it inherits the manifest (save-compatible),
        // becomes current, and the profile that tracks DLSS runs it next.
        let v4 = folder(&tmp.path().join("v4"), &[("d3d11.dll", "v4"), ("nvngx_dlss.dll", "y4")]);
        let opts = NewVersion { path: Some(v4), label: Some("3.8.10".into()), notes: Some("DLSS 3.8.10 DLLs".into()), ..Default::default() };
        let c = store.add_version("DLSS", &opts, "aco-dlss").unwrap();
        assert!(c.version.id.starts_with('v') && c.version.save_compatible());
        assert_eq!((c.record.current.as_str(), c.tracking.as_slice()), (c.version.id.as_str(), ["DLSS".to_owned()].as_slice()));
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.version.name, "DLSS 3.8.10");
        let p = store.plan(Some("DLSS")).unwrap();
        assert_eq!(p.api.mods, [c.version.id.clone(), "mevkhrc".into()]);
        assert_eq!(p.api.resolved[0].describe(), "DLSS 3.8.10 (current)");
        assert_eq!(store.history("ptkzxpv").unwrap().len(), 13, "the profile didn't change");
        // Labels are unique; the version's folder is its own.
        assert!(store.add_version("DLSS", &NewVersion { label: Some("V3".into()), ..opts.clone() }, "cli").unwrap_err().to_string().contains("already has"));

        // Pin a profile to an older version (by label); the others keep tracking.
        store.create_profile("Tracks", &["DLSS".into()], "cli", None).unwrap();
        let r = store.set_profile("DLSS", &ops(&["mods=DLSS@v2,aco-skip-intros"]), "cli", None).unwrap();
        assert_eq!(r.profile.layers, ["l9uv5ua@lbi2b9z", "mevkhrc"]);
        let p = store.plan(Some("DLSS")).unwrap();
        assert_eq!(p.api.mods, ["lbi2b9z", "mevkhrc"]);
        assert_eq!(p.api.resolved[0].describe(), "DLSS v2 (pinned)");
        // Roll back for everyone that tracks it.
        let c = store.use_version("DLSS", "v3", "cli", None).unwrap();
        assert_eq!((c.tracking.as_slice(), c.pinned.as_slice()), (["Tracks".to_owned()].as_slice(), ["DLSS (v2)".to_owned()].as_slice()));
        assert_eq!(store.plan(Some("Tracks")).unwrap().api.mods, ["lplaao3"]);
        assert_eq!(store.plan(Some("DLSS")).unwrap().api.mods, ["lbi2b9z", "mevkhrc"], "pins don't move");
        // Retire: not the current one; pins to a retired version still run, with a note.
        assert!(store.retire_version("DLSS", "v3", true, "cli", None).unwrap_err().to_string().contains("current version"));
        let c = store.retire_version("DLSS", "v2", true, "cli", None).unwrap();
        assert!(c.warnings[0].contains("DLSS"), "{:?}", c.warnings);
        let p = store.plan(Some("DLSS")).unwrap();
        assert!(p.api.problems.is_empty() && p.api.warnings.iter().any(|w| w.contains("pinned to v2, which is retired")), "{:?}", p.api.warnings);
        assert!(store.use_version("DLSS", "v2", "cli", None).unwrap_err().to_string().contains("retired"));
        store.retire_version("DLSS", "v2", false, "cli", None).unwrap();

        // A version that may change saves warns when it reaches a profile on Default.
        let v5 = folder(&tmp.path().join("v5"), &[("d3d11.dll", "v5")]);
        let c = store.add_version("DLSS", &NewVersion { path: Some(v5), save_compatible: Some(Some(false)), ..Default::default() }, "cli").unwrap();
        assert_eq!(c.record.label_of(&c.version.id), "v5");
        assert!(c.warnings.iter().any(|w| w.starts_with("profile 'Tracks'") && w.contains("DLSS v5 (not save-compatible) could change saves")), "{:?}", c.warnings);
        assert!(store.plan(Some("Tracks")).unwrap().api.warnings.iter().any(|w| w.contains("could change saves")));
        // A pre-release doesn't become current.
        let c = store.add_version("DLSS", &NewVersion { deletes: vec!["x.bk2".into()], prerelease: true, ..Default::default() }, "cli").unwrap();
        assert_ne!(c.record.current, c.version.id);
        assert!(c.version.deletes.contains(&"x.bk2".to_owned()) && !c.version.save_compatible(), "keeps v5's manifest and deletions");
        let hist = store.mod_history_of("DLSS").unwrap().1;
        assert!(hist.iter().all(|r| !r.record.versions.is_empty()));
        assert_eq!(store.mods().unwrap()[0].versions.len(), 6);
        // Uninstalling refuses while any version is stacked.
        assert!(store.uninstall("DLSS").unwrap_err().to_string().contains("used by"));
    }

    #[test]
    fn merge_pins_profiles_that_ran_an_older_version() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = v3_store(tmp.path());
        store.create_profile("Old", &["DLSS".into(), "Skip intros".into()], "cli", None).unwrap();
        store.create_profile("Both", &["DLSS-v2".into(), "DLSS-v3".into()], "cli", None).unwrap();
        let from = ["DLSS-v2".to_owned(), "DLSS-v3".to_owned()];
        assert!(store.merge_versions("DLSS", &from, &[], true, "cli").unwrap_err().to_string().contains("stacks both"));
        store.set_profile("Both", &ops(&["mods=DLSS-v2"]), "cli", None).unwrap();
        let labels: Vec<String> = ["2.0", "2.1", "3.0"].map(String::from).to_vec();
        let r = store.merge_versions("DLSS", &from, &labels, false, "cli").unwrap();
        let mut rows: Vec<(String, String, String)> = r.profiles.iter().map(|(p, _, e, d)| (p.clone(), e.clone(), d.clone())).collect();
        rows.sort();
        assert_eq!(rows, [
            ("Both".to_owned(), "l9uv5ua@lbi2b9z".to_owned(), "DLSS 2.1 (pinned)".to_owned()),
            ("DLSS".to_owned(), "l9uv5ua".to_owned(), "DLSS 3.0 (current)".to_owned()),
            ("Old".to_owned(), "l9uv5ua@l9uv5ua".to_owned(), "DLSS 2.0 (pinned)".to_owned()),
        ]);
        assert_eq!(store.plan(Some("Old")).unwrap().api.mods, ["l9uv5ua", "lzrticn"]);
        assert!(store.merge_versions("Skip intro videos", &["Skip intros".into()], &["a".into()], true, "cli").unwrap_err().to_string().contains("labels given"));
    }

    /// A mod whose versions patch different keys: switching reconciles the file like any
    /// change of what the mod wants (keys are recorded under the mod's id).
    #[test]
    fn switching_versions_reconciles_config_patches() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 9);
        folder(&g.install_dir, &[("game.exe", "e")]);
        let ini = "drive_c/users/steamuser/Documents/game.ini";
        folder(&g.prefix, &[(ini, "[S]\nA=orig\nB=orig\n")]);
        let store = ModStore::new(&t.join("data"), 9).with_game(&g, None);
        let pkg = |dir: &str, version: &str, set: &str| {
            let d = t.join(dir);
            let body = format!(
                "id = \"tweaks\"\nname = \"Tweaks\"\nversion = \"{version}\"\nsave_compatible = true\n[[patch]]\nfile = \"{ini}\"\nsection = \"S\"\nset = {{ {set} }}\n"
            );
            folder(&d, &[("quiver-mod.toml", &body)]);
            d
        };
        let v1 = store.install(&Package::Dir { path: pkg("p1", "1", "A = \"1\", B = \"2\""), name: None }, "cli").unwrap().layer;
        store.create_profile("P", &["tweaks".into()], "cli", None).unwrap();
        let read = || std::fs::read_to_string(g.prefix.join(ini)).unwrap();
        store.plan(Some("P")).unwrap().apply_patches().unwrap();
        assert_eq!(read(), "[S]\nA=1\nB=2\n");
        assert!(store.applied_by("tweaks").unwrap().iter().all(|k| k.mods == [v1.id.clone()]), "recorded under the mod's id");

        // Installing the package's next version adds it (current); B is no longer wanted.
        let c = store.install(&Package::Dir { path: pkg("p2", "2", "A = \"3\", C = \"9\""), name: None }, "cli").unwrap();
        assert!(c.warnings[0].contains("new version 2"), "{:?}", c.warnings);
        let st = store.plan(Some("P")).unwrap().apply_patches().unwrap();
        assert_eq!(read(), "[S]\nA=3\nB=orig\nC=9\n");
        assert!(st.iter().any(|s| s.state == ubi_api::mods::PatchState::Undone && s.detail.contains("B: back to \"orig\"")), "{st:?}");
        let keys = store.applied_by("tweaks").unwrap();
        let a = keys.iter().find(|k| k.key == "A").unwrap();
        assert_eq!((a.original.as_slice(), a.applied.as_deref()), (["orig".to_owned()].as_slice(), Some("3")), "the file's own value is kept");

        // Pinning the profile back to 1: A and B as 1 wanted them, C (not in the file
        // before) removed again.
        store.set_profile("P", &ops(&["mods=tweaks@1"]), "cli", None).unwrap();
        store.plan(Some("P")).unwrap().apply_patches().unwrap();
        assert_eq!(read(), "[S]\nA=1\nB=2\n");
        // Rolling the mod forward for everyone, then undoing: the file's own values.
        store.set_profile("P", &ops(&["mods=tweaks"]), "cli", None).unwrap();
        store.plan(Some("P")).unwrap().apply_patches().unwrap();
        assert_eq!(read(), "[S]\nA=3\nB=orig\nC=9\n");
        store.use_version("tweaks", "1", "cli", None).unwrap();
        store.plan(Some("P")).unwrap().apply_patches().unwrap();
        assert_eq!(read(), "[S]\nA=1\nB=2\n");
        store.undo_patches(Some("tweaks")).unwrap();
        assert_eq!(read(), "[S]\nA=orig\nB=orig\n");
        assert!(store.applied_by("tweaks").unwrap().is_empty());
    }

    #[test]
    fn builtins_update_to_a_new_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let g = game(t, 3539);
        folder(&g.install_dir, &[("videos/UbisoftLogo.bk2", "v")]);
        std::fs::create_dir_all(&g.prefix).unwrap();
        let store = ModStore::new(&t.join("data"), 3539).with_game(&g, None);
        let l = store.install(&Package::Builtin("aco-skip-intros".into()), "cli").unwrap().layer;
        assert!(store.update_builtins(false, "cli").unwrap().is_empty(), "up to date");
        // As if an older Quiver had installed version 0.
        let mut old = l.clone();
        old.manifest.as_mut().unwrap().version = Some("0".into());
        store.write_layer_meta(&old).unwrap();
        store.create_profile("P", &["aco-skip-intros".into()], "cli", None).unwrap();
        assert_eq!(store.update_builtins(true, "cli").unwrap(), ["Skip intro videos 0 -> 1"]);
        assert_eq!(store.mods().unwrap()[0].versions.len(), 1, "dry run");
        let r = store.migrate(false, "cli").unwrap();
        assert_eq!(r.builtin_updates, ["Skip intro videos 0 -> 1"]);
        let rec = store.find_mod("aco-skip-intros").unwrap();
        assert_eq!((rec.versions.len(), rec.label_of(&rec.current).as_str()), (2, "1"));
        assert_eq!(store.plan(Some("P")).unwrap().api.resolved[0].describe(), "Skip intro videos 1 (current)");
        // Rolled back by hand: left alone.
        store.use_version("aco-skip-intros", "0", "cli", None).unwrap();
        let mut older = store.layer("aco-skip-intros@1").unwrap();
        older.manifest.as_mut().unwrap().version = Some("0.5".into());
        store.write_layer_meta(&older).unwrap();
        assert!(store.migrate(false, "cli").unwrap().builtin_updates.is_empty());
    }

    #[test]
    fn labels_names_and_turning_off_live_on_the_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = v3_store(tmp.path());
        // A label makes a one-version mod versioned (revision 1: as it was).
        let c = store.set_mod("DLSS-v3", &ops(&["label=3.7", "notes=frame generation"]), "cli", None).unwrap();
        assert_eq!(c.layer.name, "DLSS-v3");
        let (rec, hist) = store.mod_history_of("lplaao3").unwrap();
        assert_eq!((hist.len(), rec.versions[0].label.as_str(), rec.versions[0].notes.as_deref()), (2, "3.7", Some("frame generation")));
        store.set_mod("DLSS-v3", &ops(&["name=DLSS FG", "enabled=off"]), "cli", None).unwrap();
        let rec = store.find_mod("DLSS FG").unwrap();
        assert!(rec.disabled && rec.id == "lplaao3");
        assert!(!store.layer("lplaao3").unwrap().disabled, "the version itself isn't changed");
        let p = store.plan(Some("DLSS")).unwrap();
        assert!(p.api.warnings.iter().any(|w| w.contains("DLSS FG is turned off")), "{:?}", p.api.warnings);
        assert!(store.set_mod("DLSS FG", &ops(&["name=DLSS"]), "cli", None).unwrap_err().to_string().contains("already a mod named"));
    }
}
