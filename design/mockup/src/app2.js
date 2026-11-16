/* ---------- Launch options with history ---------- */
function frow(label, control, opts = {}) {
  return `<div class="row" style="gap:16px;padding:9px 0;border-top:1px solid var(--line-1);align-items:${opts.top ? 'flex-start' : 'center'}">
    <div style="width:190px;flex:none;display:flex;flex-direction:column;gap:2px">
      <div class="row" style="gap:6px;font-weight:550;font-size:12.5px">${opts.changed ? '<span class="changed"></span>' : ''}${label}</div>
      ${opts.help ? `<div class="muted" style="font-size:11.5px;line-height:1.35">${opts.help}</div>` : ''}
    </div>
    <div class="grow row" style="gap:8px;white-space:nowrap">${control}</div>
    ${opts.inh ? `<span class="inh">${ic('corner-left-up', 'xs')}${opts.inh}</span>` : ''}
  </div>`;
}
function section(title, sub, rows, right = '') {
  return `<div class="card" style="padding:4px 16px 8px">
    <div class="row" style="padding:10px 0 8px"><span style="font-weight:650;font-size:13.5px">${title}</span><span class="muted" style="font-size:12px">${sub}</span><span class="grow"></span>${right}</div>${rows}</div>`;
}

function scrLaunch() {
  const id = 3539;
  const env = [
    ['DXVK_HDR', '1', 1, 'game'],
    ['ENABLE_HDR_WSI', '1', 1, 'game', 1],
    ['PROTON_ENABLE_WAYLAND', '0', 1, 'global'],
    ['PROTON_LOG', '1', 0, 'game'],
  ].map(([k, v, on, src, ch]) => `<div class="row" style="gap:8px;width:100%">
      <span class="check${on ? ' on' : ''}">${on ? ic('check') : ''}</span>
      <span class="field mono" style="width:220px;height:28px">${k}</span><span class="muted">=</span>
      <span class="field mono grow" style="height:28px">${v}</span>
      ${ch ? '<span class="changed"></span>' : ''}
      <span class="inh" style="width:64px">${src === 'global' ? 'Global' : ''}</span></div>`).join('');

  const form = `<div class="col scroll" style="gap:14px;padding:18px 22px 90px 28px;flex:1">
    <div class="row" style="gap:10px">
      <div class="seg"><span class="on">${ic('gamepad-2', 'xs')}This game</span><span>${ic('layers', 'xs')}Profile: Vanilla</span><span>${ic('globe', 'xs')}Global defaults</span></div>
      <span class="grow"></span>
      <span class="row muted" style="font-size:12px;gap:8px"><span class="toggle on"></span>Show inherited values</span>
    </div>
    ${section('Runtime', 'Wine/Proton build and translation layers', [
      frow('Compatibility tool', `<span class="field" style="width:250px">${ic('cpu')}<b style="font-weight:550">GE-Proton10-25</b><span class="grow"></span><span class="muted">Steam</span>${ic('chevron-down')}</span><span class="pill acc">${ic('pin', 'xs')}Pinned</span><span class="muted" style="font-size:12px">global default is GE-Proton10-26</span>`, { help: 'Changing it upgrades the prefix. Quiver snapshots the prefix first.' }),
      frow('DirectX translation', `<span class="field" style="width:180px">DXVK 2.7.1<span class="grow"></span>${ic('chevron-down')}</span><span class="field" style="width:200px">VKD3D-Proton 2.14.1<span class="grow"></span>${ic('chevron-down')}</span>`, { inh: 'Proton' }),
      frow('Sync primitive', `<div class="seg"><span>NTSync</span><span class="on">fsync</span><span>esync</span><span>Off</span></div><span class="muted" style="font-size:12px">NTSync crashed at startup in r12</span>`, { help: 'Global default is NTSync' }),
      frow('Upscaling', `<div class="seg"><span class="on">Off</span><span>FSR 1 (gamescope)</span><span>FSR 4 (Proton)</span></div>`, { inh: 'Global' }),
    ].join(''))}
    ${section('Wrappers', 'Run in this order around the game', [
      `<div class="row" style="gap:6px;padding:8px 0 10px;font-size:12px">${['gamemoderun', 'gamescope', 'mangohud', 'proton', 'ACOrigins.exe'].map((w, i, a) => `<span class="pill ${i === a.length - 1 ? 'acc' : 'mut'}" style="font-family:var(--f-mono);font-weight:500">${w}</span>${i < a.length - 1 ? `<span class="muted">${ic('chevron-right', 'xs')}</span>` : ''}`).join('')}</div>`,
      frow('gamescope', `<span class="toggle on"></span><span class="field mono grow focus">-W 3440 -H 1440 -r 144 --hdr-enabled -f</span>`, { changed: 1, help: 'Micro-compositor; HDR and frame limiting' }),
      frow('MangoHud', `<span class="toggle on"></span><span class="field" style="width:220px">Preset: FPS + frame time<span class="grow"></span>${ic('chevron-down')}</span><span class="kbd">Shift F12</span>`, { inh: 'Global' }),
      frow('GameMode', `<span class="toggle on"></span><span class="muted" style="font-size:12px">gamemoded 1.8.2 found</span>`, { inh: 'Global' }),
    ].join(''))}
    ${section('Environment', '4 variables · 1 from global defaults', env, `<span class="btn sm ghost">${ic('plus', 'sm')}Add variable</span>`)}
    ${section('Game', 'From the Ubisoft product config, plus your additions', [
      frow('Executable', `<span class="field mono" style="width:260px">ACOrigins.exe<span class="grow"></span>${ic('chevron-down')}</span><span class="muted" style="font-size:12px">or ACOrigins_plus.exe (Ubisoft+)</span>`),
      frow('Arguments', `<span class="chip mono" style="font-family:var(--f-mono)">${ic('lock', 'xs')}/appid:{AppId}</span><span class="field mono grow"><span class="muted">Extra arguments</span></span>`, { help: 'Ubisoft-supplied arguments are locked' }),
      frow('Online mode', `<div class="seg"><span class="on">Online</span><span>Offline</span></div><span class="muted" style="font-size:12px">Offline needs a recent online launch</span>`),
      frow('Pre-launch hook', `<span class="field mono grow"><span class="muted">~/bin/fan-curve performance</span></span>`, { help: 'Run on host before and after the session' }),
    ].join(''))}
    <div class="card" style="padding:12px 16px">
      <div class="row" style="margin-bottom:8px"><span style="font-weight:650">Resolved command</span><span class="muted" style="font-size:12px">what quiverd will run</span><span class="grow"></span><span class="btn sm">${ic('copy', 'sm')}Copy</span><span class="btn sm">${ic('square-terminal', 'sm')}Copy as quiver command</span></div>
      <div class="code"><span class="k">DXVK_HDR</span>=<span class="s">1</span> <span class="k">ENABLE_HDR_WSI</span>=<span class="s">1</span> <span class="k">PROTON_ENABLE_WAYLAND</span>=<span class="s">0</span> <span class="k">WINEPREFIX</span>=<span class="s">~/Games/ubi-prefixes/3539</span> <span class="k">UPLAY_ARGUMENTS</span>=<span class="s">"-upc_uplay_id 3539 …"</span>
gamemoderun gamescope -W 3440 -H 1440 -r 144 --hdr-enabled -f -- mangohud \\
  <span class="v">~/.local/share/Steam/compatibilitytools.d/GE-Proton10-25/proton</span> run ACOrigins.exe /appid:3539</div>
    </div>
  </div>`;

  const pending = `<div class="row" style="position:absolute;left:24px;right:404px;bottom:16px;z-index:3;gap:10px;padding:10px 12px 10px 16px;border-radius:12px;background:var(--ink-3);border:1px solid var(--line-2);box-shadow:0 14px 40px rgba(0,0,0,.55)">
    <span class="changed"></span><span style="font-weight:600;white-space:nowrap">2 unsaved changes</span><span class="muted mono" style="font-size:11.5px">wrappers.gamescope.args, env.ENABLE_HDR_WSI</span><span class="grow"></span>
    <span class="btn ghost sm">Discard</span><span class="btn sm">${ic('flask-conical', 'sm')}Test launch</span><span class="btn sm pri">Save as r15</span></div>`;

  const H = [
    { r: 'Unsaved', t: '2 changes', who: '', when: 'now', node: 'dash', ico: 'pencil' },
    { r: 'r14', t: 'Turn on gamescope with HDR', who: '<span class="who gui">' + ic('mouse-pointer-2', 'xs') + 'You · app</span>', when: '2 h ago', node: 'cur', ico: 'check', res: ['ok', '2 h 14 m'], cur: 1 },
    { r: 'r13', t: 'Pin GE-Proton10-25, back to fsync', who: '<span class="who agent">' + ic('bot', 'xs') + 'claude-code</span>', when: 'Nov 12', node: 'ok', ico: 'check', res: ['ok', '1 h 05 m'], tag: 'Known good', sel: 1 },
    { r: 'r12', t: 'Sync: fsync to NTSync', who: '<span class="who term">' + ic('square-terminal', 'xs') + 'You · CLI</span>', when: 'Nov 10', node: 'bad', ico: 'x', res: ['bad', 'crashed after 4 m'] },
    { r: 'r11', t: 'Before update 1.5.1 to 1.6.0', who: '<span class="who sys">' + ic('cog', 'xs') + 'System</span>', when: 'Nov 4', node: '', ico: 'camera', snap: 'Prefix snapshot · 1.9 GB' },
    { r: 'r10', t: 'MangoHud preset FPS + frame time', who: '<span class="who gui">' + ic('mouse-pointer-2', 'xs') + 'You · app</span>', when: 'Oct 21', node: 'ok', ico: 'check', res: ['ok', '9 sessions'] },
    { r: 'r1', t: 'Created from global defaults', who: '<span class="who sys">' + ic('cog', 'xs') + 'System</span>', when: 'Oct 2', node: '', ico: 'circle-dot' },
  ].map((h) => `<div class="tl-item"><div class="tl-node ${h.node}">${ic(h.ico)}</div>
    <div style="padding:${h.sel ? '10px 12px' : '0'};${h.sel ? 'background:var(--ink-3);border:1px solid var(--acc-line);border-radius:10px;margin-top:-6px' : ''}">
      <div class="row" style="gap:6px"><span class="mono" style="font-weight:600;color:${h.cur ? 'var(--acc-hi)' : 'var(--tx-1)'}">${h.r}</span>${h.cur ? '<span class="pill acc">Current</span>' : ''}${h.tag ? `<span class="pill ok">${ic('pin', 'xs')}${h.tag}</span>` : ''}<span class="grow"></span><span class="muted" style="font-size:11.5px">${h.when}</span></div>
      <div style="font-size:12.5px;margin-top:2px">${h.t}</div>
      ${h.who || h.res || h.snap ? `<div class="row" style="gap:6px;margin-top:5px;flex-wrap:wrap">${h.who}${h.res ? `<span class="row" style="gap:4px;font-size:11.5px;color:${h.res[0] === 'ok' ? 'var(--tx-3)' : 'var(--bad)'}">${ic(h.res[0] === 'ok' ? 'play' : 'circle-x', 'xs')}${h.res[1]}</span>` : ''}${h.snap ? `<span class="row muted" style="gap:4px;font-size:11.5px">${ic('hard-drive', 'xs')}${h.snap}</span>` : ''}</div>` : ''}
      ${h.sel ? `<div style="margin-top:10px" class="col">
        <div class="muted" style="font-size:11.5px;margin-bottom:6px">“r12 crashed at startup. Pin 10-25 and go back to fsync.” · <span class="mono">quiver config set</span> via token claude-code</div>
        <div class="diff"><div class="h">r13 → current (r14 + unsaved)</div><div class="r">- wrappers.gamescope.enabled = false</div><div class="a">+ wrappers.gamescope.enabled = true</div><div class="a">+ wrappers.gamescope.args = "-W 3440 -H 1440 -r 144 --hdr-enabled -f"</div><div class="a">+ env.DXVK_HDR = "1"</div><div class="a">+ env.ENABLE_HDR_WSI = "1"</div></div>
        <div class="row" style="gap:6px;margin-top:10px"><span class="btn sm pri">${ic('rotate-ccw', 'sm')}Restore r13</span><span class="btn sm">${ic('play', 'sm')}Launch once</span><span class="iconbtn" style="width:26px;height:26px">${ic('copy', 'sm')}</span></div>
      </div>` : ''}
    </div></div>`).join('');

  const hist = `<aside style="width:380px;flex:none;border-left:1px solid var(--line-1);background:color-mix(in srgb, var(--ink-0) 55%, transparent);display:flex;flex-direction:column;min-height:0">
    <div class="row" style="padding:14px 18px 10px;gap:8px">${ic('history')}<span style="font-weight:650">History</span><span class="badge">14 revisions</span><span class="grow"></span><div class="seg"><span class="on">All</span><span>${ic('pin', 'xs')}</span></div></div>
    <div class="row" style="padding:0 18px 12px;gap:6px"><span class="btn sm" style="flex:1">${ic('git-compare', 'sm')}Compare…</span><span class="btn sm" style="flex:1">${ic('rotate-ccw', 'sm')}Last known good</span></div>
    <div class="scroll" style="padding:4px 18px 18px;flex:1"><div class="tl-list">${H}</div></div>
  </aside>`;

  const inner = `<div class="split" style="position:relative"><div class="col" style="flex:1;min-width:0;min-height:0">${form}</div>${hist}${pending}</div>`;
  return detailShell(id, 'Launch options', inner, { heroRight: `<span class="chip">${ic('cpu', 'xs')}r14 · GE-Proton10-25</span>` });
}

/* ---------- Mods / overlays ---------- */
function scrMods() {
  const id = 5059;
  const prof = [
    ['Vanilla', 'Base game only', 'Cloud saves', 0, 'shield-check'],
    ['Visual overhaul', '3 of 4 layers on', 'Own saves', 1, 'sparkles'],
    ['Photo mode', '2 layers', 'Own saves', 0, 'camera'],
  ].map(([n, s, sv, on, i]) => `<div style="flex:1;padding:12px 14px;border-radius:12px;border:1px solid ${on ? 'var(--acc-line)' : 'var(--line-2)'};background:${on ? 'var(--acc-soft)' : 'var(--ink-2)'};display:flex;gap:12px;align-items:center">
      <div style="width:34px;height:34px;border-radius:9px;display:grid;place-items:center;background:${on ? 'var(--acc)' : 'var(--ink-4)'};color:${on ? 'var(--acc-ink)' : 'var(--tx-2)'}">${ic(i, 'sm')}</div>
      <div class="col grow" style="gap:2px"><div class="row" style="gap:6px;font-weight:650">${n}${n === 'Vanilla' ? '<span class="pill mut">Default</span>' : ''}</div><div class="muted" style="font-size:11.5px">${s}</div></div>
      ${on ? '' : `<span class="row muted" style="gap:4px;font-size:11px">${ic(sv === 'Cloud saves' ? 'cloud' : 'hard-drive', 'xs')}${sv}</span>`}${on ? `<span class="muted">${ic('pin', 'xs')}</span><span class="iconbtn" style="width:26px;height:26px;background:var(--ink-4);color:var(--tx-1)">${ic('ellipsis', 'sm')}</span>` : ''}</div>`).join('');
  const st = (cls, t) => `<span class="pill ${cls}">${t}</span>`;
  const steamDlg = `<div class="menu" style="left:0;top:120px;width:430px;padding:18px 20px;z-index:7;box-shadow:0 30px 80px rgba(0,0,0,.6)">
      <div class="row" style="gap:12px;align-items:flex-start">
        <div style="width:36px;height:36px;border-radius:10px;background:var(--warn-soft);color:var(--warn);display:grid;place-items:center;flex:none">${ic('refresh-cw', 'sm')}</div>
        <div class="col" style="gap:6px"><div style="font-weight:700;font-size:14.5px">Restart Steam now to show the new shortcut?</div>
          <div class="dim" style="font-size:12.5px">Steam is running. Quiver will close it cleanly, add <b style="color:var(--tx-1)">AC Odyssey · Visual overhaul</b>, and start Steam again. No game is running in Steam.</div>
          <div class="muted" style="font-size:11.5px">Not now keeps the shortcut staged. It appears the next time Steam restarts.</div></div>
      </div>
      <div class="row" style="gap:8px;justify-content:flex-end;margin-top:16px"><span class="btn">Not now</span><span class="btn pri">Restart Steam</span></div>
    </div>`;
  const menu = `<div class="menu" style="left:calc(33.3% + 120px);top:62px">
      <div class="row" style="padding:6px 10px 8px;gap:8px"><span style="font-weight:650">Visual overhaul</span><span class="mono muted" style="font-size:11px">id p7f3k2q</span></div>
      <div class="sep"></div>
      <div class="mi">${ic('pin', 'sm')}<span class="grow">Pin to Library</span>${st('ok', 'Pinned')}</div>
      <div class="mi">${ic('app-window', 'sm')}<span class="grow col" style="gap:1px"><span>Create .desktop entry</span><span class="mono muted" style="font-size:10.5px">quiver-5059-p7f3k2q.desktop</span></span>${st('ok', 'Created')}</div>
      <div class="mi on">${ic('gamepad-2', 'sm')}<span class="grow col" style="gap:1px"><span>Add to Steam</span><span class="muted" style="font-size:11px">Steam user ajg · staged until Steam restarts</span></span>${st('warn', 'Waiting for Steam')}</div>
      <div class="sep"></div>
      <div class="mi">${ic('image', 'sm')}<span class="grow">Change art…</span><span class="muted" style="font-size:11px">custom</span></div>
      <div class="mi">${ic('pencil', 'sm')}<span class="grow">Rename…</span></div>
      <div class="mi">${ic('copy', 'sm')}<span class="grow">Duplicate</span></div>
      <div class="sep"></div>
      <div class="mi" style="color:var(--bad)">${ic('trash-2', 'sm')}<span class="grow">Delete profile…</span></div>
    </div>`;

  const L = [
    { c: '#c38bff', n: 'ReShade 6.3.3 + “Aegean Light” preset', m: '41 files · 84 MB · archive', on: 1, note: `${ic('wand-sparkles', 'xs')}Adds <span class="mono">WINEDLLOVERRIDES=dxgi=n,b</span> to this profile` },
    { c: '#7b93ff', n: 'HD texture pack (community) v2.1', m: '1,204 files · 12.8 GB · archive', on: 1, sel: 1, warn: 'Overrides 3 files from Better water' },
    { c: '#4cc9c0', n: 'Better water and weather', m: '17 files · 640 MB · folder', on: 1 },
    { c: '#f0b44c', n: 'Skip intro videos', m: '6 files · 2 KB · folder', on: 0 },
  ].map((l, i) => `<div class="row" style="gap:12px;padding:12px 14px;border-radius:10px;background:${l.sel ? 'var(--ink-3)' : 'var(--ink-2)'};border:1px solid ${l.sel ? 'var(--acc-line)' : 'var(--line-1)'};opacity:${l.on ? 1 : 0.6}">
      <span class="muted">${ic('grip-vertical', 'sm')}</span>
      <span class="check${l.on ? ' on' : ''}">${l.on ? ic('check') : ''}</span>
      <span style="width:4px;height:30px;border-radius:2px;background:${l.c};flex:none"></span>
      <div class="col grow" style="gap:3px"><div style="font-weight:600">${l.n}</div><div class="row muted" style="font-size:11.5px;gap:10px"><span class="num">${l.m}</span>${l.note ? `<span class="row" style="gap:4px;color:var(--who-agent)">${l.note}</span>` : ''}${l.warn ? `<span class="row" style="gap:4px;color:var(--warn)">${ic('triangle-alert', 'xs')}${l.warn}</span>` : ''}</div></div>
      <span class="mono muted" style="font-size:11px">#${4 - i}</span></div>`).join('');

  const stack = `<div class="col" style="gap:8px">
      <div class="row"><span class="cap">Layer stack · top wins</span><span class="grow"></span><span class="btn sm">${ic('plus', 'sm')}Add layer</span></div>
      ${L}
      <div class="row" style="gap:12px;padding:10px 14px;border-radius:10px;border:1px dashed var(--line-2)">
        <span style="width:16px"></span><span class="muted">${ic('package', 'sm')}</span>
        <div class="col grow" style="gap:2px"><div style="font-weight:600">Ubisoft add-ons</div><div class="muted" style="font-size:11.5px">The Fate of Atlantis, Legacy of the First Blade · part of the base install, owned</div></div><span class="pill mut">Always on</span></div>
      <div class="row" style="gap:12px;padding:12px 14px;border-radius:10px;background:repeating-linear-gradient(135deg,var(--ink-2) 0 8px,var(--ink-1) 8px 16px);border:1px solid var(--line-1)">
        <span style="width:16px"></span><span class="muted">${ic('lock', 'sm')}</span>
        <div class="col grow" style="gap:2px"><div style="font-weight:600">Base install · 1.5.6</div><div class="muted" style="font-size:11.5px">~/Games/ubi-library/Assassin's Creed Odyssey · 112.4 GB · never modified</div></div><span class="pill mut">Read-only</span></div>
      <div class="row" style="gap:10px;padding:10px 14px;margin-top:4px;border-radius:10px;background:var(--ink-0);border:1px solid var(--line-1);font-size:12px">
        ${ic('pen-line', 'sm')}<span class="dim">Files the game writes go to this profile's own layer</span><span class="mono muted">312 MB</span><span class="grow"></span><span class="btn sm ghost">Inspect</span><span class="btn sm ghost">Reset</span></div>
    </div>`;

  const insp = `<div class="card" style="display:flex;flex-direction:column">
      <div class="card-h"><span style="width:4px;height:18px;border-radius:2px;background:#7b93ff"></span><h4>HD texture pack (community) v2.1</h4></div>
      <div class="card-b col" style="gap:12px">
        <dl class="kv" style="margin:0">
          <dt>Source</dt><dd class="mono">~/Downloads/odyssey-hd-2.1.7z</dd>
          <dt>Added</dt><dd>Nov 2 · by You (app)</dd>
          <dt>Built for</dt><dd><span style="color:var(--warn)">1.5.4</span> <span class="muted">· base is 1.5.6</span></dd>
          <dt>Checksum</dt><dd class="mono muted">sha256 9f3c…a07e</dd>
        </dl>
        <div class="sep"></div>
        <div class="cap">Files · 1,204</div>
        <div class="col mono" style="gap:3px;font-size:11.5px">
          ${[['O', 'DataPC_ACOdyssey_textures.forge', '7.1 GB'], ['O', 'DataPC_extra_textures.forge', '4.6 GB'], ['A', 'textures/hd/terrain_greece.dds', '88 MB'], ['O', 'water/ocean_normal_01.dds', '22 MB'], ['O', 'water/ocean_normal_02.dds', '22 MB']].map(([k, f, s]) => `<div class="row" style="gap:8px"><span style="width:16px;text-align:center;border-radius:3px;font-weight:700;font-size:10px;color:${k === 'A' ? 'var(--ok)' : 'var(--warn)'};background:${k === 'A' ? 'var(--ok-soft)' : 'var(--warn-soft)'}">${k}</span><span class="grow dim" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${f}</span><span class="muted num">${s}</span></div>`).join('')}
          <div class="muted" style="font-size:11px;margin-top:2px">A adds a file · O overrides a lower layer</div>
        </div>
        <div class="sep"></div>
        <div class="row" style="gap:8px;font-size:12px;padding:8px 10px;border-radius:8px;background:var(--warn-soft);color:var(--warn)">${ic('triangle-alert', 'sm')}<span>Overrides 3 water files from <b>Better water</b>. This layer wins because it is higher.</span></div>
        <div class="row" style="gap:6px"><span class="btn sm">${ic('folder-open', 'sm')}Open</span><span class="btn sm">${ic('refresh-cw', 'sm')}Replace…</span><span class="grow"></span><span class="btn sm danger">${ic('trash-2', 'sm')}Remove</span></div>
      </div></div>`;

  const inner = `<div class="body scroll col" style="gap:18px;padding-top:18px">
    <div class="row" style="gap:12px;position:relative;z-index:5">${prof}${menu}${steamDlg}<div style="width:120px;align-self:stretch;border-radius:12px;border:1px dashed var(--line-2);display:grid;place-items:center;color:var(--tx-3)" class="col"><span class="row" style="gap:6px">${ic('plus', 'sm')}New profile</span></div></div>
    <div style="display:grid;grid-template-columns:minmax(0,1fr) 380px;gap:20px">
      ${stack}
      <div class="col" style="gap:14px">${insp}
        <div class="card card-b col" style="gap:10px">
          <div class="row"><span style="font-weight:650">Profile settings</span></div>
          <div class="row" style="font-size:12.5px;gap:10px"><span class="dim grow">Saves</span><div class="seg"><span>Shared</span><span class="on">Own saves</span></div></div>
          <div class="row" style="font-size:12.5px;gap:10px"><span class="dim grow">Launch options</span><span class="muted">r14 + 1 override</span></div>
          <div class="row" style="font-size:12.5px;gap:10px"><span class="dim grow">Sync these saves to the cloud</span><span class="toggle"></span></div>
          <div class="muted" style="font-size:11.5px;margin-top:-4px">Off by default for modded profiles, so a broken mod can't overwrite your cloud saves.</div>
          <div class="cli"><span class="p">$</span><span class="t">quiver play 5059 --profile "visual-overhaul"</span>${ic('copy', 'xs')}</div>
        </div>
      </div>
    </div></div>`;
  return detailShell(id, 'Mods', inner, { heroRight: `<span class="chip on">${ic('layers', 'xs')}Visual overhaul</span>` });
}

/* ---------- Saves & progress ---------- */
function barsSvg(vals, w = 330, h = 96) {
  const max = 8, bw = (w - 24) / vals.length;
  const bars = vals.map((v, i) => {
    const bh = (v / max) * (h - 22);
    return `<rect x="${24 + i * bw + 3}" y="${h - 16 - bh}" width="${bw - 6}" height="${Math.max(bh, 1.5)}" rx="2" style="fill:${i === vals.length - 1 ? 'var(--acc)' : 'color-mix(in srgb, var(--acc) 35%, var(--ink-4))'}"/>`;
  }).join('');
  const grid = [0, 4, 8].map((t) => { const y = h - 16 - (t / max) * (h - 22); return `<line x1="24" x2="${w}" y1="${y}" y2="${y}" style="stroke:var(--line-1)" stroke-width="1"/><text x="0" y="${y + 3.5}" style="fill:var(--tx-3)" font-size="9.5" font-family="Geist, sans-serif">${t} h</text>`; }).join('');
  const labels = `<text x="27" y="${h - 2}" style="fill:var(--tx-3)" font-size="9.5" font-family="Geist, sans-serif">Aug 24</text><text x="${w}" y="${h - 2}" text-anchor="end" style="fill:var(--tx-2)" font-size="9.5" font-family="Geist, sans-serif">This week</text>`;
  return `<svg viewBox="0 0 ${w} ${h}" width="100%" style="display:block;height:auto">${grid}${bars}${labels}</svg>`;
}

function scrSaves() {
  const id = 3539;
  const slots = [
    [0, 'ACO[0]-tm_2026_11_14_21_46_12', 'Nov 14 21:46', '4.2 MB', 'Nov 14 21:46', 'Nov 14 21:47', ['ok', 'In sync']],
    [1, 'ACO[1]-tm_2026_11_14_20_58_40', 'Nov 14 20:58', '4.2 MB', 'Nov 14 20:58', 'Nov 14 21:47', ['ok', 'In sync']],
    [2, 'ACO[2]-tm_2026_11_12_23_10_02', 'Nov 12 23:10', '4.1 MB', 'Nov 12 23:10', 'Nov 12 23:11', ['ok', 'In sync']],
    [3, 'ACO[3]-tm_2026_10_30_18_02_55', 'Oct 30 18:02', '4.0 MB', '—', 'Oct 30 18:03', ['acc', 'Cloud only']],
    [4, 'ACO[4]-tm_2026_11_10_19_34_51', 'Nov 10 19:34', '3.9 MB', 'Nov 10 19:34', '—', ['warn', 'Not uploaded']],
  ].map(([n, raw, when, size, loc, cloud, st], i) => `<tr class="${i === 4 ? 'warnrow' : ''}">
      <td><span class="mono" style="display:inline-grid;place-items:center;width:24px;height:24px;border-radius:6px;background:var(--ink-4);font-weight:600">${n}</span></td>
      <td><div style="font-weight:550">${when}</div><div class="mono muted" style="font-size:11px">${raw}</div></td>
      <td class="num dim">${size}</td>
      <td class="num dim">${loc}</td>
      <td class="num dim">${cloud}</td>
      <td><span class="pill ${st[0]}">${st[1]}</span></td>
      <td style="text-align:right">${i === 3 ? `<span class="btn sm">${ic('cloud-download', 'sm')}Download</span>` : i === 4 ? `<span class="btn sm">${ic('cloud-upload', 'sm')}Upload</span>` : `<span class="iconbtn" style="width:26px;height:26px;display:inline-grid">${ic('ellipsis', 'sm')}</span>`}</td></tr>`).join('');

  const backups = [
    ['Nov 14 21:47', 'After session, 3 pushed', 'gui', 5],
    ['Nov 14 19:33', 'Before session pull', 'sys', 5],
    ['Nov 12 21:07', 'After session, 2 pushed', 'sys', 5],
    ['Nov 10 19:35', 'After crash', 'sys', 5],
    ['Nov 6 10:12', '“before Siwa questline”', 'cli', 4],
  ].map(([w, t, who, n], i) => `<div class="row" style="gap:10px;padding:9px 0;border-top:${i ? '1px solid var(--line-1)' : '0'}">
      <span class="dot ${who === 'cli' ? '' : 'off'}" style="${who === 'cli' ? 'background:var(--who-cli)' : ''}"></span>
      <div class="col grow" style="gap:1px"><span style="font-size:12.5px;font-weight:550" class="num">${w}</span><span class="muted" style="font-size:11.5px;white-space:nowrap">${t} · ${n} slots</span></div>
      <span class="btn sm ghost">${ic('rotate-ccw', 'sm')}Restore</span></div>`).join('');

  const inner = `<div class="body scroll col" style="gap:18px;padding-top:18px">
    <div style="display:grid;grid-template-columns:1.15fr 1fr 1fr;gap:16px">
      <div class="card card-b col" style="gap:8px">
        <div class="row"><span class="cap">Play time</span><span class="grow"></span><span class="pill future">Ubisoft total, not wired yet</span></div>
        <div class="row" style="gap:10px;align-items:baseline"><span style="font-size:24px;font-weight:700" class="num">86 h</span><span class="muted">Ubisoft total · local 87 h 12 m in 41 sessions</span></div>
        ${barsSvg([2.1, 0, 3.4, 5.2, 0, 1.6, 6.8, 4.1, 0, 3.9, 7.4, 6.1])}
        <div class="muted" style="font-size:11.5px">Weekly detail from local sessions · 6 h 05 m this week</div>
      </div>
      <div class="card card-b col" style="gap:10px">
        <div class="row"><span class="cap">Achievements</span><span class="grow"></span><span class="pill future">UbiServices, later</span></div>
        <div class="row" style="gap:14px">
          <svg width="64" height="64" viewBox="0 0 64 64"><circle cx="32" cy="32" r="27" fill="none" style="stroke:var(--ink-4)" stroke-width="7"/><circle cx="32" cy="32" r="27" fill="none" style="stroke:var(--acc)" stroke-width="7" stroke-linecap="round" stroke-dasharray="${(2 * Math.PI * 27 * 0.66).toFixed(1)} 400" transform="rotate(-90 32 32)"/><text x="32" y="37" text-anchor="middle" style="fill:var(--tx-1)" font-size="14" font-weight="700" font-family="Geist, sans-serif">66%</text></svg>
          <div class="col" style="gap:2px"><span style="font-size:18px;font-weight:700" class="num">41 of 62</span><span class="muted" style="font-size:12px">Latest: Nov 14, 21:30</span></div>
        </div>
        <div class="row" style="gap:6px">${achTile('pyramid', 38)}${achTile('eye', 200)}${achTile('swords', 4)}${achTile('ship', 190)}${achTile('', 0, 1)}</div>
      </div>
      <div class="card card-b col" style="gap:10px">
        <div class="row"><span class="cap">Player stats</span><span class="grow"></span><span class="pill future">Not wired yet</span></div>
        ${[['Total progression', '68%', 68], ['Player level', '40', 72], ['Total kills', '3,214', 0], ['Distance travelled', '1,182 km', 0], ['Time played (Ubisoft)', '86 h', 0]].map(([k, v, p]) => `<div class="row" style="font-size:12.5px;gap:10px"><span class="dim grow">${k}</span>${p ? `<div class="bar" style="width:70px"><i style="width:${p}%"></i></div>` : ''}<span class="num" style="font-weight:600;width:64px;text-align:right">${v}</span></div>`).join('')}
      </div>
    </div>
    <div style="display:grid;grid-template-columns:minmax(0,1fr) 300px;gap:16px">
      <div class="card">
        <div class="card-h"><h4>Save slots</h4><span class="pill ok">${ic('cloud', 'xs')}Synced Nov 14, 21:47</span><span class="muted" style="font-size:12px">Pull before launch · push after exit</span><span class="grow"></span><span class="btn sm">${ic('refresh-cw', 'sm')}Sync now</span><span class="btn sm ghost">${ic('folder-open', 'sm')}</span></div>
        <table class="tbl"><thead><tr><th>Slot</th><th>Saved</th><th>Size</th><th>Local</th><th>Cloud</th><th>Status</th><th></th></tr></thead><tbody>${slots}</tbody></table>
        <div class="row" style="gap:10px;margin:0 16px 14px;padding:10px 12px;border-radius:8px;background:var(--warn-soft);font-size:12.5px">${ic('triangle-alert', 'sm')}<span style="color:var(--warn)">Slot 4 was written during the Nov 10 crash and never uploaded.</span><span class="grow"></span><span class="btn sm">Upload</span><span class="btn sm ghost">Keep local only</span></div>
      </div>
      <div class="card">
        <div class="card-h"><h4>Backups</h4><span class="muted" style="font-size:12px">23 kept · 96 MB</span><span class="grow"></span><span class="btn sm">${ic('plus', 'sm')}Back up</span></div>
        <div class="card-b" style="padding-top:4px">${backups}</div>
      </div>
    </div></div>`;
  return detailShell(id, 'Saves & progress', inner, {});
}
