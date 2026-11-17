# Mods v2: one model for files, settings and config (design notes)

Status: agreed direction (2026-11-16); built (core, daemon, API, CLI, app). Mods are
versioned (decision 7–12, "Mod versions (as built)"). See "As built" at the end and README
"Mods v2" and "Mod versions".

## Problem

Today a *layer* (files over the install) and a *profile's settings overrides* (args, env, DLL
overrides) are separate things, and saves fork whenever a profile has layers. A user thinks of
"a mod" as everything needed to change the game. For example, a "Minimal fixes" mod should
bring its flags and its file changes together.

## Model

**A mod is a package with a manifest**, declaring any of:

- **Install files:** add, replace or delete (whiteouts, opaque folders) under the install root.
  Today's layers become this part only.
- **Launch settings:** args, env, DLL overrides, wrappers, and possibly constraints (e.g.
  needs Proton ≥ X).
- **Config patches:** structured key edits to files the game keeps in its prefix (INI/XML),
  e.g. `GamerProfile.xml: MouseAcceleration=0`. They are re-applied at launch, the original
  values are recorded so they can be undone, and they are queued until the file first exists.
  They are patches, not file replacements, because the game rewrites these files itself.
- **Metadata:** name, source (link), version, known-good game builds (manifest hash), requires
  and conflicts, and whether the mod is **save-compatible**.

**A profile is an ordered stack of mods plus the user's own overrides.**
- Settings resolve as: defaults → game settings → each mod in order → profile overrides.
- Args and wrappers append in order; env and DLL overrides merge, with the later one winning.
- Conflicts are surfaced up front, never resolved silently (e.g. two mods setting
  `dll.d3d11` differently).

**Saves fork per mod, not per profile:** saves fork only when a mod in the stack is not
save-compatible (gameplay or save-format changes). Fixes such as intro skips and mouse
settings don't fork saves. The profile shows which way it goes, and the user can override it.
*Superseded by decision 4 below: saves are orthogonal to profiles.*

## Decisions (user, 2026-11-16)

1. **Fixes on vanilla:** behind an app-level setting, with a prompt on first launch. This is
   part of a broader **first-launch UX** design, which still needs doing:
   - sign-in;
   - where games are installed (the per-game prefixes folder under "Advanced");
   - importing Ubisoft Connect downloads (Lutris, Wine, Connect inside a Steam game);
   - Steam: Ubisoft games bought on Steam start through Quiver (launch options), and shortcuts;
   - controller and hyprpad;
   - "apply recommended fixes to unmodded games?";
   - the theme.
2. **Per-profile prefix config:** yes. A profile can overlay the game's config files in the
   prefix (e.g. `Documents/My Games/...`), so the DLSS profile gets its own graphics settings.
   The UI must show clearly where each profile's settings live and which settings are
   per-profile.
3. **Third-party recipes:** sources like PCGamingWiki, ProtonDB and community lists are
   **audited when translated into Quiver's built-in mods**. Users install built-in mods, not
   raw third-party recipes.

## Decision (user, 2026-11-16, later): saves are orthogonal to profiles

User feedback: the Vanilla and DLSS profiles of AC Origins had different saves folders and
only "met" through the cloud; they should just share the same Default saves. Which saves a
profile uses and which mods it runs are separate questions, though creating a profile should
steer by whether its mods can break saves.

4. **Save stores** are a first-class, per-game concept. Every game has **Default** (Connect's
   own `savegames/<user>/<product>`) and any number of named extra stores. A profile points
   at a store by id; vanilla points at one too (normally Default); several may share one. The
   choice is explicit and recorded, not an auto rule: a new profile starts on Default when
   every mod is save-compatible, else on a new store, and the reason is shown. Changing mods
   later only warns while a save-incompatible stack shares vanilla's store or Default.
5. **One cloud store per game.** Ubisoft keeps one set of cloud slots per game and account,
   so at most one store syncs (a store-level flag, Default by default). Launches with any
   other store keep saves local and say which store syncs. The per-profile `cloud_saves`
   switch is gone.
6. **Moving between stores** is a plain setting; combining two stores is an explicit
   **merge** (per slot, newer mtime wins, the target is backed up first, refused while the game
   runs). Retiring a store keeps its files.

## Decision (user, 2026-11-16, later still): mods are versioned

User request: "mods can just install new versions and have all existing profiles pick up
those updates automatically", without the "DLSS / DLSS-v2 / DLSS-v3" pile of separate mods,
"but track the versions as separate objects, so the user can override and use an older
version".

7. **A mod is a stable identity** (id, name, on/off) with **one or more versions**. Each
   version is its own object: its files folder (never rewritten once added) and manifest
   (deletions, launch settings, config patches, save compatibility, known builds), a free-text
   label unique within the mod (`3.8.10`, `v3`), added-at/by, notes, and flags *retired*
   (kept, hidden from defaults, can't be current) and *pre-release* (added, not current).
8. **Profiles reference mods, not versions, by default.** A stack entry `MOD` tracks the
   mod's *current* version; `MOD@VERSION` pins one. Adding a version makes it current (unless
   `--no-update` or a pre-release), so every tracking profile runs it at its next launch.
   `version use` rolls the current version back or forward for everyone; pinning is per
   profile.
9. **Save compatibility is per version.** A version that may change saves reaching a profile
   that shares vanilla's store or Default warns (when added, in `mods list` and `plan`),
   through the save stores' steering. Saves never move on their own.
10. **Reproducible play history:** launches and sessions record the version each mod resolved
    to ("DLSS v3 (current)").
11. **Built-ins are versioned the same way:** a newer catalog manifest becomes a new current
    version.
12. **Consolidating separate mods is explicit** (`merge-versions INTO FROM…`); nothing is
    merged by guessing from names.

## Distribution

- **Recipes** (no binaries: flags, deletions, config patches) are small text manifests that
  can ship in the repo as built-in mods, each linking its source.
- **Payload mods** (with binaries, e.g. DLSS DLLs) are a manifest plus files, as a folder or
  archive with checksums. Licensing and trust need care; this comes later.
- After a game update, mods pinned to known builds warn if files they replace have changed.

## Migration

- Existing layers become file-only mods.
- Profile overrides stay as profile overrides.
- Game settings history stays as the base.
- Hand-made `.quiver-backup-*` config edits from the PCGamingWiki pass become config-patch
  mods.

## As built (2026-11-16)

- **Format:** `quiver-mod.toml` (TOML, `deny_unknown_fields` so typos fail loudly;
  `quiver-mod.json` with the same fields is read too). Types: `quiver_api::mods::ModManifest`.
- **Store:** an installed mod is a layer record with a `manifest` (same folders, same ids,
  same history file; new ids start with `m`). v1 layers are file-only mods as they are,
  with unknown save compatibility (counted as not compatible). `quiver mods <game> set MOD
  save_compatible=on kind=fix` gives one a manifest of its own, recorded in its history.
- **Profiles:** `Profile::layers` is the mod stack and `Profile::settings` the person's
  overrides, so v1 histories read unchanged. New fields: `profile_config` (prefix folders the
  profile has its own copy of) and `saves` (its save store, see below); `separate_saves` and
  `cloud_saves` are read from old revisions but no longer settable.
- **Saves migration (first pass):** a profile that already had a
  `savegames/<user>/<product>-<id>` folder got `separate_saves=on` recorded. Replaced by save
  stores (below), which migrate that setting again.

### Save stores (as built, 2026-11-16)

- **Types:** `quiver_api::savestores` (`SaveStore`, `SaveStoresRevision`, `SaveStoreOp`,
  `SaveStoresReport`, `MergeReport`, `saves_advice`, `shared_store_warning`); core in
  `quiver_core::savestores`. Requests (appended): `SaveStores`, `SaveStoreEdit`,
  `SaveStoreHistory`, `SavesMerge`; `ModsRequest::ProfileCreate` takes an optional `saves`.
- **Storage:** `games/<product>/saves.jsonl`, whole-state revisions (stores with id, name,
  folder, created, created_by, cloud, retired, note; plus vanilla's store). No file means
  revision 0: Default only, cloud, used by vanilla. Each store's `folder` is fixed when it's
  made (`<product>` for Default, `<product>-<store id>` for new stores, `<product>-<profile id>`
  for a migrated profile folder), so files never move. Who uses a store is read from the
  profiles' own `saves` field, never stored twice.
- **Cloud rule:** at most one live store has `cloud`; moving the flag clears the old one in the
  same revision, and the cloud store can't be retired. `SavesPlan` carries the store and a
  `cloud_note` when the store isn't the cloud one; the launch job and `mods list`/`plan` show it.
  `Saves`/`SavesRestore` work on the cloud store (else Default).
- **Migration:** `ModStore::migrate` (each profile launch, `quiver mods <game> migrate`) gives
  every live profile without `saves` one. Profiles the old rule gave their own folder
  (`separate_saves=on`, an existing folder, or an auto stack with a save-incompatible mod) get a
  store adopting that folder (named after the profile, not synced; an existing store with that
  folder is reused, and unretired if needed); the rest get Default (an unused folder left by `separate_saves=off` becomes a retired
  store, so it can be merged). Each is a stores revision
  plus a profile revision clearing `separate_saves`/`cloud_saves`, so restoring the earlier
  profile revision undoes it (and the next migration finds the same store). Until migrated, a
  profile's plan follows the old rule. A profile that had `cloud_saves=on` with its own folder
  gets a note: its saves and vanilla's took turns in the one cloud set; merge, then point it at
  Default.
- **Merge:** `merge_dirs` per account folder that has the source: union of slots; source-only
  copied; both: equal bytes kept, source strictly newer mtime replaces (mtime kept on copy),
  else the target's is kept. The target is backed up with the save-backup machinery (reason
  "before merging save store A into B") before anything is replaced; slots taken into the cloud
  store get `.upload` markers. The daemon refuses while the game runs (its launches or any play
  session) and asks agents for confirmation.
- **Named backups:** `quiver saves` and the Saves tab also list backup folders named by hand
  (`save-backups/<product>-<name>`, e.g. `3539-ptkzxpv-fresh-playthrough-…`), dated by the
  folder's mtime, restorable like the others.
- **Not done:** Steam launches (`steam-run`) still use Default whatever vanilla's store is;
  restoring a backup into a store other than the cloud one needs a merge from a store instead.
- **Config patches:** applied by the daemon before the namespace is mounted (never under a
  live overlay), to the file the launch will see: the prefix's, or the profile's copy in
  `profiles/<id>/prefix/<dir>` (copied up first, as overlayfs would). Record:
  `mods/<product>/patches.json` (file's own values, where a deleted key stood, the applied
  value, who wants it). Every launch reconciles the files it sees; a key changed since
  Quiver set it is left alone when undoing. `*` in a path component matches any one name
  (Far Cry 4 keeps `GamerProfile.xml` in a per-account folder).
- **Vanilla with fixes:** fixes with install files mount over the install for vanilla too,
  with `profiles/vanilla/upper` as the writable layer.
- **Built-ins:** `mods/<product>/<id>/quiver-mod.toml`, compiled in by `quiver-core/build.rs`;
  a unit test checks every entry parses, links PCGamingWiki, has an `audited` note and names
  its product.
- **Default profile:** the game setting `profile` (`GameSettings::profile`, a profile id; the
  daemon resolves names), so changes are settings revisions. `Request::Play` with
  `profile: None` (what old clients send) runs it; `Some("vanilla")` forces vanilla. A
  default that was deleted runs vanilla with a warning (`ModStore::launch_profile`); the id
  is kept, so restoring the profile brings it back. `ModsState::default_profile` reports it.
  CLI: `quiver mods <game> default PROFILE|vanilla`; app: the profile chip next to Play.

### Mod versions (as built, 2026-11-16)

- **Types** (`quiver_api::mods`): `ModRecord` (id, name, current, versions, disabled),
  `ModVersion` (id, label, added, added_by, notes, retired, prerelease), `ModRevision`
  (whole-record history entries; `deleted` for a mod merged away or removed),
  `ResolvedMod` (an entry resolved: mod, version, label, pinned, current; `describe()`),
  `mods_of`/`resolve_entry`/`split_entry`/`stack_entry`; `ModsState.mods` (every mod, one-version
  ones included), `ProfileInfo.resolved`, `LaunchPlan.resolved`, `ModDetail.record/used_as`,
  `PlaySession.mods`, `MigrationReport.builtin_updates`; replies `VersionChange`,
  `VersionMerge`. Requests (appended): `VersionAdd`, `VersionUse`, `VersionRetire`,
  `ModHistory`, `MergeVersions`. Core: `quiver_core::modversions`.
- **Storage:** a version *is* a layer: `mods/<product>/layers/<version id>/` (files),
  `<version id>.json` (its `Layer`: metadata and manifest), `games/<product>/layers/<version
  id>.jsonl` (deletions history). A mod's record is `games/<product>/mods/<mod id>.jsonl`, one
  whole record per revision, the last current. New version ids start with `v`.
- **Ids and old data:** a layer that was never versioned is a one-version mod whose id is the
  layer's id (`ModRecord::implicit`; label: the manifest's `version`, else `v1`), so v1/v2
  stores, profile entries and histories read unchanged and nothing is written until a mod
  gets a second version, a label or a merge; then revision 1 records the mod as it was and
  revision 2 the change. The mod id is the first layer's id forever, so it stays valid
  wherever it was used. A profile entry that is a bare *version* id (old revisions, or one
  that named `DLSS-v2` before a merge) resolves pinned to that version: restoring an old
  profile revision runs exactly what it ran.
- **Adding a version** (`add_version`): a package folder brings its own manifest and files;
  a plain folder keeps the current version's manifest (save compatibility, kind, launch
  settings, patches, requires/conflicts, without `version`, `builds`, `audited`) and its
  deletions, plus `--delete`; `--save-compatible` overrides. Labels: given, else the
  package's `version`, else `vN`. Installing a package whose manifest id is installed adds a
  version (the same manifest again is refused).
- **Resolution:** the stack resolves each entry to a version (a turned-off mod is skipped as
  before; a pin to a retired version runs with a note); the version's layer, named after its
  mod ("DLSS v3"), is what mounts, conflicts, save advice and warnings see. Config patches are
  recorded under the **mod's** id, so switching versions reconciles like any change of what
  the mod wants: keys the new version sets get its values (the file's own value stays
  recorded), keys it no longer sets go back, keys it adds are recorded fresh. `undo-patches
  MOD` and `applied_by` cover the mod's id and its versions' ids (records from before).
- **Mod-level vs version-level settings:** `name`, `enabled` are the mod's (in its record once
  versioned); `save_compatible`, `kind` are a version's manifest (current, or `MOD@VERSION`);
  `label`, `notes` are a version's entry in the record.
- **Retire:** not the current version; pins keep running it (with a note); `unretire`
  brings it back; `version use` refuses a retired version.
- **Merge** (`merge_versions`): every version of each FROM mod joins INTO (it keeps its id and
  name), ordered by added time; labels: `--labels` in order, else a version keeps a label it
  had (versioned mod, or a manifest `version`) and the rest are `vN` by position; the newest
  live version becomes current. Each live profile entry that resolved to one of these mods is
  rewritten to INTO, tracking current if it ran the newest version and pinned to what it ran
  otherwise, so no launch changes; a profile stacking two of them is refused. Revisions: the
  mod's (revision 1 as it was, then the merge), a `deleted` revision for a versioned FROM, a
  note in each moved version's layer history, a profile revision per rewritten stack. Agents
  need confirmation; `--dry-run` shows everything first.
- **Built-ins:** `install ID` with a newer catalog manifest adds it as a new current version
  (or makes the version that has it current again). The daemon does the same at launch
  (`migrate`, and for vanilla launches) unless someone made an older version current by hand.
- **Uninstall** removes every version (refused while any version is stacked).
- **App:** the Mods tab stacks mods (version chips: track current or pin), the mod store shows
  each mod's current version and its history (Make current, Retire / Bring back, who pins
  what).

### Known limits

- A profile without its own copy of a folder shares that folder's files with every other
  launch, so its config patches there are undone by the next launch that doesn't want them.
  Use `profile_config` to give a profile settings of its own.
- A profile's own copy starts as the prefix's file at the time it was copied up, including
  any patch vanilla had applied then; undoing in the copy restores the prefix's pre-Quiver
  value (recorded in the prefix's entry).
- `profile_config` folders are comma-separated, so a folder name with a comma can't be given.
- A single version can't be deleted (retire keeps its files); uninstalling removes them all.
- `layer delete-files` still edits a version's deletions in place (recorded in its history);
  a new version is the way to change what a mod ships.
- Mod revisions can be listed (`version history`) but not restored as a whole; `version use`,
  `unretire` and profile restores undo the usual changes.
