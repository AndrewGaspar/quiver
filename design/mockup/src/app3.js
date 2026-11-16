/* ---------- Downloads ---------- */
function areaSvg(w = 930, h = 130) {
  const n = 60, net = [], disk = [];
  for (let i = 0; i < n; i++) {
    const t = i / n;
    net.push(70 + 18 * Math.sin(i * 0.45) + 9 * Math.sin(i * 1.7) + (i > 40 ? 6 : 0));
    disk.push(150 + 70 * Math.max(0, Math.sin(i * 0.33 + 1)) + 25 * Math.sin(i * 1.1));
  }
  const max = 260, X = (i) => 34 + (i / (n - 1)) * (w - 34), Y = (v) => h - 16 - (v / max) * (h - 26);
  const line = (a) => a.map((v, i) => `${i ? 'L' : 'M'}${X(i).toFixed(1)},${Y(v).toFixed(1)}`).join('');
  const grid = [0, 100, 200].map((t) => `<line x1="34" x2="${w}" y1="${Y(t)}" y2="${Y(t)}" style="stroke:var(--line-1)"/><text x="0" y="${Y(t) + 3.5}" style="fill:var(--tx-3)" font-size="9.5" font-family="Geist, sans-serif">${t}</text>`).join('');
  return `<svg viewBox="0 0 ${w} ${h}" width="100%" style="display:block;height:auto">
    <defs><linearGradient id="gA" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#7b93ff" stop-opacity=".45"/><stop offset="1" stop-color="#7b93ff" stop-opacity="0"/></linearGradient></defs>
    ${grid}<path d="${line(net)}L${X(n - 1)},${Y(0)}L${X(0)},${Y(0)}Z" fill="url(#gA)"/><path d="${line(net)}" fill="none" stroke="#7b93ff" stroke-width="1.6"/>
    <path d="${line(disk)}" fill="none" stroke="#4cc9c0" stroke-width="1.3" stroke-dasharray="3 3"/>
    <circle cx="${X(n - 1)}" cy="${Y(net[n - 1])}" r="3.5" fill="#7b93ff" stroke="#0f1218" stroke-width="2"/>
    <text x="36" y="${h - 2}" style="fill:var(--tx-3)" font-size="9.5" font-family="Geist, sans-serif">5 min ago</text><text x="${w}" y="${h - 2}" text-anchor="end" style="fill:var(--tx-3)" font-size="9.5" font-family="Geist, sans-serif">now</text></svg>`;
}

function scrDownloads() {
  const q = [
    [5059, 'Update 1.5.6 → 1.6.1', '4.1 GB', '<span class="who sys">' + ic('cog', 'xs') + 'Auto-update</span>', 'Waits for 02:00 window'],
    [5595, 'Install', '6.2 GB', '<span class="who agent">' + ic('bot', 'xs') + 'claude-code</span>', '“install Trackmania for tonight”'],
    [2688, 'Verify files', '47.6 GB to read', '<span class="who term">' + ic('square-terminal', 'xs') + 'You · CLI</span>', 'quiver verify 2688'],
  ].map(([id, what, size, who, note], i) => `<div class="row" style="gap:14px;padding:12px 16px;border-top:1px solid var(--line-1)">
      <span class="muted">${ic('grip-vertical', 'sm')}</span><span class="mono muted" style="width:14px">${i + 1}</span>
      <img src="${A(id, 'thumb')}" alt="" style="width:38px;height:44px;border-radius:6px;object-fit:cover">
      <div class="col grow" style="gap:2px"><div style="font-weight:600">${nm(id)}</div><div class="muted" style="font-size:12px">${what} · <span class="num">${size}</span></div></div>
      <div class="col" style="gap:4px;align-items:flex-end;width:240px">${who}<span class="muted" style="font-size:11.5px">${note}</span></div>
      <span class="iconbtn">${ic('play', 'sm')}</span><span class="iconbtn">${ic('x', 'sm')}</span></div>`).join('');

  const done = [
    [1651, 'Installed', '3.4 GB', 'Nov 9', 'ok', ''],
    [3539, 'Updated 1.5.1 → 1.6.0', '2.3 GB', 'Nov 4', 'ok', 'Prefix snapshot r11 taken first'],
    [420, 'Install failed', '12.0 of 40.1 GB', 'Oct 28', 'bad', 'No space left on /mnt/hdd'],
  ].map(([id, what, size, when, st, note]) => `<div class="row" style="gap:12px;padding:10px 16px;border-top:1px solid var(--line-1)">
      <span style="color:${st === 'ok' ? 'var(--ok)' : 'var(--bad)'}">${ic(st === 'ok' ? 'circle-check' : 'circle-x', 'sm')}</span>
      <div class="col grow" style="gap:1px"><div style="font-weight:550">${sn(id)} <span class="muted" style="font-weight:400">· ${what}</span></div><div class="muted" style="font-size:11.5px">${note || '&nbsp;'}</div></div>
      <span class="num muted" style="font-size:12px">${size}</span><span class="muted" style="width:52px;text-align:right;font-size:12px">${when}</span>
      ${st === 'bad' ? '<span class="btn sm">Retry</span>' : '<span style="width:56px"></span>'}</div>`).join('');

  const segs = [[13504, 131, '#7b93ff'], [5059, 112.4, '#9aaaff'], [3539, 61.8, '#c38bff'], [720, 51.2, '#4cc9c0'], [6100, 48.7, '#f0b44c'], [2688, 47.6, '#4fc48e'], [1803, 44.9, '#f06272'], [0, 79, '#4f5667']];
  const used = 790, total = 2000;
  const disk = `<div class="card card-b col" style="gap:12px">
    <div class="row"><span style="font-weight:650">Storage</span><span class="grow"></span><span class="btn sm ghost">Manage</span></div>
    <div class="col" style="gap:6px"><div class="row" style="font-size:12px"><span class="mono dim">~/Games</span><span class="muted">· nvme0 · ext4</span><span class="grow"></span><span class="num muted">1.21 TB free</span></div>
      <div class="row" style="height:10px;border-radius:5px;overflow:hidden;background:var(--ink-4);gap:1px">${segs.map(([, gb, c]) => `<i style="display:block;height:100%;width:${(gb / total) * 100}%;background:${c}"></i>`).join('')}</div></div>
    <div class="col" style="gap:5px;font-size:12px">${segs.map(([id, gb, c]) => `<div class="row" style="gap:8px"><i style="width:8px;height:8px;border-radius:2px;background:${c};display:block"></i><span class="dim grow">${id ? sn(id) : '3 others + prefixes'}</span><span class="num muted">${gb} GB</span></div>`).join('')}</div>
    <div class="sep"></div>
    <div class="row" style="font-size:12px"><span class="mono dim">/mnt/hdd/games</span><span class="muted">· sda1</span><span class="grow"></span><span class="num muted">3.1 TB free</span></div>
    <div class="bar thick"><i style="width:22%;background:var(--tx-4)"></i></div>
  </div>`;

  const prefs = `<div class="card card-b col" style="gap:10px;font-size:12.5px">
    <div style="font-weight:650;font-size:13px">Download rules</div>
    <div class="row"><span class="dim grow">Speed limit</span><span class="field" style="height:28px;width:120px">Unlimited${ic('chevron-down')}</span></div>
    <div class="row"><span class="dim grow">Pause while a game runs</span><span class="toggle on"></span></div>
    <div class="row"><span class="dim grow">Updates install</span><span class="field" style="height:28px;width:140px;white-space:nowrap">02:00 – 07:00${ic('chevron-down')}</span></div>
    <div class="row"><span class="dim grow">Snapshot prefix first</span><span class="muted" style="font-size:11.5px;margin-right:8px">btrfs/xfs: auto</span><span class="toggle on"></span></div>
    <div class="row"><span class="dim grow">Offer Steam shortcut on install</span><span class="toggle on"></span></div>
    <div class="cli" style="margin-top:4px"><span class="p">$</span><span class="t">quiver jobs --watch</span>${ic('copy', 'xs')}</div>
  </div>`;

  return `<div class="app">${topbar('dl')}<div class="scroll" style="flex:1;padding:24px 32px">
    <div class="row" style="gap:12px;margin-bottom:18px"><div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:22px">Downloads</div><span class="muted">1 active · 3 queued</span><span class="grow"></span><span class="btn">${ic('pause', 'sm')}Pause all</span><span class="btn">${ic('plus', 'sm')}Install a game</span></div>
    <div style="display:grid;grid-template-columns:minmax(0,1fr) 340px;gap:20px">
      <div class="col" style="gap:20px">
        <div class="card" style="overflow:hidden;position:relative">
          <img src="${A(6100, 'hero')}" alt="" style="position:absolute;inset:0;width:100%;height:100%;object-fit:cover;object-position:60% 30%;opacity:.38">
          <div style="position:absolute;inset:0;background:linear-gradient(90deg,var(--ink-2) 30%,color-mix(in srgb, var(--ink-2) 55%, transparent))"></div>
          <div style="position:relative;padding:20px 22px;display:flex;flex-direction:column;gap:14px">
            <div class="row" style="gap:16px">
              <img src="${A(6100, 'logo')}" alt="Assassin's Creed Mirage" style="height:64px;max-width:200px;object-fit:contain;object-position:left">
              <div class="col grow" style="gap:4px"><div class="row" style="gap:8px"><span class="pill acc">Installing</span><span class="who gui">${ic('mouse-pointer-2', 'xs')}You · app</span><span class="muted mono" style="font-size:11px">job 418</span></div>
                <div class="row num" style="gap:18px;font-size:13px"><span><b>31.2</b> <span class="muted">of 48.7 GB</span></span><span><b>84.6 MB/s</b> <span class="muted">network</span></span><span><b>212 MB/s</b> <span class="muted">disk</span></span><span><b>3 min 26 s</b> <span class="muted">left</span></span></div></div>
              <span class="btn">${ic('pause', 'sm')}Pause</span><span class="iconbtn bd">${ic('x', 'sm')}</span>
            </div>
            <div class="bar thick" style="height:8px"><i style="width:64%;background:linear-gradient(90deg,#6f88ff,#a6b5ff)"></i></div>
            <div class="row" style="gap:6px;font-size:11.5px">${[['Manifest', 'ok'], ['Download slices · 2,317 of 3,604', 'cur'], ['Verify', ''], ['Register, create prefix, add shortcuts', '']].map(([s, st], i) => `${i ? `<span class="muted">${ic('chevron-right', 'xs')}</span>` : ''}<span class="row" style="gap:5px;color:${st === 'ok' ? 'var(--ok)' : st === 'cur' ? 'var(--acc-hi)' : 'var(--tx-3)'}">${ic(st === 'ok' ? 'circle-check' : st === 'cur' ? 'loader' : 'circle', 'xs')}${s}</span>`).join('')}</div>
            <div style="background:color-mix(in srgb, var(--ink-0) 60%, transparent);border:1px solid var(--line-1);border-radius:10px;padding:10px 12px 6px">
              <div class="row" style="gap:14px;font-size:11.5px;margin-bottom:4px"><span class="cap">Throughput, MB/s</span><span class="grow"></span><span class="row" style="gap:5px"><i style="width:12px;height:2px;background:#7b93ff;display:block"></i>Network</span><span class="row" style="gap:5px"><i style="width:12px;height:0;border-top:2px dashed #4cc9c0;display:block"></i>Disk write</span></div>
              ${areaSvg()}
            </div>
            <div class="row" style="gap:16px;font-size:12.5px;padding:10px 12px;border-radius:10px;background:color-mix(in srgb, var(--ink-0) 60%, transparent);border:1px solid var(--line-1)">
              <span class="cap">When it finishes</span>
              <span class="row" style="gap:8px"><span class="check on">${ic('check')}</span>Add to Steam</span>
              <span class="row" style="gap:8px"><span class="check on">${ic('check')}</span>Create .desktop entry</span>
              <span class="row" style="gap:8px"><span class="check">${''}</span>Start the game</span>
              <span class="grow"></span><span class="muted" style="font-size:11.5px">Steam lists new shortcuts after it restarts</span>
            </div>
          </div>
        </div>
        <div class="card"><div class="card-h"><h4>Up next</h4><span class="muted" style="font-size:12px">Drag to reorder</span></div>${q.replace('border-top:1px solid var(--line-1)', 'border-top:0')}</div>
        <div class="card"><div class="card-h"><h4>Recent</h4><span class="grow"></span><span class="btn sm ghost">Clear</span></div>${done.replace('border-top:1px solid var(--line-1)', 'border-top:0')}</div>
      </div>
      <div class="col" style="gap:20px">${disk}${prefs}</div>
    </div></div></div>`;
}

/* ---------- Global settings ---------- */
function setNav(on) {
  const items = [['users', 'Accounts'], ['hard-drive', 'Library and storage'], ['download', 'Downloads'], ['cpu', 'Compatibility'], ['sliders-horizontal', 'Launch defaults'], ['cloud', 'Cloud saves'], ['layers', 'Mods'], ['palette', 'Appearance'], ['bell', 'Notifications'], ['keyboard', 'Keyboard and controller'], ['bot', 'Automation and CLI'], ['stethoscope', 'Diagnostics'], ['info', 'About']];
  return `<aside class="side" style="width:250px;padding:18px 10px">
    <div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:20px;padding:0 10px 12px">Settings</div>
    <div class="field" style="margin:0 6px 10px">${ic('search')}<span class="muted">Find a setting</span></div>
    ${items.map(([i, t]) => `<div class="gl${t === on ? ' on' : ''}" style="height:32px">${ic(i, 'sm')}<span class="n">${t}</span></div>`).join('')}
    <div class="grow"></div>
    <div class="muted" style="font-size:11px;line-height:1.45;padding:12px 10px 0">Quiver 0.9 · a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft.</div>
  </aside>`;
}

function scrSettings(section = 'Compatibility') {
  let body = '';
  if (section === 'Compatibility') {
    const rt = [
      ['GE-Proton10-26', 'Managed by Quiver', 'Nov 13', '2 games', '612 MB', 'new'],
      ['GE-Proton10-25', 'Steam compatibilitytools.d', 'Oct 28', '7 games', '598 MB', ''],
      ['Proton 10.0-3', 'Steam', 'Oct 2', '0 games', '1.1 GB', ''],
      ['Proton Experimental', 'Steam', 'Nov 12', '1 game', '1.2 GB', ''],
    ].map(([n, src, d, used, sz, tag]) => `<tr><td><div class="row" style="gap:8px"><span style="font-weight:600" class="mono">${n}</span>${tag ? '<span class="pill acc">New</span>' : ''}</div></td><td class="dim">${src}</td><td class="dim num">${d}</td><td class="dim">${used}</td><td class="num dim">${sz}</td><td style="text-align:right"><span class="iconbtn" style="display:inline-grid;width:26px;height:26px">${ic('ellipsis', 'sm')}</span></td></tr>`).join('');
    body = `<div class="col" style="gap:18px;max-width:900px">
      <div><div style="font-size:20px;font-weight:700">Compatibility</div><div class="muted">Proton builds, prefixes and what new games start with.</div></div>
      <div class="card"><div class="card-h"><h4>Proton builds</h4><span class="grow"></span><span class="btn sm">${ic('refresh-cw', 'sm')}Rescan</span><span class="btn sm pri">${ic('download', 'sm')}Get GE-Proton…</span></div>
        <table class="tbl"><thead><tr><th>Build</th><th>Source</th><th>Added</th><th>Used by</th><th>Size</th><th></th></tr></thead><tbody>${rt}</tbody></table></div>
      ${section_('Defaults for games', '', [
        frow('Default build', `<span class="field" style="width:260px">Newest GE-Proton<span class="grow"></span>${ic('chevron-down')}</span><span class="muted" style="font-size:12px">7 games follow this, 3 are pinned</span>`),
        frow('When a newer build appears', `<div class="seg"><span>Switch games that follow newest</span><span class="on">Ask me</span><span>Do nothing</span></div>`),
        frow('Before switching builds', `<span class="check on">${ic('check')}</span><span class="dim">Snapshot the prefix</span><span class="pill ok">Automatic · /mnt/hdd is btrfs</span><span class="muted" style="font-size:12px">ext4 folders: off</span><span class="btn sm ghost">Turn on for ext4</span>`, { help: 'Reflink copies cost almost nothing on btrfs and xfs; on ext4 each is a 1–3 GB copy' }),
        frow('New prefixes', `<div class="seg"><span class="on">Fresh per game</span><span>Clone a template</span></div><span class="muted" style="font-size:12px">~/Games/ubi-prefixes/&lt;product id&gt;</span>`),
      ].join(''))}
      <div class="card"><div class="card-h"><h4>Tools found on this system</h4><span class="muted" style="font-size:12px">Used by launch options; missing ones are greyed out there</span></div>
        <table class="tbl"><tbody>${[['gamescope', '3.16.4', '/usr/bin/gamescope', 1], ['MangoHud', '0.8.1', '/usr/bin/mangohud', 1], ['GameMode', '1.8.2', '/usr/bin/gamemoderun', 1], ['NTSync', 'kernel 7.2', '/dev/ntsync', 1], ['fuse-overlayfs', 'not installed', 'kernel overlayfs in user namespace is used instead', 0]].map(([n, v, p, ok]) => `<tr><td style="width:180px"><span class="row" style="gap:8px"><span class="dot ${ok ? '' : 'off'}"></span><b style="font-weight:600">${n}</b></span></td><td class="dim num" style="width:140px">${v}</td><td class="mono muted">${p}</td></tr>`).join('')}</tbody></table></div>
    </div>`;
  } else if (section === 'Automation and CLI') {
    const pol = [['Read library and status', 'Allow', 'Allow'], ['Change launch options, mod profiles, exports', 'Allow', 'Allow'], ['Launch and stop games', 'Allow', 'Allow'], ['Install and update', 'Allow', 'Allow'], ['Restore saves or settings', 'Allow', 'Ask'], ['Uninstall, delete prefixes, profiles or saves', 'Allow', 'Ask'], ['Sign in, sign out, add or remove accounts', 'Ask', 'Deny']];
    const sel = (v) => `<span class="field" style="height:26px;width:96px;color:${v === 'Ask' ? 'var(--warn)' : v === 'Deny' ? 'var(--bad)' : 'var(--tx-1)'}">${v}<span class="grow"></span>${ic('chevron-down')}</span>`;
    body = `<div class="col" style="gap:18px;max-width:900px">
      <div><div style="font-size:20px;font-weight:700">Automation and CLI</div><div class="muted">The app, the <span class="mono">quiver</span> command and agents all talk to the same background service.</div></div>
      <div class="card card-b col" style="gap:10px">
        <div class="row" style="gap:10px"><span class="dot"></span><span style="font-weight:650">quiverd is running</span><span class="muted mono" style="font-size:11.5px">pid 4121 · v0.9.0 · up 3 days</span><span class="grow"></span><span class="btn sm">Restart</span></div>
        <div class="row" style="gap:10px;font-size:12.5px"><span class="dim" style="width:180px">Socket</span><span class="mono dim">$XDG_RUNTIME_DIR/quiver/quiverd.sock</span></div>
        <div class="row" style="gap:10px;font-size:12.5px"><span class="dim" style="width:180px">Start at login</span><span class="toggle on"></span><span class="muted mono" style="font-size:11.5px">systemd --user quiverd.service</span></div>
        <div class="row" style="gap:10px;font-size:12.5px"><span class="dim" style="width:180px">MCP server for agents</span><span class="toggle on"></span><span class="muted mono" style="font-size:11.5px">quiver mcp</span></div>
      </div>
      <div class="card"><div class="card-h"><h4>Access tokens</h4><span class="grow"></span><span class="btn sm">${ic('plus', 'sm')}New token</span></div>
        <table class="tbl"><tbody>
          <tr><td><span class="who agent">${ic('bot', 'xs')}claude-code</span></td><td class="dim">All except account</td><td class="dim">Used 6 min ago</td><td style="text-align:right"><span class="btn sm ghost">Revoke</span></td></tr>
          <tr><td><span class="who sys">${ic('house', 'xs')}home-assistant</span></td><td class="dim">Read only</td><td class="dim">Used yesterday</td><td style="text-align:right"><span class="btn sm ghost">Revoke</span></td></tr>
        </tbody></table></div>
      <div class="card"><div class="card-h"><h4>What each client may do</h4><span class="muted" style="font-size:12px">The app always may; it asks you directly</span></div>
        <table class="tbl"><thead><tr><th>Action</th><th>quiver CLI (you)</th><th>Agents (tokens)</th></tr></thead><tbody>${pol.map(([a, c, g]) => `<tr><td>${a}</td><td>${sel(c)}</td><td>${sel(g)}</td></tr>`).join('')}</tbody></table></div>
    </div>`;
  } else if (section === 'Accounts') {
    const acct = (av, grad, name, mail, games, status, dot, extra) => `<div class="card card-b row" style="gap:14px;align-items:flex-start">
        <div class="avatar" style="width:40px;height:40px;font-size:14px;background:${grad}">${av}</div>
        <div class="col grow" style="gap:4px"><div class="row" style="gap:8px"><span style="font-weight:650;font-size:14px">${name}</span>${extra}</div><span class="muted" style="font-size:12px">${mail} · ${games}</span>
          <span class="row" style="gap:7px;font-size:12px;margin-top:4px"><span class="dot ${dot}"></span><span class="dim">${status}</span></span></div>
        <span class="btn sm ghost">Sign out</span></div>`;
    body = `<div class="col" style="gap:18px;max-width:900px">
      <div class="row"><div class="col grow"><div style="font-size:20px;font-weight:700">Accounts</div><div class="muted">Games from every signed-in account show up in one library.</div></div><span class="btn">${ic('plus', 'sm')}Add account</span></div>
      ${acct('AG', 'linear-gradient(135deg,#3b4a7a,#6a5aa8)', 'ajg', 'a•••@g•••.com', '23 games', 'Live session in quiverd · ticket refreshes in 41 min', '', '<span class="pill acc">Default</span>')}
      ${acct('F', 'linear-gradient(135deg,#2f6b5e,#4a9a83)', 'Family', 'f•••@g•••.com', '6 games, 2 only on this account', 'Signed in · connects when one of its games starts', 'off', '')}
      <div class="row" style="gap:10px;padding:12px 14px;border-radius:10px;background:var(--warn-soft);font-size:12.5px;align-items:flex-start">${ic('triangle-alert', 'sm')}<span style="color:var(--warn)">Ubisoft allows one live session per account. Signing in to Ubisoft Connect elsewhere with the same account ends Quiver's session for it, and its games won't start here until you sign in again.</span></div>
      <div class="card"><div class="card-h"><h4>Who owns what</h4><span class="muted" style="font-size:12px">Showing the 3 games that involve Family</span></div>
        <table class="tbl"><thead><tr><th>Game</th><th>Owned by</th><th>Launch with</th></tr></thead><tbody>
          ${[[3539, 'ajg, Family', 'ajg (default)'], [5408, 'Family', 'Family'], [5277, 'Family', 'Family']].map(([id, o, l]) => `<tr><td><span class="row" style="gap:10px"><img src="${A(id, 'thumb')}" alt="" style="width:22px;height:26px;border-radius:4px;object-fit:cover">${nm(id)}</span></td><td class="dim">${o}</td><td><span class="field" style="height:26px;width:150px">${l}<span class="grow"></span>${ic('chevron-down')}</span></td></tr>`).join('')}
        </tbody></table></div>
    </div>`;
  } else if (section === 'Appearance') {
    const om = ['#0a1713', '#142620', '#203b31', '#8bcbb1', '#dfede5', '#d88d91', '#c5cc91'];
    body = `<div class="col" style="gap:18px;max-width:900px">
      <div><div style="font-size:20px;font-weight:700">Appearance</div><div class="muted">Colors, density, art and button hints.</div></div>
      ${section_('Theme', '', [
        frow('Colors', `<div class="seg"><span class="on">Follow Omarchy</span><span>Quiver dark</span><span>Quiver light</span></div>`),
        frow('Current Omarchy theme', `<span class="row" style="gap:0;border-radius:6px;overflow:hidden">${om.map((c) => `<i style="width:18px;height:22px;background:${c};display:block"></i>`).join('')}</span><span style="font-weight:600">jade-horse</span><span class="pill mut">dark</span>`, { help: 'Restyles when theme.generation changes' }),
        frow('Density', `<div class="seg"><span class="on">Comfortable</span><span>Compact</span></div>`),
      ].join(''))}
      ${section_('Art', 'Ubisoft art is the default', [
        frow('Sources', `<span class="check on">${ic('check')}</span>Ubisoft<span class="check on" style="margin-left:12px">${ic('check')}</span>SteamGridDB<span class="pill ok">API key set</span><span class="check on" style="margin-left:12px">${ic('check')}</span>My own images`),
        frow('Custom art', `<span class="dim">2 games, 1 pinned profile</span><span class="btn sm ghost">Reset all to Ubisoft</span>`),
      ].join(''))}
      ${section_('Controller', '', [
        frow('Button hints', `<div class="seg"><span class="on">When I use a gamepad</span><span>Always</span><span>Never</span></div>`),
        frow('Guide button', `<span class="dim">Not used by Quiver. hyprpad owns guide chords.</span><span class="pill ok">hyprpad detected</span>`),
      ].join(''))}
    </div>`;
  } else {
    body = `<div class="col" style="gap:18px;max-width:900px">
      <div><div style="font-size:20px;font-weight:700">Library and storage</div><div class="muted">Where games, prefixes and art live.</div></div>
      <div class="card"><div class="card-h"><h4>Library folders</h4><span class="grow"></span><span class="btn sm">${ic('plus', 'sm')}Add folder</span></div>
        <table class="tbl"><tbody>
          <tr><td><div class="row" style="gap:8px"><span class="mono" style="font-weight:600">~/Games/ubi-library</span><span class="pill acc">Default</span></div><div class="muted" style="font-size:11.5px">nvme0 · ext4 · reflink: no</div></td><td style="width:260px"><div class="bar thick"><i style="width:39%"></i></div><div class="muted num" style="font-size:11.5px;margin-top:4px">790 GB used · 1.21 TB free</div></td><td class="dim">9 games</td></tr>
          <tr><td><div class="mono" style="font-weight:600">/mnt/hdd/games</div><div class="muted" style="font-size:11.5px">sda1 · btrfs · reflink: yes</div></td><td><div class="bar thick"><i style="width:22%;background:var(--tx-3)"></i></div><div class="muted num" style="font-size:11.5px;margin-top:4px">880 GB used · 3.1 TB free</div></td><td class="dim">1 game</td></tr>
        </tbody></table></div>
      ${section_('Other locations', '', [
        frow('Prefixes', `<span class="field mono grow">~/Games/ubi-prefixes</span><span class="btn sm">Move…</span>`),
        frow('Save backups', `<span class="field mono grow">~/.local/share/ubisoft-linux/backups</span><span class="muted num">412 MB</span>`),
        frow('Art cache', `<span class="field mono grow">~/.cache/quiver/art</span><span class="muted num">58 MB</span><span class="btn sm ghost">Clear</span>`),
        frow('Ubisoft SDK files', `<span class="field mono grow">Fetched from Ubisoft · uplay_r1 4.2, upc_r2 1.1</span>`),
      ].join(''))}
    </div>`;
  }
  return `<div class="app">${topbar('')}<div class="split">${setNav(section)}<main class="pane scroll" style="padding:28px 36px">${body}</main></div></div>`;
}
const section_ = (t, s, r) => section(t, s, r);

/* ---------- Sign in with 2FA ---------- */
function scrSignin() {
  const mosaic = ORDER.filter((x) => typeof x === 'number').concat(ORDER.filter((x) => typeof x === 'number')).slice(0, 40).map((id) => `<img src="${A(id, 'thumb')}" alt="" style="width:100%;aspect-ratio:240/280;object-fit:cover;border-radius:10px">`).join('');
  const digit = (d, cur) => `<div style="width:52px;height:62px;border-radius:10px;display:grid;place-items:center;font-size:26px;font-weight:650;font-family:var(--f-mono);background:var(--ink-1);border:1px solid ${cur ? 'var(--acc)' : 'var(--line-2)'};${cur ? 'box-shadow:0 0 0 4px var(--acc-soft)' : ''}">${d}${cur ? '<span style="width:2px;height:28px;background:var(--acc);display:block"></span>' : ''}</div>`;
  return `<div class="app" style="background:var(--ink-0)">
    <div style="position:absolute;inset:-40px;display:grid;grid-template-columns:repeat(10,1fr);gap:14px;opacity:.5">${mosaic}</div>
    <div style="position:absolute;inset:0;background:radial-gradient(ellipse 60% 70% at 50% 50%,color-mix(in srgb, var(--ink-0) 84%, transparent),color-mix(in srgb, var(--ink-0) 96%, transparent) 70%)"></div>
    <div style="position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);width:480px;display:flex;flex-direction:column;gap:18px">
      <div class="row" style="gap:12px;justify-content:center"><div class="mark" style="transform:scale(1.3)"><i></i><i></i><i></i></div><span style="font-family:var(--f-disp);font-stretch:115%;font-weight:800;font-size:22px">Quiver</span></div>
      <div class="row" style="gap:10px;padding:10px 14px;border-radius:10px;background:var(--warn-soft);border:1px solid rgba(240,180,76,.3);font-size:12.5px;color:var(--warn)">${ic('clock-alert', 'sm')}<span>The Ubisoft session for ajg expired. Its downloads are paused; Family is still signed in.</span></div>
      <div class="card" style="padding:28px 30px;display:flex;flex-direction:column;gap:20px;background:color-mix(in srgb, var(--ink-2) 94%, transparent);box-shadow:0 30px 80px rgba(0,0,0,.6)">
        <div class="row" style="gap:8px;font-size:12px">
          <span class="row" style="gap:6px;color:var(--ok)">${ic('circle-check', 'sm')}Account</span><span class="grow" style="height:1px;background:var(--line-2)"></span>
          <span class="row" style="gap:6px;color:var(--acc-hi);font-weight:600"><span style="width:16px;height:16px;border-radius:50%;background:var(--acc);color:var(--acc-ink);display:grid;place-items:center;font-size:10px;font-weight:800">2</span>Two-step verification</span><span class="grow" style="height:1px;background:var(--line-2)"></span>
          <span class="row muted" style="gap:6px"><span style="width:16px;height:16px;border-radius:50%;border:1px solid var(--tx-4);display:grid;place-items:center;font-size:10px">3</span>Library</span>
        </div>
        <div class="col" style="gap:6px"><div style="font-size:22px;font-weight:700">Enter your verification code</div><div class="dim">Open your authenticator app and enter the 6-digit code for <b style="color:var(--tx-1)">a•••@g•••.com</b>. <span style="color:var(--acc-hi)">Not you?</span></div></div>
        <div class="row" style="gap:10px;justify-content:space-between">${digit('4')}${digit('8')}${digit('1')}${digit('', 1)}${digit('')}${digit('')}</div>
        <div class="row" style="gap:10px;font-size:12.5px"><span class="check on">${ic('check')}</span><span class="dim">Trust this device so you are not asked again for 30 days</span></div>
        <div class="row" style="gap:10px"><span class="btn lg pri" style="flex:1;opacity:.55">Verify</span></div>
        <div class="row" style="gap:16px;font-size:12.5px;justify-content:center;color:var(--acc-hi)"><span>Email me a code instead</span><span class="muted">·</span><span>Use a backup code</span></div>
      </div>
      <div class="row" style="gap:10px;justify-content:center;font-size:12px" class="muted"><span class="muted">${ic('key-round', 'xs')}</span><span class="muted">Session ticket goes to the system keyring. Your password is never stored.</span></div>
      <div class="muted" style="text-align:center;font-size:11.5px">Quiver is a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft.</div>
      <div class="cli" style="align-self:center;width:300px"><span class="p">$</span><span class="t">quiver login</span><span class="muted" style="font-size:11px">same service</span></div>
    </div>
  </div>`;
}

/* ---------- Activity / audit ---------- */
function scrActivity() {
  const ev = (time, who, txt, id, extra = '', sel = false, act = '') => `<div style="display:grid;grid-template-columns:60px 150px minmax(0,1fr) auto;gap:14px;align-items:start;padding:12px 18px;border-top:1px solid var(--line-1);${sel ? 'background:var(--ink-3);box-shadow:inset 3px 0 0 var(--who-agent)' : ''}">
      <span class="num muted" style="padding-top:2px">${time}</span><span>${who}</span>
      <div class="col" style="gap:6px"><div class="row" style="gap:8px">${id ? `<img src="${A(id, 'thumb')}" alt="" style="width:20px;height:24px;border-radius:3px;object-fit:cover">` : ''}<span>${txt}</span></div>${extra}</div>
      <div class="row" style="gap:6px">${act}</div></div>`;
  const W = {
    gui: `<span class="who gui">${ic('mouse-pointer-2', 'xs')}You · app</span>`,
    cli: `<span class="who term">${ic('square-terminal', 'xs')}You · CLI</span>`,
    agent: `<span class="who agent">${ic('bot', 'xs')}claude-code</span>`,
    sys: `<span class="who sys">${ic('cog', 'xs')}System</span>`,
  };
  const undo = `<span class="btn sm ghost">${ic('undo-2', 'sm')}Undo</span>`;
  const copy = `<span class="iconbtn" style="width:26px;height:26px">${ic('square-terminal', 'sm')}</span>`;
  const day = (t) => `<div class="cap" style="padding:14px 18px 8px">${t}</div>`;
  const list = day('Today')
    + ev('21:52', W.agent, `Asked to uninstall <b>Watch Dogs 2</b> · frees 47.6 GB`, 2688, `<div class="muted" style="font-size:12px">“Make room for the Mirage install”</div>`, false, `<span class="pill warn">Waiting for you</span>`)
    + ev('21:47', W.sys, `Uploaded 3 save slots for <b>AC Origins</b>`, 3539, '', false, copy)
    + ev('19:33', W.gui, `Launched <b>AC Origins</b> · Vanilla · r14 · 2 h 14 m`, 3539, '', false, copy)
    + ev('19:31', W.gui, `Changed launch options of <b>AC Origins</b> · r13 → r14`, 3539, `<div class="mono muted" style="font-size:11.5px">+ wrappers.gamescope.enabled · + env.DXVK_HDR</div>`, false, undo + copy)
    + ev('18:05', W.agent, `Queued install of <b>Trackmania</b> · 6.2 GB`, 5595, `<div class="muted" style="font-size:12px">“install Trackmania for tonight”</div>`, false, `<span class="btn sm ghost">Cancel job</span>` + copy)
    + day('Nov 12')
    + ev('23:12', W.agent, `Changed launch options of <b>AC Origins</b> · r12 → r13`, 3539, `<div class="col" style="gap:8px">
        <div class="dim" style="font-size:12.5px">Reason given: “r12 crashed at startup. Pin 10-25 and go back to fsync.”</div>
        <div class="diff" style="max-width:560px"><div class="r">- runtime.proton = "newest"</div><div class="a">+ runtime.proton = "GE-Proton10-25"</div><div class="r">- runtime.sync = "ntsync"</div><div class="a">+ runtime.sync = "fsync"</div></div>
        <div class="code" style="max-width:560px"><span class="c"># ran via token claude-code · pid 88213 · kitty</span>
quiver config set 3539 runtime.proton=GE-Proton10-25 runtime.sync=fsync \\
  --reason <span class="s">"r12 crashed at startup. Pin 10-25 and go back to fsync."</span></div></div>`, true, `<span class="btn sm">${ic('undo-2', 'sm')}Undo</span>` + copy)
    + ev('22:40', W.cli, `Pulled cloud saves for <b>AC Origins</b> · 5 slots`, 3539, `<div class="mono muted" style="font-size:11.5px">quiver saves pull 3539</div>`, false, copy)
    + ev('02:00', W.sys, `Checked 9 installed games for updates · 1 found`, 0, '', false, '');

  const clients = [
    ['gui', 'Quiver app', 'This window', 'dot'],
    ['cli', 'quiver jobs --watch', 'kitty · pid 90112', 'dot'],
    ['agent', 'claude-code', 'MCP · idle 6 min', 'dot'],
  ].map(([w, n, s]) => `<div class="row" style="gap:10px;padding:8px 0"><span class="dot" style="background:var(--who-${w})"></span><div class="col grow" style="gap:1px"><span class="${w === 'cli' ? 'mono' : ''}" style="font-weight:600;font-size:12.5px">${n}</span><span class="muted" style="font-size:11.5px">${s}</span></div></div>`).join('');

  return `<div class="app">${topbar('act')}<div class="split">
    <main class="pane" style="min-height:0">
      <div style="padding:22px 28px 14px" class="col">
        <div class="row" style="gap:12px"><div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:22px">Activity</div><span class="muted">Everything that changed, and who changed it</span><span class="grow"></span><span class="btn">${ic('file-down', 'sm')}Export JSON</span></div>
        <div class="row" style="gap:10px;margin-top:14px">
          <div class="seg"><span class="on">Everyone</span><span>${ic('mouse-pointer-2', 'xs')}App</span><span>${ic('square-terminal', 'xs')}CLI</span><span>${ic('bot', 'xs')}Agents</span><span>${ic('cog', 'xs')}System</span></div>
          <span class="btn sm">All games${ic('chevron-down', 'sm')}</span><span class="btn sm">Settings, installs, launches, saves${ic('chevron-down', 'sm')}</span>
          <span class="grow"></span><span class="field" style="width:220px">${ic('search')}<span class="muted">Search</span></span>
        </div>
      </div>
      <div style="margin:0 28px 12px;padding:14px 16px;border-radius:12px;background:linear-gradient(90deg,rgba(195,139,255,.14),rgba(195,139,255,.04));border:1px solid rgba(195,139,255,.35)" class="row">
        <div style="width:36px;height:36px;border-radius:10px;background:rgba(195,139,255,.2);color:var(--who-agent);display:grid;place-items:center">${ic('shield-question')}</div>
        <div class="col grow" style="gap:2px;margin-left:12px"><div style="font-weight:650">claude-code wants to uninstall Watch Dogs 2</div><div class="dim" style="font-size:12.5px">Deletes 47.6 GB and the prefix. Saves stay in the cloud and in backups. Your policy asks before agents uninstall.</div></div>
        <span class="btn">Deny</span><span class="btn pri" style="margin-left:8px">Allow once</span>
      </div>
      <div class="scroll" style="flex:1;margin:0 28px 20px;border:1px solid var(--line-1);border-radius:12px;background:var(--ink-2)">${list.replace('border-top:1px solid var(--line-1)', 'border-top:0')}</div>
    </main>
    <aside style="width:300px;flex:none;border-left:1px solid var(--line-1);padding:22px 20px;display:flex;flex-direction:column;gap:18px;background:var(--ink-0)">
      <div class="col" style="gap:4px"><span class="cap">Connected now</span>${clients}</div>
      <div class="sep"></div>
      <div class="col" style="gap:10px"><span class="cap">This week</span>
        ${[['App', 'gui', 34], ['CLI', 'cli', 12], ['Agents', 'agent', 9], ['System', 'sys', 41]].map(([n, w, c]) => `<div class="row" style="gap:10px;font-size:12px"><span class="dim" style="width:52px">${n}</span><div class="grow bar" style="height:6px"><i style="width:${c * 2.2}%;background:var(--who-${w})"></i></div><span class="num muted" style="width:22px;text-align:right">${c}</span></div>`).join('')}
      </div>
      <div class="sep"></div>
      <div class="col" style="gap:8px;font-size:12px"><span class="cap">Undo works for</span><span class="dim">Launch options, mod profiles, collections, cloud-save choices and settings. Installs and uninstalls are not undoable; agents need your approval for those.</span></div>
      <div class="cli"><span class="p">$</span><span class="t">quiver activity --actor agent</span>${ic('copy', 'xs')}</div>
    </aside></div></div>`;
}

/* ---------- Import wizard ---------- */
function scrImport() {
  const rows = [
    [720, 'Lutris · ubisoft-connect', '~/Games/ubisoft-connect/drive_c/…/Assassin\'s Creed Unity', 'ok', 'Matches latest', 'In place', '6 slots'],
    [1803, 'Lutris · ubisoft-connect', '~/Games/ubisoft-connect/drive_c/…/Far Cry 5', 'warn', '1.3 GB update', 'In place', '11 slots'],
    [2688, 'Steam · non-Steam shortcut', '~/.local/share/Steam/steamapps/compatdata/3127…/pfx/…/WATCH_DOGS2', 'acc', 'Verify needed', 'Move to library', '3 slots'],
    [420, 'Folder', '/mnt/hdd/Games/Far Cry 4', 'mut', 'Unknown version', 'Copy', 'none found'],
  ].map(([id, src, path, st, stt, act, sv], i) => `<tr class="${i === 0 ? 'sel' : ''}">
      <td><span class="check on">${ic('check')}</span></td>
      <td><div class="row" style="gap:10px"><img src="${A(id, 'thumb')}" alt="" style="width:30px;height:35px;border-radius:5px;object-fit:cover"><div class="col" style="gap:1px"><span style="font-weight:600">${nm(id)}</span><span class="muted" style="font-size:11.5px">${src}</span></div></div></td>
      <td class="mono muted" style="font-size:11px;max-width:300px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${path}</td>
      <td><span class="pill ${st}">${stt}</span></td>
      <td><span class="field" style="height:28px;width:150px">${act}<span class="grow"></span>${ic('chevron-down')}</span></td>
      <td class="dim">${sv}</td></tr>`).join('');
  return `<div class="app">${topbar('lib')}
    <div style="position:absolute;inset:52px 0 0 0;background:var(--ink-1);display:flex;flex-direction:column">
      <div class="row" style="padding:22px 40px;gap:14px;border-bottom:1px solid var(--line-1)">
        <div class="col" style="gap:2px"><div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:22px">Import existing games</div><div class="muted">Bring in installs from Ubisoft Connect under Lutris, Steam or a plain folder. Sources are never changed.</div></div>
        <span class="grow"></span>
        <div class="row" style="gap:8px;font-size:12.5px">
          <span class="row" style="gap:6px;color:var(--ok)">${ic('circle-check', 'sm')}Scan</span><span style="width:30px;height:1px;background:var(--line-2)"></span>
          <span class="row" style="gap:6px;color:var(--acc-hi);font-weight:600">${ic('circle-dot', 'sm')}Choose</span><span style="width:30px;height:1px;background:var(--line-2)"></span>
          <span class="row muted" style="gap:6px">${ic('circle', 'sm')}Import</span>
        </div>
      </div>
      <div class="scroll" style="flex:1;padding:22px 40px;display:flex;flex-direction:column;gap:18px">
        <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:14px">
          ${[['Lutris', 'ubisoft-connect · wine-ge-8-26', '2 games · Connect 152.0', 'wine'], ['Steam', 'Ubisoft Connect shortcut · Proton 9.0', '1 game', 'gamepad-2'], ['Folder', '/mnt/hdd/Games', '1 game, no prefix', 'folder']].map(([n, s, c, i]) => `<div class="card card-b row" style="gap:12px"><div style="width:38px;height:38px;border-radius:10px;background:var(--ink-4);display:grid;place-items:center">${ic(i)}</div><div class="col grow" style="gap:2px"><span style="font-weight:650">${n}</span><span class="muted" style="font-size:11.5px">${s}</span></div><span class="pill mut">${c}</span></div>`).join('')}
        </div>
        <div class="card"><table class="tbl"><thead><tr><th></th><th>Game</th><th>Found at</th><th>Against Ubisoft manifest</th><th>Files</th><th>Saves</th></tr></thead><tbody>${rows}</tbody></table></div>
        <div style="display:grid;grid-template-columns:1fr 1fr;gap:14px">
          <div class="card card-b col" style="gap:10px">
            <div style="font-weight:650">Prefixes</div>
            <div class="row" style="gap:10px;font-size:12.5px"><span style="width:16px;height:16px;border-radius:50%;border:5px solid var(--acc);display:block"></span><span>Create a fresh prefix per game</span><span class="pill acc">Recommended</span></div>
            <div class="row" style="gap:10px;font-size:12.5px"><span style="width:16px;height:16px;border-radius:50%;border:1.5px solid var(--tx-4);display:block"></span><span class="dim">Clone the Lutris prefix for each game (keeps registry tweaks)</span></div>
          </div>
          <div class="card card-b col" style="gap:10px">
            <div style="font-weight:650">Saves</div>
            <div class="row" style="gap:10px;font-size:12.5px"><span class="check on">${ic('check')}</span><span>Copy save files found in the old prefixes into backups</span></div>
            <div class="row" style="gap:10px;font-size:12.5px"><span class="check on">${ic('check')}</span><span>Compare with cloud saves before the first launch</span></div>
          </div>
        </div>
      </div>
      <div class="row" style="padding:14px 40px;border-top:1px solid var(--line-1);gap:12px;background:var(--ink-0)">
        <span class="dim">4 games · 2 in place, 1 moved (47.6 GB), 1 copied (40.1 GB) · then verify</span><span class="grow"></span>
        <span class="cli" style="width:300px"><span class="p">$</span><span class="t">quiver import --from lutris --dry-run</span></span>
        <span class="btn">Back</span><span class="btn pri">Import 4 games</span></div>
    </div></div>`;
}

/* ---------- Now playing ---------- */
function scrNowPlaying() {
  return `<div class="app" style="background:#000">
    <img src="${A(3539, 'hero')}" alt="" style="position:absolute;inset:0;width:100%;height:100%;object-fit:cover;filter:brightness(.55) saturate(1.1)">
    <div style="position:absolute;left:28px;top:24px;font-family:var(--f-mono);font-size:12px;line-height:1.5;color:#e8ffe8;background:rgba(0,0,0,.45);padding:8px 10px;border-radius:6px"><span style="color:#9fe6c3">GPU</span> 61% 64°C<br><span style="color:#9fe6c3">CPU</span> 38% 71°C<br><span style="color:#f2c38b">FPS</span> 118 · 8.4 ms</div>
    <div class="glass" style="position:absolute;right:28px;top:24px;width:380px;border-radius:12px;padding:12px 14px;display:flex;gap:12px;align-items:flex-start;background:rgba(16,19,26,.86)">
      <div style="width:32px;height:32px;border-radius:8px;background:var(--ok-soft);color:var(--ok);display:grid;place-items:center;flex:none">${ic('cloud', 'sm')}</div>
      <div class="col grow" style="gap:2px"><div style="font-weight:650">Saves are up to date</div><div class="dim" style="font-size:12px">Pulled 5 slots from the cloud before launch. Backup taken first.</div><div class="row" style="gap:12px;margin-top:6px;font-size:12px;color:var(--acc-hi)"><span>View saves</span></div></div>
      <span class="muted">${ic('x', 'sm')}</span>
    </div>
    <div style="position:absolute;right:28px;bottom:28px;width:400px;border-radius:16px;overflow:hidden;background:var(--ink-1);border:1px solid var(--line-2);box-shadow:0 30px 80px rgba(0,0,0,.7)">
      <div style="position:relative;height:92px;overflow:hidden">
        <img src="${A(3539, 'hero')}" alt="" style="position:absolute;inset:0;width:100%;height:100%;object-fit:cover;object-position:50% 20%">
        <div style="position:absolute;inset:0;background:linear-gradient(180deg,rgba(15,18,24,.1),var(--ink-1))"></div>
        <div class="row" style="position:absolute;left:14px;right:10px;top:10px;gap:6px"><span class="pill ok" style="background:rgba(10,20,14,.7)"><span class="dot"></span>Playing</span><span class="grow"></span><span class="iconbtn glass" style="width:26px;height:26px;border-radius:6px">${ic('maximize-2', 'xs')}</span></div>
      </div>
      <div style="padding:2px 16px 16px;display:flex;flex-direction:column;gap:12px">
        <div class="row" style="gap:10px;align-items:baseline"><span style="font-weight:700;font-size:16px">Assassin's Creed Origins</span><span class="grow"></span><span class="num" style="font-size:20px;font-weight:700;font-family:var(--f-mono)">1:12:08</span></div>
        <div class="row" style="gap:6px;flex-wrap:wrap"><span class="chip" style="height:22px">${ic('layers', 'xs')}Vanilla</span><span class="chip" style="height:22px">${ic('cpu', 'xs')}GE-Proton10-25 · r14</span><span class="chip" style="height:22px">${ic('monitor', 'xs')}gamescope HDR</span></div>
        <div class="col" style="gap:6px;font-size:12px">
          ${[['Ubisoft bridge', 'Connected · 3 pipes', 'ok'], ['Denuvo activation', 'Token valid', 'ok'], ['Ticket refresh', 'Next in 41 min', 'ok'], ['Cloud saves', 'Upload when the game exits', 'acc']].map(([k, v, s]) => `<div class="row" style="gap:8px"><span class="dot ${s === 'ok' ? '' : 'acc'}"></span><span class="dim" style="width:130px">${k}</span><span class="muted">${v}</span></div>`).join('')}
        </div>
        <div class="row" style="gap:8px"><span class="btn danger" style="flex:1">${ic('square', 'sm')}Stop game</span><span class="btn" style="flex:1">${ic('scroll-text', 'sm')}Live log</span><span class="iconbtn bd">${ic('ellipsis', 'sm')}</span></div>
      </div>
    </div></div>`;
}

/* ---------- Command palette ---------- */
function scrPalette() {
  const item = (icn, t, sub, cli, on, kb = '') => `<div class="row" style="gap:12px;padding:9px 14px;border-radius:8px;margin:0 6px;${on ? 'background:var(--acc-soft)' : ''}">
    <span style="color:${on ? 'var(--acc-hi)' : 'var(--tx-3)'}">${ic(icn, 'sm')}</span>
    <div class="col grow" style="gap:1px"><span style="font-weight:${on ? 600 : 500}">${t}</span>${sub ? `<span class="muted" style="font-size:11.5px">${sub}</span>` : ''}</div>
    <span class="mono" style="font-size:11px;color:${on ? 'var(--who-cli)' : 'var(--tx-4)'};max-width:280px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${cli}</span>${kb}</div>`;
  const overlay = `<div class="scrim"></div><div class="palette">
    <div class="row" style="gap:10px;padding:14px 16px;border-bottom:1px solid var(--line-1)">${ic('search')}<span style="font-size:15px">origins proton<span style="display:inline-block;width:2px;height:18px;background:var(--acc);vertical-align:-3px;margin-left:1px"></span></span><span class="grow"></span><span class="pill mut">AC Origins</span></div>
    <div style="padding:8px 0">
      <div class="cap" style="padding:6px 20px">Assassin's Creed Origins</div>
      ${item('cpu', 'Set Proton build…', 'Now GE-Proton10-25 (pinned)', 'quiver config set 3539 runtime.proton=…', 1, '<span class="kbd">↵</span>')}
      ${item('rotate-ccw', 'Restore launch options to r13', 'Known good · Nov 12', 'quiver config restore 3539 r13', 0)}
      ${item('git-compare', 'Compare launch options r12 and r14', '', 'quiver config diff 3539 r12 r14', 0)}
      ${item('flask-conical', 'Launch once with GE-Proton10-26', 'Does not save a revision', 'quiver play 3539 --with runtime.proton=GE-Proton10-26', 0)}
      ${item('camera', 'Snapshot prefix', '~/Games/ubi-prefixes/3539', 'quiver prefix snapshot 3539', 0)}
      <div class="cap" style="padding:10px 20px 6px">Everywhere</div>
      ${item('download', 'Install GE-Proton10-26', 'Latest release, Nov 13', 'quiver runtime install GE-Proton10-26', 0)}
      ${item('settings', 'Open Settings › Compatibility', '', '', 0, '<span class="kbd">Ctrl ,</span>')}
    </div>
    <div class="row" style="gap:16px;padding:10px 16px;border-top:1px solid var(--line-1);font-size:11.5px;background:var(--ink-1)" class="muted">
      <span class="row muted" style="gap:6px"><span class="kbd">↑↓</span>Move</span><span class="row muted" style="gap:6px"><span class="kbd">↵</span>Run</span><span class="row muted" style="gap:6px"><span class="kbd">Tab</span>Fill arguments</span><span class="row muted" style="gap:6px"><span class="kbd">Ctrl C</span>Copy as quiver command</span><span class="grow"></span><span class="muted">Every entry is a daemon call</span></div>
  </div>`;
  const html = scrGame();
  return html.replace(/<\/div>$/, overlay + '</div>');
}
