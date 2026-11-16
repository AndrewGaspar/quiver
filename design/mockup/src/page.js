/* ---------- review page ---------- */
const P = 'p', L = 'l', F = 'f', D = 'd';
const SCREENS = [
  { id: 'library', grp: 'Library', label: 'Library home', title: 'Library home', sub: 'Art-forward grid with the last-played game up front.', render: scrLibrary,
    notes: [
      '<b>Hero</b> is the last-played game, showing the profile and Proton build it will launch with, so Play never surprises.',
      '<b>Needs attention</b> collects what is waiting on you: updates, installs, save conflicts, agent approvals. It disappears when empty.',
      '<b>Pinned profiles are tiles of their own.</b> “AC Odyssey · Visual overhaul” launches straight into that mod profile, has its own art, and has its own .desktop entry and Steam shortcut.',
      '<b>Grid</b> uses Ubisoft’s 240×280 thumb art by default; any game or pinned profile can use your own image instead. Badges encode state: update, conflict, queued, mod layers, favorite, and which account owns a game when it isn’t your default one.',
      '<b>Collections</b>: smart ones (Installed, Favorites, Modded, Pinned profiles), franchise ones derived from <code>sort_string</code> in product configs, and your own. An account filter narrows the merged library to one account.',
      '<b>Keyboard and controller</b>: arrows or D-pad move, Enter or A opens, P or A plays, / filters, Ctrl K or Start opens the command palette.',
    ],
    src: [['Owned games, names, art', P, 'demux ownership + product config (thumb, background, logo), per account'], ['Install state, size, update available', P, 'manifests, compared with local install records'], ['Play time', P, 'Ubisoft total once wired; local sessions until then'], ['Franchise collections', P, 'sort_string, e.g. “Assassin’s Creed 16”'], ['Custom art', L, 'image files you pick'], ['Favorites, collections, pinned profiles', L, ''], ['Agent requests', L, 'quiverd approval queue']] },
  { id: 'game', grp: 'Library', label: 'Game detail', title: 'Game detail', sub: 'List on the left, the game on the right, the way Steam’s library works.', render: () => scrGame(false),
    notes: [
      '<b>List</b>: recent and installed first, then not installed. Pinned profiles sit indented under their game. The right edge carries status (progress, update, conflict, mods).',
      '<b>Play is a split button.</b> The arrow picks a mod profile or one-off options like “launch once with GE-Proton10-26”.',
      '<b>Add to Steam</b> is always one click away, for the game and for each pinned profile. It is also offered when an install finishes.',
      '<b>Add-ons</b> lists everything associated with the game. Ones you don’t own get a “View in Ubisoft Store” link, which opens a store search for the add-on’s name; there is no store inside Quiver.',
      '<b>Launch account</b>: a game owned by more than one account launches with the default account unless you override it here. The launch account also decides whose cloud saves the game uses.',
      '<b>Art</b> menu on the hero switches between Ubisoft art and your own image file, per slot (tile, hero, logo).',
    ],
    src: [['Hero art, executable, Denuvo, cloud saves, “Animus Control Panel”, links', P, 'product config'], ['Version, manifest, size', P, 'manifest + local install record'], ['Add-ons owned, add-on names', P, 'ownership associations; names from each add-on’s own product config (not fetched in this dump)'], ['Store links', D, 'Ubisoft web store'], ['Achievements, player stats', F, 'UbiServices; the stat ids are already in the config'], ['Sessions, crash codes', L, 'quiverd'], ['Steam shortcut and .desktop state', L, 'shortcuts.vdf and ~/.local/share/applications']] },
  { id: 'launch', grp: 'Library', label: 'Launch options', title: 'Launch options and history', sub: 'Every change is a revision you can compare, test and restore.', render: scrLaunch,
    notes: [
      '<b>Three scopes</b>: this game, the mod profile, global defaults. Each value shows where it is inherited from. Changed values get a dot.',
      '<b>Edits are staged.</b> Save makes a new revision with an optional message. <b>Test launch</b> runs the staged config once without saving it.',
      '<b>History</b> shows who made each revision (app, CLI, agent, system), why (<code>--reason</code>) and how the next launches went. A clean session of 10 minutes or more tags the revision “known good”.',
      '<b>Restore never rewrites history.</b> Restoring r13 creates r15 with r13’s content, and that restore can itself be undone.',
      '<b>Proton changes snapshot the prefix first</b>, automatically on btrfs and xfs where a reflink copy is nearly free, and opt-in on ext4.',
      '<b>Resolved command</b> is exactly what quiverd will run. Copy it as a shell line or as <code>quiver config set</code>.',
    ],
    src: [['Executable, Ubisoft arguments, working directory', P, 'product config'], ['Proton builds', L, 'Steam compatibilitytools.d + Quiver-managed builds'], ['Revisions, outcomes, snapshots', L, 'quiverd config store'], ['Kernel support (NTSync), wrappers found', L, 'probed at startup']] },
  { id: 'mods', grp: 'Library', label: 'Mods', title: 'Mods as overlay layers', sub: 'Run vanilla or any profile. The base install is never touched.', render: scrMods,
    notes: [
      '<b>Profiles are named stacks of layers.</b> Vanilla always exists and runs the base install directly, with no mount.',
      '<b>The profile menu</b> (open here) pins the profile to the library, creates its .desktop entry and adds it to Steam, each with its current status. Exports use the profile’s stable id, so renaming never breaks them.',
      '<b>Restarting Steam needs your confirmation every time.</b> Steam only shows new shortcuts after a restart, so Quiver asks “Restart Steam now to show the new shortcut?” (shown here). Not now keeps the shortcut staged until Steam restarts on its own. Quiver never restarts Steam silently, as a side effect, or for an agent without your approval.',
      '<b>Deleting a profile</b> asks whether to remove its pin, .desktop entry, Steam shortcut and saves as well.',
      '<b>At launch</b> quiverd mounts base (read-only) + layers + a per-profile writable layer with unprivileged overlayfs in a user namespace, calling mount() directly so the game still runs as your uid. Verified on this machine.',
      '<b>Modded profiles get their own saves.</b> Cloud sync for them is off by default and can be turned on per profile.',
      '<b>After a game update</b>, layers built for an older version get a warning.',
    ],
    src: [['Base version, owned add-ons', P, 'manifest and ownership'], ['Layers, profiles, conflicts, writable layer, exports', L, 'quiverd mod store'], ['Mod sources such as Nexus', F, 'not planned for the first version']] },
  { id: 'saves', grp: 'Library', label: 'Saves & progress', title: 'Saves and progress', sub: 'Cloud slots, local backups and what the platform knows about your progress.', render: scrSaves,
    notes: [
      '<b>Play time</b>: Ubisoft’s total is the headline once it is wired. Local sessions stay underneath as the detail (weekly chart, per-session outcomes).',
      '<b>Slots</b> are Ubisoft cloud-save slots: number, Ubisoft’s save name (timestamp parsed for display), size, local and cloud times.',
      '<b>Status per slot</b>: in sync, cloud only, local only, conflict. Resolution is explicit. Quiver never overwrites silently.',
      '<b>Every pull and push is preceded by a backup</b>, which is how ubi-core already works. Backups are listed and restorable.',
      '<b>Modded profiles</b> keep separate saves; for a game with mod profiles, a profile switcher appears at the top of this tab.',
    ],
    src: [['Save slots', P, 'cloudsave_service V2'], ['Backups', L, '~/.local/share/ubisoft-linux'], ['Play time total', F, 'Ubisoft play-session API'], ['Weekly chart, sessions', L, 'quiverd'], ['Achievements', F, 'UbiServices'], ['Player stats', F, 'stat ids from config, values from UbiServices']] },
  { id: 'queue', grp: 'System', label: 'Downloads', title: 'Downloads', sub: 'One active job, a queue you can reorder, and where the space goes.', render: scrDownloads,
    notes: [
      '<b>Every job shows who started it</b>: you in the app, you in the CLI, an agent, or the auto-updater.',
      '<b>When it finishes</b>: add to Steam and create a .desktop entry are offered at install time, both on by default. Steam lists a new shortcut only after it restarts; if Steam is running, Quiver asks before restarting it and otherwise leaves the shortcut staged.',
      '<b>Phases mirror the installer</b>: manifest, slices, verify, then register, create the prefix and write shortcuts.',
      '<b>Throughput</b> plots network against disk writes. Slice installs are often disk-bound, and this shows it.',
      '<b>Rules</b>: speed limit, pause while playing, an update window, snapshot before updating (automatic on btrfs/xfs).',
    ],
    src: [['Progress, speed, phases', L, 'quiverd job events'], ['Sizes', P, 'manifests'], ['Disk usage', L, 'statvfs + install records'], ['Steam shortcuts', L, 'shortcuts.vdf']] },
  { id: 'prefs', grp: 'System', label: 'Settings', title: 'Settings', sub: 'Accounts, appearance, Proton builds, storage, and what scripts and agents may do.', render: () => scrSettings(SETSEC),
    notes: [
      '<b>Every setting is a <code>quiver config</code> key.</b> Hovering a row shows the key and a copy-as-CLI button.',
      '<b>Accounts</b>: several Ubisoft accounts, one merged library. Each account keeps its own single live session, because Ubisoft allows only one per account. Games owned by more than one account launch with the default account, with a per-game override that also picks whose cloud saves are used.',
      '<b>Appearance</b>: follow Omarchy (default), custom art, the Steam user that gets shortcuts (asked once when the machine has several), controller input and the markers on launched games.',
      '<b>Compatibility</b>: Proton builds with usage counts, the newest-GE-Proton policy, and prefix snapshots (automatic on btrfs/xfs, opt-in on ext4).',
      '<b>Automation and CLI</b>: daemon status, agent tokens, the MCP server, and the confirmed approval policy.',
      '<b>About</b> carries the line “a third-party launcher for your Ubisoft library, not affiliated with or endorsed by Ubisoft”, also shown at the foot of this sidebar.',
    ],
    src: [['Settings', L, 'quiverd config store'], ['Accounts, sessions', P, 'UbiServices + demux, one session per account'], ['Theme', L, 'Omarchy colors.toml'], ['GE-Proton releases', D, 'GitHub releases API']] },
  { id: 'theme', grp: 'System', label: 'Omarchy theme', title: 'Omarchy theme', sub: 'The same screen under two real Omarchy palettes, from the same mapping.', render: scrTheme,
    notes: [
      '<b>Source</b>: <code>~/.local/state/omarchy/current/theme/colors.toml</code>. Quiver watches <code>theme.generation</code> in the same directory and restyles on every change, with no restart.',
      '<b>Mapping</b>: backgrounds become the ink steps, foreground and muted become text, accent drives Play, focus and progress, selection becomes selected rows, and the ANSI colors give the status colors. Missing steps are derived by mixing.',
      '<b>Contrast checks</b>: text, status and actor colors are nudged until they reach 4.5:1 (3:1 for status marks). Actor colors never follow the theme.',
      '<b>Art keeps a dark scrim</b> with light text in every theme, including light ones.',
      '<b>Try it everywhere</b>: the “App theme” buttons above the canvas apply any of these palettes to every screen.',
    ],
    src: [['Palette', L, 'Omarchy theme files'], ['Fallback palette', L, 'built into Quiver']] },
  { id: 'signin', grp: 'System', label: 'Sign in', title: 'Sign in with two-step verification', sub: 'Shown here as one account’s session expiring while the other stays signed in.', render: scrSignin,
    notes: [
      '<b>Three cases</b>: first run, adding another account, and a session that ended (shown here). When another device signs in with the same account, Quiver shows a notification with a “Sign in again” action and never reconnects on its own.',
      '<b>Six digit boxes</b> accept paste and auto-advance. Email code and backup code are alternatives Ubisoft supports.',
      '<b>Login runs in the daemon.</b> The session ticket goes to the system keyring. <code>quiver login [--account NAME]</code> is the same flow in a terminal.',
      '<b>This mock-up has no real inputs</b>: it is a picture of the screen, not a form.',
    ],
    src: [['Sign-in, 2FA, remember device', P, 'UbiServices sessions'], ['Session storage', L, 'Secret Service keyring']] },
  { id: 'activity', grp: 'System', label: 'Activity', title: 'Activity and approvals', sub: 'Every change from the app, the CLI, agents and the system in one log.', render: scrActivity,
    notes: [
      '<b>Append-only log</b> of every state change in quiverd, with the actor: app, CLI, an agent token, or the system.',
      '<b>Agents can pass <code>--reason</code></b>, and it is shown next to the change.',
      '<b>Each entry has its diff and the exact <code>quiver</code> command</b>, so anything done in the app can be repeated in a script.',
      '<b>Approval policy (confirmed)</b>: agents may change launch options, install and launch freely; restoring saves, uninstalling and deleting wait for you here; account changes are denied.',
    ],
    src: [['Events, actors, diffs, approvals', L, 'quiverd event log (SQLite)']] },
  { id: 'pad', grp: 'Flows', label: 'Controller', title: 'Controller navigation', sub: 'The game page after the last input came from a gamepad: focus ring and button hints.', render: () => scrGame(true),
    notes: [
      '<b>Spatial focus</b>: D-pad and left stick move focus to the nearest control in that direction. The focus ring is a theme token (<code>focus</code>, 2 px plus a soft halo) and is drawn only on the focused control.',
      '<b>Buttons</b>: A selects or plays, B goes back, X opens play options, Y opens the menu or details, LB/RB switch Library, Downloads and Activity, LT/RT switch tabs, Start opens the command palette.',
      '<b>Hints appear when the last input was a gamepad</b> and disappear on the next mouse or keyboard input.',
      '<b>The Guide button is never used.</b> hyprpad owns guide chords and keeps Steam Input working. hyprpad forwards the pad to Quiver whenever Quiver has focus, and Quiver reads it as a gamepad.',
      '<b>Games Quiver launches are marked</b> with the Hyprland tag <code>quiver-game</code> (plus <code>quiver-game:&lt;id&gt;</code>), <code>QUIVER_*</code> environment variables, a systemd scope per session and a socket query, so hyprpad needs one rule for all of them.',
      '<b>No separate big-picture mode.</b> The normal app is controller-friendly, and every game and pinned profile can be added to Steam for Steam’s own big picture.',
    ],
    src: [['Gamepad input', L, 'forwarded by hyprpad while Quiver has focus; read via SDL game controller mapping'], ['Desktop chords', D, 'hyprpad (~/code/hyprsc)'], ['Launched-game markers', L, 'Hyprland tag, environment, systemd scope, quiverd socket']] },
  { id: 'import', grp: 'Flows', label: 'Import', title: 'Import existing installs', sub: 'Adopt games from Lutris, Steam or a folder without re-downloading.', render: scrImport,
    notes: [
      '<b>Scans</b> Lutris game configs and prefixes, Steam compatdata for a Ubisoft Connect shortcut, and folders you point at.',
      '<b>Matching</b>: executable names from product configs identify the product; the manifest decides between matches, needs update and verify.',
      '<b>Per game</b>: use in place, move into the library, or copy. Fresh prefix recommended. Old save files go into backups and are compared with cloud before the first launch.',
    ],
    src: [['Product match, version check', P, 'configs + manifests'], ['Found installs, prefixes, saves', L, 'filesystem scan']] },
  { id: 'playing', grp: 'Flows', label: 'Now playing', title: 'Now playing', sub: 'The window steps aside while you play.', render: scrNowPlaying,
    notes: [
      '<b>Compact card</b> while a game runs: elapsed time, profile, revision, and the health of the bridge, the Denuvo token and the ticket refresh.',
      '<b>Stop game</b> ends the process tree in the prefix after a confirm.',
      '<b>Notifications</b> report the cloud pull before launch and the push after exit.',
      '<b>The top-left overlay is MangoHud</b>, enabled in launch options. It is not drawn by Quiver.',
    ],
    src: [['Session state', L, 'quiverd + ubi-bridge IPC'], ['Ticket refresh, Denuvo token', P, 'UbiServices, via the broker']] },
  { id: 'palette', grp: 'Flows', label: 'Command palette', title: 'Command palette', sub: 'Ctrl K or Start. Every command is a daemon call with a CLI twin.', render: scrPalette,
    notes: [
      '<b>Ctrl K or Start anywhere.</b> Fuzzy search over games, actions and settings, scoped to the current game first.',
      '<b>Every entry maps one-to-one to a daemon call</b> and shows its CLI form. Ctrl C copies it.',
      '<b>One command registry</b> drives the palette, <code>quiver --help</code> and the MCP tool list, so the three cannot drift apart.',
    ],
    src: [['Commands', L, 'quiverd command registry']] },
];
let SETSEC = 'Compatibility';
const SRC_LABEL = { p: 'Platform', l: 'Local', f: 'Not yet', d: 'Third party' };

function renderSwitcher(cur) {
  const groups = [...new Set(SCREENS.map((s) => s.grp))];
  document.getElementById('switcher').innerHTML = groups.map((g) => `<div class="grp"><span class="grp-label">${g}</span>${SCREENS.filter((s) => s.grp === g).map((s) => `<button type="button" data-go="${s.id}" aria-current="${s.id === cur}">${s.label}</button>`).join('')}</div>`).join('');
}

function fit() {
  const st = document.getElementById('stage');
  if (!st) return;
  const w = st.clientWidth, s = Math.min(1, w / 1440);
  st.style.height = `${900 * s}px`;
  document.getElementById('stage-in').style.transform = `scale(${s})`;
}

function showScreen(id) {
  const i = Math.max(0, SCREENS.findIndex((s) => s.id === id));
  const s = SCREENS[i];
  renderSwitcher(s.id);
  const extra = s.id === 'prefs' ? `<div class="seg" style="margin-left:8px">${['Accounts', 'Appearance', 'Compatibility', 'Automation and CLI', 'Library and storage'].map((n) => `<span style="cursor:pointer" data-sec="${n}" class="${n === SETSEC ? 'on' : ''}">${n}</span>`).join('')}</div>` : '';
  document.getElementById('st-title').innerHTML = `<h2>${s.title}</h2><p>${s.sub}</p>${extra}<div class="nav"><button type="button" aria-label="Previous screen" data-go="${SCREENS[(i + SCREENS.length - 1) % SCREENS.length].id}">‹</button><button type="button" aria-label="Next screen" data-go="${SCREENS[(i + 1) % SCREENS.length].id}">›</button></div>`;
  document.getElementById('stage-in').innerHTML = s.render();
  applyTheme();
  document.getElementById('notes').innerHTML = `<div class="note-card"><h3>What this screen does</h3><ul>${s.notes.map((n) => `<li>${n}</li>`).join('')}</ul></div>
    <div class="note-card"><h3>Where the data comes from</h3>${s.src.map(([what, k, det]) => `<div class="src-row"><div>${what}${det ? `<small>${det}</small>` : ''}</div><span class="src ${k}">${SRC_LABEL[k]}</span></div>`).join('')}</div>`;
  try { lucide.createIcons(); } catch (e) { /* icons are decorative */ }
  fit();
}

function setTab(t) {
  document.querySelectorAll('.pg-tabs button').forEach((b) => b.setAttribute('aria-selected', String(b.dataset.tab === t)));
  document.getElementById('view-screens').hidden = t !== 'screens';
  document.getElementById('view-proposal').hidden = t !== 'proposal';
  if (t === 'screens') fit();
}

function route() {
  const h = (location.hash || '').slice(1);
  if (h === 'proposal' || h.startsWith('p-')) { setTab('proposal'); if (h.startsWith('p-')) { const el = document.getElementById(h); if (el) el.scrollIntoView(); } return; }
  setTab('screens');
  showScreen(h || 'library');
}

function renderProposal() {
  const box = document.getElementById('prose');
  try {
    box.innerHTML = marked.parse(PROPOSAL_MD);
  } catch (e) {
    box.textContent = PROPOSAL_MD;
    return;
  }
  box.querySelectorAll('table').forEach((t) => { const w = document.createElement('div'); w.className = 'ptbl'; t.parentNode.insertBefore(w, t); w.appendChild(t); });
  box.querySelectorAll('td').forEach((td) => {
    const m = { Platform: 'p', Local: 'l', 'Not yet': 'f', 'Third party': 'd' }[td.textContent.trim()];
    if (m) td.innerHTML = `<span class="src ${m}">${td.textContent.trim()}</span>`;
  });
  box.querySelectorAll('a[href^="#"]').forEach((a) => { a.setAttribute('href', '#p-' + a.getAttribute('href').slice(1)); });
  const toc = document.getElementById('toc');
  const slug = (s) => 'p-' + s.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
  toc.innerHTML = '';
  box.querySelectorAll('h2, h3').forEach((h) => {
    h.id = slug(h.textContent);
    const a = document.createElement('a');
    a.href = '#' + h.id; a.textContent = h.textContent; a.className = h.tagName.toLowerCase();
    if (h.tagName === 'H2' || /^(Color|Omarchy|Type|Spacing|Components|Pinning|Stable ids|Marking|What agents)/.test(h.textContent)) toc.appendChild(a);
  });
  const colorH = [...box.querySelectorAll('h3')].find((h) => /^Color/.test(h.textContent));
  if (colorH) {
    const cs = getComputedStyle(document.documentElement);
    const names = ['ink-0', 'ink-1', 'ink-2', 'ink-3', 'ink-4', 'ink-5', 'line-1', 'line-2', 'tx-1', 'tx-2', 'tx-3', 'acc', 'acc-hi', 'sel', 'focus', 'ok', 'warn', 'bad', 'who-gui', 'who-cli', 'who-agent', 'who-sys'];
    const d = document.createElement('div'); d.className = 'swatches';
    d.innerHTML = names.map((n) => { const v = cs.getPropertyValue('--' + n).trim(); return `<span class="swatch"><i style="background:${v}"></i>${n} ${v}</span>`; }).join('');
    colorH.insertAdjacentElement('afterend', d);
  }
}

document.addEventListener('click', (e) => {
  const go = e.target.closest('[data-go]');
  if (go) { location.hash = go.dataset.go; return; }
  const th = e.target.closest('[data-theme]');
  if (th) { THEME = th.dataset.theme; try { localStorage.setItem('quiver-theme', THEME); } catch (err) { /* storage blocked */ } applyTheme(); return; }
  const sec = e.target.closest('[data-sec]');
  if (sec) { SETSEC = sec.dataset.sec; showScreen('prefs'); return; }
  const tab = e.target.closest('[data-tab]');
  if (tab) { location.hash = tab.dataset.tab === 'proposal' ? 'proposal' : (SCREENS.find((s) => s.id === (location.hash || '').slice(1)) ? location.hash.slice(1) : 'library'); }
});
document.addEventListener('keydown', (e) => {
  if (document.getElementById('view-screens').hidden || e.target.closest('input,textarea')) return;
  const i = SCREENS.findIndex((s) => s.id === (location.hash.slice(1) || 'library'));
  if (e.key === 'ArrowRight') location.hash = SCREENS[(i + 1) % SCREENS.length].id;
  if (e.key === 'ArrowLeft') location.hash = SCREENS[(i + SCREENS.length - 1) % SCREENS.length].id;
});
window.addEventListener('hashchange', route);
window.addEventListener('resize', fit);
renderThemeRow();
renderProposal();
route();
