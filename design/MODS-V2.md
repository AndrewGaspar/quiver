# Mods v2: one model for files, settings and config (design notes)

Status: agreed direction (2026-11-16); backend built on `feat/mods-v2` (core, daemon, API,
CLI). The app's UI is still to come. See "As built" at the end and README "Mods v2".

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
  `quiver-mod.json` with the same fields is read too). Types: `ubi_api::mods::ModManifest`.
- **Store:** an installed mod is a layer record with a `manifest` (same folders, same ids,
  same history file; new ids start with `m`). v1 layers are file-only mods as they are,
  with unknown save compatibility (counted as not compatible). `quiver mods <game> set MOD
  save_compatible=on kind=fix` gives one a manifest of its own, recorded in its history.
- **Profiles:** `Profile::layers` is the mod stack and `Profile::settings` the person's
  overrides, so v1 histories read unchanged. New fields: `separate_saves` (`None` = auto)
  and `profile_config` (prefix folders the profile has its own copy of).
- **Saves migration:** a profile that already has a `savegames/<user>/<product>-<id>` folder
  keeps it. The rule checks for the folder even before migration, and `ModStore::migrate`
  (run by every profile launch, and by `quiver mods <game> migrate`) records
  `separate_saves=on` as a revision, so the choice is visible and reversible. Nothing is moved;
  only an explicit `separate_saves=off` makes such a profile share vanilla's saves.
- **Shared saves sync with the cloud** like vanilla's (a profile's `cloud_saves` only
  matters for its own saves).
- **Config patches:** applied by the daemon before the namespace is mounted (never under a
  live overlay), to the file the launch will see: the prefix's, or the profile's copy in
  `profiles/<id>/prefix/<dir>` (copied up first, as overlayfs would). Record:
  `mods/<product>/patches.json` (file's own values, where a deleted key stood, the applied
  value, who wants it). Every launch reconciles the files it sees; a key changed since
  Quiver set it is left alone when undoing. `*` in a path component matches any one name
  (Far Cry 4 keeps `GamerProfile.xml` in a per-account folder).
- **Vanilla with fixes:** fixes with install files mount over the install for vanilla too,
  with `profiles/vanilla/upper` as the writable layer.
- **Built-ins:** `mods/<product>/<id>/quiver-mod.toml`, compiled in by `ubi-core/build.rs`;
  a unit test checks every entry parses, links PCGamingWiki, has an `audited` note and names
  its product.

### Known limits

- A profile without its own copy of a folder shares that folder's files with every other
  launch, so its config patches there are undone by the next launch that doesn't want them.
  Use `profile_config` to give a profile settings of its own.
- A profile's own copy starts as the prefix's file at the time it was copied up, including
  any patch vanilla had applied then; undoing in the copy restores the prefix's pre-Quiver
  value (recorded in the prefix's entry).
- `profile_config` folders are comma-separated, so a folder name with a comma can't be given.
