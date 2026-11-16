/* ---------- review page ---------- */
const P = 'p', L = 'l', F = 'f', D = 'd';
const SCREENS = [
  { id: 'library', grp: 'Library', label: 'Library home', title: 'Library home', sub: 'Art-forward grid with the last-played game up front.', render: scrLibrary,
    notes: [
      '<b>Hero</b> is the last-played game, showing the profile and Proton build it will launch with, so Play never surprises.',
      '<b>Needs attention</b> collects what is waiting on you: updates, installs, save conflicts, agent approvals. It disappears when empty.',
      '<b>Grid</b> uses Ubisoft’s 240×280 thumb art. Badges encode state: update, conflict, queued, mod layers, favorite. Games that are not installed are desaturated, not hidden.',
      '<b>Collections</b>: smart ones (Installed, Favorites, Modded), franchise ones derived from <code>sort_string</code> in product configs, and your own. Sort by recent, name, series order, size or play time.',
      '<b>Keyboard</b>: arrows move, Enter opens, P plays, / filters, Ctrl K opens the command palette.',
    ],
    src: [['Owned games, names, art', P, 'demux ownership + product config (thumb, background, logo)'], ['Install state, size, update available', P, 'manifests, compared with local install records'], ['Last played, play time', L, 'ubid session tracking; Ubisoft play history later'], ['Franchise collections', P, 'sort_string, e.g. “Assassin’s Creed 16”'], ['Favorites, user collections', L, ''], ['Agent requests', L, 'ubid approval queue']] },
  { id: 'game', grp: 'Library', label: 'Game detail', title: 'Game detail', sub: 'List on the left, the game on the right, the way Steam’s library works.', render: scrGame,
    notes: [
      '<b>List</b>: recent and installed first, then not installed. The right edge carries status (progress, update, conflict, mods). Filter with /, move with j/k.',
      '<b>Play is a split button.</b> The arrow picks a mod profile or one-off options like “launch once with GE-Proton10-26”.',
      '<b>Chips under the hero</b> name the exact launch-options revision (r14) that will run. That number appears again in sessions, history and activity.',
      '<b>Sessions</b> record duration, exit status, profile and revision, which ties a crash to the settings change that caused it.',
      '<b>Tabs</b>: Overview, Launch options, Mods, Saves & progress, Add-ons (Origins has 43 SKUs), Files (verify, move, uninstall), Logs. Other tabs collapse the hero to a slim banner.',
    ],
    src: [['Hero art, executable, Denuvo, cloud saves, “Animus Control Panel”, links', P, 'product config'], ['Version, manifest, size', P, 'manifest + local install record'], ['Add-ons owned (4 of 43)', P, 'ownership associations; add-on names need store data'], ['Achievements, player stats', F, 'UbiServices; the stat ids are already in the config'], ['Sessions, crash codes', L, 'ubid'], ['Developer, release date, description', F, 'store metadata; left out until available']] },
  { id: 'launch', grp: 'Library', label: 'Launch options', title: 'Launch options and history', sub: 'Every change is a revision you can compare, test and restore.', render: scrLaunch,
    notes: [
      '<b>Three scopes</b>: this game, the mod profile, global defaults. Each value shows where it is inherited from. Changed values get a dot.',
      '<b>Edits are staged.</b> Save makes a new revision with an optional message. <b>Test launch</b> runs the staged config once without saving it.',
      '<b>History</b> shows who made each revision (app, CLI, agent, system), why (<code>--reason</code>) and how the next launches went. A clean session of 10 minutes or more tags the revision “known good”.',
      '<b>Restore never rewrites history.</b> Restoring r13 creates r15 with r13’s content, and that restore can itself be undone.',
      '<b>Proton changes snapshot the prefix first</b>, because Proton upgrades a prefix one way. Restoring a revision offers to restore its snapshot too.',
      '<b>Resolved command</b> is exactly what ubid will run. Copy it as a shell line or as <code>ubi config set</code>.',
    ],
    src: [['Executable, Ubisoft arguments, working directory', P, 'product config'], ['Proton builds', L, 'Steam compatibilitytools.d + ubi-managed builds'], ['Revisions, outcomes, snapshots', L, 'ubid config store'], ['Kernel support (NTSync), wrappers found', L, 'probed at startup']] },
  { id: 'mods', grp: 'Library', label: 'Mods', title: 'Mods as overlay layers', sub: 'Run vanilla or any profile. The base install is never touched.', render: scrMods,
    notes: [
      '<b>Profiles are named stacks of layers.</b> Vanilla always exists and runs the base install directly, with no mount.',
      '<b>Layers</b> come from archives or folders. ubi unpacks them into its own store with a file manifest, so conflicts are known before launch.',
      '<b>At launch</b> ubid mounts base (read-only) + layers + a per-profile writable layer with overlayfs in a user namespace, falling back to fuse-overlayfs. Updates and verify always see a clean base.',
      '<b>Profiles can carry launch overrides.</b> ReShade’s layer adds a DLL override only for this profile.',
      '<b>Modded profiles get their own saves</b> and skip cloud sync by default, so a broken mod can’t overwrite cloud saves.',
      '<b>After a game update</b>, layers built for an older version get a warning.',
    ],
    src: [['Base version, owned add-ons', P, 'manifest + ownership'], ['Layers, profiles, conflicts, writable layer', L, 'ubid mod store'], ['Mod sources such as Nexus', F, 'not planned for the first version']] },
  { id: 'saves', grp: 'Library', label: 'Saves & progress', title: 'Saves and progress', sub: 'Cloud slots, local backups and what the platform knows about your progress.', render: scrSaves,
    notes: [
      '<b>Slots</b> are Ubisoft cloud-save slots: number, Ubisoft’s save name (timestamp parsed for display), size, local and cloud times.',
      '<b>Status per slot</b>: in sync, cloud only, local only, conflict. Resolution is explicit. ubi never overwrites silently.',
      '<b>Every pull and push is preceded by a backup</b>, which is how ubi-core already works. Backups are listed and restorable, with optional named manual backups.',
      '<b>Progression cards</b> show what the platform can provide. Dashed tags mark data that is not wired yet.',
    ],
    src: [['Save slots', P, 'cloudsave_service V2'], ['Backups', L, '~/.local/share/ubisoft-linux'], ['Play time chart', L, 'local sessions; Ubisoft play sessions later'], ['Achievements', F, 'UbiServices'], ['Player stats', F, 'stat ids from config, values from UbiServices']] },
  { id: 'queue', grp: 'System', label: 'Downloads', title: 'Downloads', sub: 'One active job, a queue you can reorder, and where the space goes.', render: scrDownloads,
    notes: [
      '<b>Every job shows who started it</b>: you in the app, you in the CLI, an agent, or the auto-updater.',
      '<b>Phases mirror the installer</b>: manifest, slices, verify, then register and create the prefix.',
      '<b>Throughput</b> plots network against disk writes. Slice installs are often disk-bound, and this shows it.',
      '<b>Storage</b> shows usage per library folder and the biggest games. Failed jobs say why and offer Retry.',
      '<b>Rules</b>: speed limit, pause while playing, an update window, snapshot before updating.',
    ],
    src: [['Progress, speed, phases', L, 'ubid job events'], ['Sizes', P, 'manifests'], ['Disk usage', L, 'statvfs + install records']] },
  { id: 'prefs', grp: 'System', label: 'Settings', title: 'Settings', sub: 'Global defaults, Proton builds, storage, and what scripts and agents may do.', render: () => scrSettings(SETSEC),
    notes: [
      '<b>Every setting is a <code>ubi config</code> key.</b> Hovering a row shows the key and a copy-as-CLI button.',
      '<b>Compatibility</b>: Proton builds from Steam and ubi-managed downloads, with how many games use each. Default policy “newest GE-Proton”, with per-game pins.',
      '<b>Automation and CLI</b>: daemon status, tokens for agents, the MCP server, and a policy grid of what the CLI and agents may do without asking.',
      '<b>Library and storage</b>: several library folders; reflink support is detected because it makes prefix snapshots almost free.',
    ],
    src: [['All of it', L, 'ubid config store'], ['GE-Proton releases', D, 'GitHub releases API']] },
  { id: 'signin', grp: 'System', label: 'Sign in', title: 'Sign in with two-step verification', sub: 'Shown here as the session-expired case, over the cached library art.', render: scrSignin,
    notes: [
      '<b>Two cases</b>: first run (plain background, then onboarding) and session expired (shown here, over cached art).',
      '<b>Six digit boxes</b> accept paste and auto-advance. Email code and backup code are alternatives Ubisoft supports.',
      '<b>Login runs in the daemon.</b> The app hands over credentials once; the session ticket goes to the system keyring. <code>ubi login</code> is the same flow in a terminal.',
      '<b>This mock-up has no real inputs</b>: it is a picture of the screen, not a form.',
    ],
    src: [['Sign-in, 2FA, remember device', P, 'UbiServices sessions'], ['Session storage', L, 'Secret Service keyring']] },
  { id: 'activity', grp: 'System', label: 'Activity', title: 'Activity and approvals', sub: 'Every change from the app, the CLI, agents and the system in one log.', render: scrActivity,
    notes: [
      '<b>Append-only log</b> of every state change in ubid, with the actor: app, CLI, an agent token, or the system (scheduler, auto-update, sync).',
      '<b>Agents can pass <code>--reason</code></b>, and it is shown next to the change.',
      '<b>Each entry has its diff and the exact <code>ubi</code> command</b>, so anything done in the app can be repeated in a script.',
      '<b>Undo</b> for reversible changes adds a new revision. Destructive actions from agents wait for approval here and in a desktop notification.',
    ],
    src: [['Events, actors, diffs, approvals', L, 'ubid event log (SQLite)']] },
  { id: 'import', grp: 'Flows', label: 'Import', title: 'Import existing installs', sub: 'Adopt games from Lutris, Steam or a folder without re-downloading.', render: scrImport,
    notes: [
      '<b>Scans</b> Lutris game configs and prefixes, Steam compatdata for a Ubisoft Connect shortcut, and folders you point at.',
      '<b>Matching</b>: executable names from product configs identify the product; the manifest decides between matches, needs update and verify.',
      '<b>Per game</b>: use in place, move into the library, or copy. Fresh prefix recommended. Old save files go into backups and are compared with cloud before the first launch.',
    ],
    src: [['Product match, version check', P, 'configs + manifests'], ['Found installs, prefixes, saves', L, 'filesystem scan']] },
  { id: 'playing', grp: 'Flows', label: 'Now playing', title: 'Now playing', sub: 'The window steps aside while you play, then reports what happened.', render: scrNowPlaying,
    notes: [
      '<b>Compact card</b> while a game runs: elapsed time, profile, revision, and the health of the bridge, the Denuvo token and the ticket refresh.',
      '<b>Stop game</b> ends the process tree in the prefix after a confirm.',
      '<b>On exit</b> a desktop notification sums up the session and the save sync.',
      '<b>The top-left overlay is MangoHud</b>, enabled in launch options. It is not drawn by ubi.',
    ],
    src: [['Session state', L, 'ubid + ubi-bridge IPC'], ['Ticket refresh, Denuvo token', P, 'UbiServices, via the broker']] },
  { id: 'palette', grp: 'Flows', label: 'Command palette', title: 'Command palette', sub: 'Ctrl K. Every command is a daemon call with a CLI twin.', render: scrPalette,
    notes: [
      '<b>Ctrl K anywhere.</b> Fuzzy search over games, actions and settings, scoped to the current game first.',
      '<b>Every entry maps one-to-one to a daemon call</b> and shows its CLI form. Ctrl C copies it.',
      '<b>One command registry</b> drives the palette, <code>ubi --help</code> and the MCP tool list, so the three cannot drift apart.',
    ],
    src: [['Commands', L, 'ubid command registry']] },
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
  const extra = s.id === 'prefs' ? `<div class="seg" style="margin-left:8px">${['Compatibility', 'Automation and CLI', 'Library and storage'].map((n) => `<span style="cursor:pointer" data-sec="${n}" class="${n === SETSEC ? 'on' : ''}">${n}</span>`).join('')}</div>` : '';
  document.getElementById('st-title').innerHTML = `<h2>${s.title}</h2><p>${s.sub}</p>${extra}<div class="nav"><button type="button" aria-label="Previous screen" data-go="${SCREENS[(i + SCREENS.length - 1) % SCREENS.length].id}">‹</button><button type="button" aria-label="Next screen" data-go="${SCREENS[(i + 1) % SCREENS.length].id}">›</button></div>`;
  document.getElementById('stage-in').innerHTML = s.render();
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
  const toc = document.getElementById('toc');
  const slug = (s) => 'p-' + s.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
  toc.innerHTML = '';
  box.querySelectorAll('h2, h3').forEach((h) => {
    h.id = slug(h.textContent);
    const a = document.createElement('a');
    a.href = '#' + h.id; a.textContent = h.textContent; a.className = h.tagName.toLowerCase();
    if (h.tagName === 'H2' || /^(Color|Type|Spacing|Components|Settings history|Mod overlays|CLI)/.test(h.textContent)) toc.appendChild(a);
  });
  const colorH = [...box.querySelectorAll('h3')].find((h) => /^Color/.test(h.textContent));
  if (colorH) {
    const cs = getComputedStyle(document.documentElement);
    const names = ['ink-0', 'ink-1', 'ink-2', 'ink-3', 'ink-4', 'ink-5', 'line-1', 'line-2', 'tx-1', 'tx-2', 'tx-3', 'acc', 'acc-hi', 'ok', 'warn', 'bad', 'who-gui', 'who-cli', 'who-agent', 'who-sys'];
    const d = document.createElement('div'); d.className = 'swatches';
    d.innerHTML = names.map((n) => { const v = cs.getPropertyValue('--' + n).trim(); return `<span class="swatch"><i style="background:${v}"></i>${n} ${v}</span>`; }).join('');
    colorH.insertAdjacentElement('afterend', d);
  }
}

document.addEventListener('click', (e) => {
  const go = e.target.closest('[data-go]');
  if (go) { location.hash = go.dataset.go; return; }
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
renderProposal();
route();
