/* ---------- data & helpers ---------- */
const clean = (s) => s.replace(/[®™]/g, '').replace(/’/g, "'").replace(/\s+/g, ' ').trim();
const G = {};
GAMES.forEach((g) => { g.title = clean(g.name); G[g.id] = g; });
const SHORT = {
  1651: "AC Chronicles: China", 1847: "AC Chronicles: India", 1848: "AC Chronicles: Russia",
  5183: "AC III Remastered", 6100: "AC Mirage", 5059: "AC Odyssey", 3539: "AC Origins", 1875: "AC Syndicate",
  720: "AC Unity", 13504: "AC Valhalla", 5100: "Discovery Tour: Egypt", 5404: "Discovery Tour: Greece",
  420: "Far Cry 4", 1803: "Far Cry 5", 5405: "Immortals Fenyx Rising", 277: "Prince of Persia",
  5408: "Rabbids Coding!", 5277: "Starlink: Battle for Atlas", 11784: "The Settlers 3: History Ed.",
  5595: "Trackmania", 3600: "Trials Rising", 2688: "Watch Dogs 2", 3353: "Watch Dogs: Legion",
};
const nm = (id) => G[id].title;
const sn = (id) => SHORT[id] || nm(id);
const A = (id, k) => (ART[id] && ART[id][k]) || '';
const ic = (n, cls = '') => `<i data-lucide="${n}"${cls ? ` class="${cls}"` : ''}></i>`;

/* illustrative install state (mock) */
const ST = {
  3539: { inst: 1, size: '61.8 GB', play: '87 h', last: 'Yesterday', fav: 1 },
  1651: { inst: 1, size: '3.4 GB', play: '2 h', last: 'Nov 9' },
  5059: { inst: 1, size: '112.4 GB', play: '142 h', last: 'Oct 30', upd: '4.1 GB', mods: 3, fav: 1 },
  13504: { inst: 1, size: '131.0 GB', play: '64 h', last: 'Sep 12' },
  720: { inst: 1, size: '51.2 GB', play: '31 h', last: 'Aug 2', conflict: 1 },
  1803: { inst: 1, size: '44.9 GB', play: '22 h', last: 'Jul 19' },
  2688: { inst: 1, size: '47.6 GB', play: '18 h', last: 'Jun 3', mods: 1 },
  277: { inst: 1, size: '7.9 GB', play: '9 h', last: 'May 1' },
  6100: { dl: 64, size: '48.7 GB' },
  5595: { queued: 1, size: '6.2 GB', fav: 1 },
  5183: { size: '38.5 GB' }, 1847: { size: '3.1 GB' }, 1848: { size: '3.3 GB' }, 1875: { size: '47.2 GB' },
  5100: { size: '24.0 GB' }, 5404: { size: '29.6 GB' }, 420: { size: '40.1 GB' }, 5405: { size: '33.8 GB' },
  5408: { size: '1.2 GB' }, 5277: { size: '27.5 GB' }, 11784: { size: '0.9 GB' }, 3600: { size: '19.7 GB' }, 3353: { size: '71.4 GB' },
};
const RECENT = [3539, 1651, 5059, 13504, 720, 1803, 2688, 277, 6100, 5595];
const REST = [5183, 1847, 1848, 1875, 5100, 5404, 420, 5405, 5408, 5277, 11784, 3600, 3353];
const ORDER = RECENT.concat(REST);

function topbar(active) {
  const a = (k) => (active === k ? 'on' : '');
  return `<div class="topbar">
    <div class="brand"><div class="mark"><i></i><i></i><i></i></div>ubi</div>
    <nav class="nav">
      <a class="${a('lib')}">${ic('library-big')}Library</a>
      <a class="${a('dl')}">${ic('download')}Downloads <span class="badge acc num">64%</span></a>
      <a class="${a('act')}">${ic('activity')}Activity <span class="badge agent">1</span></a>
    </nav>
    <div class="search">${ic('search')}<span class="grow">Search games, settings, commands</span><span class="kbd">Ctrl K</span></div>
    <div class="daemon" title="Background service"><span class="dot"></span>ubid · 3 jobs</div>
    <div class="iconbtn">${ic('bell')}<span class="pip"></span></div>
    <div class="iconbtn">${ic('settings')}</div>
    <div class="avatar">AG</div>
  </div>`;
}

function tile(id, opts = {}) {
  const s = ST[id] || {};
  const off = !s.inst && !s.dl;
  let tl = '', tr = '', bl = '', meta;
  if (s.upd) tl += `<span class="tag acc">${ic('arrow-down-to-line', 'xs')}Update</span>`;
  if (s.conflict) tl += `<span class="tag warn">${ic('cloud-alert', 'xs')}Conflict</span>`;
  if (s.queued) tl += `<span class="tag agent">${ic('bot', 'xs')}Queued</span>`;
  if (s.mods) tr += `<span class="tag">${ic('layers', 'xs')}${s.mods}</span>`;
  if (s.fav) tr += `<span class="tag">${ic('star', 'xs')}</span>`;
  if (s.dl) bl = `<div class="bl"><div class="row" style="justify-content:space-between;font-size:11px;font-weight:600;margin-bottom:5px"><span>Installing</span><span class="num">${s.dl}%</span></div><div class="bar"><i style="width:${s.dl}%"></i></div></div>`;
  if (s.inst) meta = `${s.play} played · ${s.last}`;
  else if (s.dl) meta = `31.2 of 48.7 GB · 3 min left`;
  else if (s.queued) meta = `Queued · ${s.size}`;
  else meta = `Not installed · ${s.size}`;
  if (s.upd) meta = `Update ${s.upd} · ${s.play} played`;
  if (s.conflict) meta = `Save conflict in slot 2`;
  const hp = opts.hover ? `<div class="hoverplay"><span>${ic('play')}</span></div>` : '';
  return `<div class="tile${opts.hover ? ' hover' : ''}">
    <div class="boxart${off ? ' off' : ''}"><img alt="" src="${A(id, 'thumb')}">${hp}<div class="tl">${tl}</div><div class="tr">${tr}</div>${bl}</div>
    <div class="nm">${nm(id)}</div><div class="mt">${meta}</div></div>`;
}

/* ---------- Library home ---------- */
function scrLibrary() {
  const attn = [
    [5059, 'Update ready', '1.6.1 · 4.1 GB · scheduled 02:00', `<span class="btn sm">Update now</span>`],
    [6100, 'Installing 64%', `<div class="bar" style="margin-top:6px;width:150px"><i style="width:64%"></i></div>`, `<span class="iconbtn" style="width:26px;height:26px">${ic('pause', 'sm')}</span>`],
    [720, 'Save conflict', 'Slot 2 changed locally and in the cloud', `<span class="btn sm">Resolve</span>`],
    [2688, 'Uninstall?', 'Asked by claude-code · frees 47.6 GB', `<span class="btn sm">Review</span>`],
  ].map(([id, t, sub, act]) => `<div class="row" style="gap:12px;padding:10px 0;border-top:1px solid rgba(255,255,255,.07)">
      <img src="${A(id, 'thumb')}" style="width:34px;height:40px;border-radius:5px;object-fit:cover" alt="">
      <div class="grow col" style="gap:2px"><div style="font-size:12.5px;font-weight:600">${sn(id)} <span class="muted" style="font-weight:400">· ${t}</span></div><div class="muted" style="font-size:11.5px">${sub}</div></div>${act}</div>`).join('');

  return `<div class="app">${topbar('lib')}
  <div class="scroll" style="flex:1">
    <section style="position:relative;height:396px;overflow:hidden">
      <img src="${A(3539, 'hero')}" alt="" style="position:absolute;inset:0;width:100%;height:100%;object-fit:cover;object-position:50% 35%">
      <div style="position:absolute;inset:0;background:linear-gradient(90deg,rgba(10,12,16,.92) 0%,rgba(10,12,16,.55) 34%,rgba(10,12,16,0) 58%),linear-gradient(180deg,rgba(15,18,24,0) 55%,var(--ink-1) 100%)"></div>
      <div style="position:absolute;left:40px;bottom:44px;width:520px;display:flex;flex-direction:column;gap:14px">
        <div class="cap" style="color:var(--acc-hi)">Continue playing</div>
        <div style="font-family:var(--f-disp);font-stretch:112%;font-weight:800;font-size:40px;line-height:1.02;letter-spacing:-.005em">Assassin's Creed Origins</div>
        <div class="dim" style="font-size:13.5px">Last session yesterday, 19:33 to 21:47 · 87 h 12 m total</div>
        <div class="row" style="gap:6px">
          <span class="chip">${ic('layers', 'xs')}Vanilla</span>
          <span class="chip">${ic('cpu', 'xs')}GE-Proton10-25</span>
          <span class="chip">${ic('cloud', 'xs')}Saves synced 21:47</span>
        </div>
        <div class="row" style="gap:10px;margin-top:4px">
          <div class="play"><div class="main">${ic('play')}Play</div><div class="more">${ic('chevron-down')}</div></div>
          <span class="btn lg" style="background:rgba(255,255,255,.08);border-color:rgba(255,255,255,.14)">Game details</span>
        </div>
      </div>
      <div class="glass" style="position:absolute;right:32px;top:28px;width:372px;border-radius:14px;padding:14px 16px 6px">
        <div class="row" style="margin-bottom:6px"><span class="cap" style="color:var(--tx-2)">Needs attention</span><span class="badge">4</span><span class="grow"></span><span class="muted" style="font-size:11.5px">Dismiss all</span></div>
        ${attn}
      </div>
    </section>
    <section style="padding:6px 40px 40px;display:flex;flex-direction:column;gap:18px">
      <div class="row" style="gap:14px">
        <div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:20px">Library</div>
        <div class="muted">23 games · 10 installed · 576 GB on disk</div>
        <span class="grow"></span>
        <span class="field" style="width:220px">${ic('list-filter')}<span class="muted">Filter</span><span class="grow"></span><span class="kbd">/</span></span>
        <span class="btn">${ic('arrow-down-up', 'sm')}Recently played${ic('chevron-down', 'sm')}</span>
        <div class="seg"><span class="on">${ic('layout-grid', 'sm')}</span><span>${ic('list', 'sm')}</span></div>
      </div>
      <div class="row" style="gap:6px;flex-wrap:wrap">
        <span class="chip on">All <span class="n">23</span></span>
        <span class="chip">Installed <span class="n">10</span></span>
        <span class="chip">${ic('star', 'xs')}Favorites <span class="n">3</span></span>
        <span class="chip">${ic('layers', 'xs')}Modded <span class="n">2</span></span>
        <span style="width:1px;height:18px;background:var(--line-2);margin:0 6px"></span>
        <span class="chip">Assassin's Creed <span class="n">12</span></span>
        <span class="chip">Far Cry <span class="n">2</span></span>
        <span class="chip">Watch Dogs <span class="n">2</span></span>
        <span class="chip">Couch co-op <span class="n">3</span></span>
        <span class="chip" style="border-style:dashed">${ic('plus', 'xs')}Collection</span>
      </div>
      <div style="display:grid;grid-template-columns:repeat(8,minmax(0,1fr));gap:22px 18px">
        ${ORDER.map((id) => tile(id, { hover: id === 13504 })).join('')}
      </div>
    </section>
  </div></div>`;
}

/* ---------- list + detail shell ---------- */
function sideList(sel) {
  const row = (id) => {
    const s = ST[id] || {};
    let r = '';
    if (s.dl) r = `<span class="num" style="color:var(--acc-hi)">${s.dl}%</span>`;
    else if (s.upd) r = `<span style="color:var(--acc-hi)">${ic('arrow-down-to-line', 'xs')}</span>`;
    else if (s.conflict) r = `<span style="color:var(--warn)">${ic('cloud-alert', 'xs')}</span>`;
    else if (s.queued) r = `<span>Queued</span>`;
    else if (s.mods) r = `<span>${ic('layers', 'xs')}</span>`;
    const off = !s.inst && !s.dl;
    return `<div class="gl${id === sel ? ' on' : ''}${off ? ' off' : ''}"><img alt="" src="${A(id, 'thumb')}"><span class="n">${sn(id)}</span><span class="r">${r}</span></div>`;
  };
  return `<aside class="side">
    <div class="side-top">
      <div class="field">${ic('search')}<span class="muted grow">Filter 23 games</span><span class="kbd">/</span></div>
      <div class="row" style="gap:6px"><span class="chip on" style="height:24px">All</span><span class="chip" style="height:24px">Installed</span><span class="chip" style="height:24px">${ic('star', 'xs')}</span><span class="grow"></span><span class="iconbtn" style="width:26px;height:26px">${ic('arrow-down-up', 'sm')}</span></div>
    </div>
    <div class="side-list scroll">
      <div class="side-grp"><span class="cap">Recent and installed</span><span class="badge">10</span></div>
      ${RECENT.map(row).join('')}
      <div class="side-grp"><span class="cap">Not installed</span><span class="badge">13</span></div>
      ${REST.map(row).join('')}
    </div>
    <div class="side-foot">
      <div class="row" style="font-size:11.5px"><span class="dim">${ic('hard-drive', 'xs')}</span><span class="dim">~/Games</span><span class="grow"></span><span class="muted num">1.21 TB free of 2 TB</span></div>
      <div class="bar"><i style="width:39%;background:var(--tx-3)"></i></div>
      <span class="btn sm" style="justify-content:flex-start;background:transparent">${ic('folder-input', 'sm')}Import existing games</span>
    </div>
  </aside>`;
}

const TABS = ['Overview', 'Launch options', 'Mods', 'Saves & progress', 'Add-ons', 'Files', 'Logs'];
function tabs(on, id) {
  const extra = { 'Add-ons': `<span class="badge">${G[id].dlc_count}</span>`, Mods: id === 5059 ? `<span class="badge">3</span>` : '' };
  return `<div class="tabs">${TABS.map((t) => `<a class="${t === on ? 'on' : ''}">${t}${extra[t] || ''}</a>`).join('')}</div>`;
}

function miniHero(id, right = '') {
  const baked = id === 3539;
  return `<div class="mini-hero"><img class="bg" alt="" src="${A(id, 'hero')}"${baked ? ' style="object-position:50% 92%"' : ''}><div class="shade"></div>
    ${baked ? `<div style="font-family:var(--f-disp);font-stretch:112%;font-weight:800;font-size:24px">${nm(id)}</div>` : `<img class="lg" alt="${nm(id)}" src="${A(id, 'logo')}">`}
    <span class="grow"></span>${right}
    <div class="play sm"><div class="main">${ic('play')}Play</div><div class="more">${ic('chevron-down', 'sm')}</div></div>
  </div>`;
}

function detailShell(id, tab, inner, opts = {}) {
  return `<div class="app">${topbar('lib')}<div class="split">${sideList(id)}
    <main class="pane"><div class="amb"><img alt="" src="${A(id, 'amb')}"></div>
      ${opts.full || miniHero(id, opts.heroRight || '')}
      ${opts.full ? '' : tabs(tab, id)}
      ${inner}
    </main></div>${opts.overlay || ''}</div>`;
}

/* ---------- Game detail ---------- */
function achTile(icon, hue, locked) {
  return `<div title="" style="width:44px;height:44px;border-radius:10px;display:grid;place-items:center;flex:none;${locked
    ? 'background:var(--ink-3);color:var(--tx-4);border:1px solid var(--line-2)'
    : `background:linear-gradient(145deg,hsl(${hue} 60% 46%),hsl(${hue} 55% 28%));color:#fff;box-shadow:inset 0 1px 0 rgba(255,255,255,.25)`}">${ic(locked ? 'lock' : icon, 'sm')}</div>`;
}

function scrGame() {
  const id = 3539;
  const sess = [
    ['Yesterday', '19:33 – 21:47', '2 h 14 m', 'r14', 'ok', 'Exited normally · 3 saves uploaded'],
    ['Nov 12', '20:02 – 21:07', '1 h 05 m', 'r13', 'ok', 'Exited normally · 2 saves uploaded'],
    ['Nov 10', '19:31 – 19:35', '4 m', 'r12', 'bad', 'Crashed at startup · 0xC0000005'],
    ['Nov 8', '18:10 – 21:51', '3 h 41 m', 'r11', 'ok', 'Exited normally · 4 saves uploaded'],
  ].map(([d, t, dur, r, st, msg]) => `<tr>
      <td><div style="font-weight:550">${d}</div><div class="muted num" style="font-size:11.5px">${t}</div></td>
      <td class="num" style="font-weight:600">${dur}</td>
      <td><span class="chip" style="height:22px">${ic('layers', 'xs')}Vanilla</span></td>
      <td><span class="mono dim">${r}</span></td>
      <td><span class="row" style="gap:6px;color:${st === 'ok' ? 'var(--tx-2)' : '#ff8b97'}">${ic(st === 'ok' ? 'circle-check' : 'circle-x', 'sm')}${msg}</span></td>
      <td style="text-align:right">${st === 'bad' ? `<span class="btn sm ghost">View log</span>` : ''}</td></tr>`).join('');

  const inner = `<div class="body scroll" style="padding-top:4px">
    <div style="display:grid;grid-template-columns:minmax(0,1fr) 360px;gap:20px">
      <div class="col" style="gap:20px">
        <div class="card">
          <div class="card-h"><h4>Sessions</h4><span class="muted">Tracked locally since Oct 2</span><span class="grow"></span><span class="btn sm ghost">All sessions${ic('chevron-right', 'sm')}</span></div>
          <table class="tbl"><tbody>${sess}</tbody></table>
        </div>
        <div class="card">
          <div class="card-h"><h4>Progression</h4><span class="pill future">${ic('plug-zap', 'xs')}UbiServices, not connected yet</span><span class="grow"></span><span class="btn sm ghost">Saves & progress${ic('chevron-right', 'sm')}</span></div>
          <div class="card-b col" style="gap:16px">
            <div class="row" style="gap:14px"><div style="font-size:22px;font-weight:700" class="num">41<span class="muted" style="font-size:14px;font-weight:500"> / 62 achievements</span></div><div class="grow bar thick"><i style="width:66%"></i></div><span class="num dim">66%</span></div>
            <div class="row" style="gap:8px">${achTile('pyramid', 38)}${achTile('eye', 200)}${achTile('swords', 4)}${achTile('ship', 190)}${achTile('feather', 280)}${achTile('crown', 45)}${achTile('', 0, 1)}${achTile('', 0, 1)}${achTile('', 0, 1)}<span class="muted" style="margin-left:6px;font-size:12px">+ 53 more</span></div>
            <div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px">
              ${[['Total progression', '68%'], ['Player level', '40'], ['Total kills', '3,214'], ['Distance travelled', '1,182 km']].map(([k, v]) => `<div style="background:var(--ink-1);border:1px solid var(--line-1);border-radius:8px;padding:10px 12px"><div class="cap" style="font-size:9.5px">${k}</div><div class="num" style="font-size:17px;font-weight:650;margin-top:3px">${v}</div></div>`).join('')}
            </div>
          </div>
        </div>
      </div>
      <div class="col" style="gap:20px">
        <div class="card"><div class="card-h"><h4>This install</h4><span class="grow"></span><span class="btn sm ghost">${ic('shield-check', 'sm')}Verify</span></div>
          <div class="card-b"><dl class="kv" style="margin:0">
            <dt>Version</dt><dd>1.6.0 <span class="muted mono">· manifest 3B9C…E4A1</span></dd>
            <dt>Size</dt><dd class="num">61.8 GB on nvme0 (ext4)</dd>
            <dt>Executable</dt><dd class="mono">ACOrigins.exe</dd>
            <dt>DRM</dt><dd>Denuvo, activated by ubid</dd>
            <dt>Cloud saves</dt><dd>On · pull before, push after</dd>
            <dt>Game settings</dt><dd>Animus Control Panel <span class="muted mono">ACP.ini</span></dd>
            <dt>Add-ons</dt><dd>4 of 43 owned · all installed</dd>
          </dl></div></div>
        <div class="card"><div class="card-h"><h4>Locations</h4></div>
          <div class="card-b col" style="gap:8px">
            ${[['Game', '~/Games/ubi-library/Assassin\'s Creed Origins'], ['Prefix', '~/Games/ubi-prefixes/3539'], ['Saves', '…/savegames/&lt;user&gt;/3539'], ['Logs', '~/.local/state/ubi/logs/3539']].map(([k, v]) => `<div class="row" style="gap:10px;font-size:12px"><span class="muted" style="width:46px">${k}</span><span class="mono dim grow" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${v}</span><span class="dim">${ic('folder-open', 'sm')}</span></div>`).join('')}
          </div></div>
        <div class="card"><div class="card-h"><h4>Links</h4></div>
          <div class="card-b row" style="gap:6px;flex-wrap:wrap">
            <span class="chip">${ic('globe', 'xs')}Homepage</span><span class="chip">${ic('life-buoy', 'xs')}Support</span><span class="chip">${ic('messages-square', 'xs')}Forums</span><span class="chip">${ic('book-open', 'xs')}ProtonDB</span><span class="chip">${ic('book-open', 'xs')}PCGamingWiki</span>
          </div></div>
        <div class="cli"><span class="p">$</span><span class="t">ubi play 3539 --profile vanilla</span><span class="iconbtn" style="width:24px;height:24px">${ic('copy', 'xs')}</span></div>
      </div>
    </div></div>`;

  const full = `<div class="hero"><img class="bg" alt="" src="${A(id, 'hero')}" style="object-position:50% 30%"><div class="shade"></div>
      <div class="row" style="position:absolute;right:24px;top:20px;gap:6px"><span class="iconbtn glass" style="border-radius:8px">${ic('star', 'sm')}</span><span class="iconbtn glass" style="border-radius:8px">${ic('image', 'sm')}</span></div></div>
    <div class="actionbar">
      <div class="play"><div class="main">${ic('play')}Play</div><div class="more">${ic('chevron-down')}</div></div>
      <span class="btn" style="height:36px">${ic('layers', 'sm')}Vanilla${ic('chevron-down', 'sm')}</span>
      <span class="chip" style="height:30px">${ic('cpu', 'xs')}GE-Proton10-25 · r14</span>
      <span class="chip" style="height:30px;color:var(--ok)">${ic('cloud', 'xs')}Saves synced</span>
      <span class="grow"></span>
      <span class="iconbtn bd">${ic('folder-open')}</span><span class="iconbtn bd">${ic('sliders-horizontal')}</span><span class="iconbtn bd">${ic('ellipsis')}</span>
    </div>
    <div class="stats">
      ${[['Last played', 'Yesterday'], ['Play time', '87 h 12 m'], ['Achievements', '41 / 62'], ['Installed', '61.8 GB'], ['Version', '1.6.0'], ['Add-ons', '4 of 43 owned']].map(([k, v]) => `<div class="stat"><span class="cap">${k}</span><b>${v}</b></div>`).join('')}
    </div>
    ${tabs('Overview', id)}`;
  return detailShell(id, 'Overview', inner, { full });
}
