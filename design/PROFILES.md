# Profiles: a game's named setups (mods and game settings)

Status: decided and built (2026-11-19). Extends MODS-V2 (which defines the stack, overrides,
`profile_config` and save stores), MOD-CONFIG (Options values per profile) and TWEAKS
(profile scope). Nothing here changes how a launch resolves a stack.

## Problem

The app presented profiles as *mod profiles*: "New mod profile", "Layers, bottom to top",
"No layers yet: … or create an empty profile". A person who wants "HDR on, high settings"
and "Performance" for a game with no mods at all had to make two "empty mod profiles", and
those still shared the game's own settings files (Alan Wake Remastered's `resolution.xml`),
so switching profile didn't switch the settings.

## Model

**A profile is a named setup of a game**: its mods and its game settings. Mods are a library
you put into profiles. A profile with no mods is a normal profile.

| Part | Where it lives | Existed? |
|---|---|---|
| Mods (stack, bottom to top; tracking or pinned versions) | `Profile::layers` | MODS-V2 |
| Own copy of the game's settings files | `Profile::profile_config` (prefix folders) → `mods/<dir>/profiles/<id>/prefix/<folder>` | MODS-V2 decision 2 |
| Options values (mods', the game's) | `games/<dir>/options/<id>.jsonl` | MOD-CONFIG |
| Tweaks at profile scope (frame rate, DLSS, HDR …) | the same file, surface `quiver/tweaks` | TWEAKS §3.5 |
| Launch overrides (Proton, env, DLLs, args, wrappers) | `Profile::settings` | v1 |
| Save store (Ubisoft) / the real saves (Epic, Steam) | `Profile::saves` | MODS-V2 decision 4 |

**Vanilla is the base profile**: the unmodded game with the prefix's own settings files, the
game-scope Tweaks and its own Options file (`vanilla.jsonl`). It isn't stored as a profile
and can't be deleted.

## Decisions

1. **Profiles and mods are separate things in every surface.** The game page's tab is
   *Profiles*; it has a **Profiles** section (a card per profile: mods, settings, saves; the
   selected one's detail) and a **Mods** section (the library: each mod, its versions, add a
   mod). The dialog is "New profile". The CLI gains `quiver profiles GAME …`; `quiver mods
   GAME profile …` keeps working (same requests). Agents and the skill say "profile".
2. **A new profile gets its own settings by default.** When Quiver knows the game's settings
   folders (below), a new profile's `profile_config` lists them and its copy starts as a
   **snapshot** of the source's files at creation time (vanilla: the prefix's; a profile: its
   own copy over the prefix's). A snapshot, not an overlay that tracks the source: "HDR on"
   must not change when someone turns HDR off in vanilla. Opt out per profile ("Share
   Vanilla's settings": `--shared-settings`, `own_settings: false`); opt in later for an older
   profile (`quiver profiles GAME settings PROFILE own`, which snapshots then).
3. **Create from the current profile by default; blank is a choice.** "From" copies the
   source's mods (entries as they are: tracking stays tracking, pins stay pins), launch
   overrides, settings snapshot, Options values (with its profile-scope Tweaks, one file) and
   save store (the same store, so the copy plays the same saves; pick another in the dialog).
   "Blank" starts from vanilla: no mods, no overrides, vanilla's settings files, no Options
   values, and the store the advice picks. The new profile's first revision and its Options
   revision say what they were copied from.
4. **Saves stay orthogonal.** A settings folder can hold saves (AWR keeps
   `savegames/` inside `Documents/Remedy/AlanWakeRemastered`). A profile's own copy of a
   folder never forks the saves in it: save subfolders are **kept shared**, bind-mounted from
   the prefix over the profile's overlay inside the launch namespace (`OverlaySpec::shared`),
   and never snapshotted. Save stores remain the only way to give a profile saves of its own.
5. **Which folders hold a game's settings** (`ModStore::settings_dirs`, shown in the app and
   `quiver profiles GAME settings`), first match wins per folder:
   - *catalog*: `quiver_core::gamesettings::KNOWN`, audited entries per game (folder, and save
     subfolders to keep shared);
   - *mods*: the folder of every prefix file an installed mod's config patch names (above
     the first wildcard component), settings files by definition;
   - *inferred*: Quiver gives each game its own prefix (Ubisoft, Epic) and Steam's Proton
     prefixes are per app too, so a folder in the prefix's user folders (`Documents/*`,
     `Documents/My Games/*`, `AppData/{Local,LocalLow,Roaming}/*`, one level deeper for a
     publisher folder) that isn't Wine's or a runtime's (`Microsoft`, `Temp`, `dxvk`, …)
     belongs to the game. Inferred folders are listed as such.
   Save subfolders: the catalog's, plus any subfolder (two levels down) whose name contains
   `save` (any case). A folder inside another listed one is dropped (the outer one covers it).
   A game that hasn't run yet has no inferred folders; a new profile then shares settings and
   says so ("the game hasn't written its settings yet"), and `settings PROFILE own` fixes it
   after a first run.
6. **What a snapshot copies**: regular files up to 8 MiB, skipping save subfolders, crash
   dumps (`*.dmp`), logs and cache folders (`cache`, `*cache`, `shadercache`, `crashes`,
   `report`). Everything else is copied lazily by the overlay when the game writes it.
7. **Migration: none needed on disk, nothing changes for existing profiles.** Their histories
   read as they are; a profile without `profile_config` keeps sharing the prefix's settings
   (the app says "Shares Vanilla's game settings" with *Give it its own*). The only behaviour
   change for an existing profile with `profile_config` is decision 4: save subfolders inside
   its own folders are now shared (none of this machine's profiles had `profile_config`).
   `ModsRequest::ProfileCreate` gains `from` and `own_settings` (appended, defaulted), so older
   clients create blank profiles, now with own settings where folders are known.

## Surfaces (as built)

- **API**: `ModsRequest::ProfileCreate { …, from, own_settings }`; `ModsRequest::ProfileSettings
  { game, profile, own }` (snapshot into a profile, or go back to sharing: the copy is kept
  on disk); `ModsState::settings_dirs` (`SettingsDir { dir, source, shared, exists }`);
  `ProfileInfo::{option_values, tweak_values}` (counts, for the cards).
- **CLI**: `quiver profiles GAME` (list), `create NAME [--from P|--blank] [--mods a,b]
  [--saves S] [--shared-settings]`, `show P`, `set P k=v…`, `settings [P [own|shared]]`,
  `default P`, `delete P`, `restore P REV`, `history P`.
- **App**: the Profiles tab (cards with "N mods · own settings · Default saves"), the selected
  profile's Mods / Game settings (folders and files, launch overrides, Options and Tweaks
  counts, links) / Saves; the Mods section (library). New profile dialog: name, *Start from*
  (current profile, other profiles, Blank), mods (prefilled from the source, "none: a
  settings-only profile"), game settings (own copy of which folders, or share), save store.
- **Sight**: the Session section shows the profile's settings ("own settings" or "Vanilla's
  settings") next to its mods; the Mods section's empty state says the profile has no mods
  rather than being an error.

## Left open

- Two tabs (Profiles, Mods) instead of one tab with two sections: one tab keeps controller
  focus and deep links as they are; revisit if the library grows.
- Steam's `ufs` save roots and Epic's resolved `CloudSaveFolder` aren't used for "keep
  shared" yet (the name rule covers the games seen); Epic's needs the account id.
- Deleting a profile keeps its settings copy (as it keeps everything); there is no "clean up
  deleted profiles' files" yet.
- Comparing two profiles' settings files (a diff view) and copying one file between profiles.
- Whether game-scope Tweaks should become vanilla's (today vanilla launches use game scope).
- Options surfaces' prefix files aren't a settings-folder source yet (the mods' config patches
  name the same files for every catalog surface so far).
- Controller focus doesn't reach the dialog's new chips and checkboxes or the Game settings
  card's buttons (pointer only); the dialog's own buttons still do.
- The demo has no Epic or Steam game with profiles, so the "real saves" dialog text for Alan
  Wake Remastered is covered by tests, not a screenshot.
