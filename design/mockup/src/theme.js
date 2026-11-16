/* ---------- Omarchy theme -> app tokens ----------
   This is the mapping the GPUI app should implement: read colors.toml, normalise the
   two schemas Omarchy themes use, derive every token, and fix contrast where needed. */
function parseToml(t) {
  const o = {};
  t.split('\n').forEach((l) => { const m = l.match(/^\s*([A-Za-z0-9_]+)\s*=\s*"([^"]*)"/); if (m) o[m[1]] = m[2].trim().toLowerCase(); });
  return o;
}
const hx = (h) => { h = h.replace('#', ''); return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16)); };
const toHex = (c) => '#' + c.map((v) => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, '0')).join('');
const mix = (a, b, t) => { const x = hx(a), y = hx(b); return toHex(x.map((v, i) => v + (y[i] - v) * t)); };
const alpha = (a, t) => { const c = hx(a); return `rgba(${c[0]},${c[1]},${c[2]},${t})`; };
const lum = (h) => { const c = hx(h).map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }); return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]; };
const contrast = (a, b) => { const x = lum(a), y = lum(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); };
/* Move a color toward white (dark themes) or black (light themes) until it reaches the target contrast. */
function ensure(c, bg, target, dark) {
  for (let t = 0; t <= 1.0001; t += 0.05) { const m = mix(c, dark ? '#ffffff' : '#000000', t); if (contrast(m, bg) >= target) return m; }
  return dark ? '#ffffff' : '#000000';
}

function normalise(raw) {
  const r = { ...raw };
  if (!r.red && r.color1) { // legacy schema: color0..15 + selection_background
    const names = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];
    names.forEach((n, i) => { r[n] = r[n] || r['color' + i]; r['bright_' + n] = r['bright_' + n] || r['color' + (i + 8)]; });
    r.muted = r.muted || r.color8;
  }
  r.selection = r.selection || r.selection_background;
  const bg = r.background, fg = r.foreground;
  r.mode = r.mode || (lum(bg) < 0.4 ? 'dark' : 'light');
  const dark = r.mode === 'dark';
  r.dark_background = r.dark_background || mix(bg, dark ? '#000000' : '#000000', dark ? 0.25 : 0.04);
  r.darker_background = r.darker_background || mix(bg, '#000000', dark ? 0.45 : 0.08);
  r.lighter_background = r.lighter_background || (dark ? mix(bg, fg, 0.13) : mix(bg, '#000000', 0.15));
  r.muted = r.muted || mix(fg, bg, 0.5);
  r.orange = r.orange || mix(r.red, r.yellow, 0.5);
  r.selection = r.selection || mix(bg, r.accent, 0.3);
  return r;
}

const ACTORS = {
  dark: { gui: '#7b93ff', cli: '#4cc9c0', agent: '#c38bff', sys: '#9aa3b5' },
  light: { gui: '#3d5bd9', cli: '#0f7f78', agent: '#7a35e0', sys: '#5f6878' },
};

function deriveTokens(raw) {
  const r = normalise(raw);
  const dark = r.mode === 'dark';
  const fg = r.foreground, acc = r.accent;
  const T = {};
  if (dark) {
    Object.assign(T, { 'ink-0': r.dark_background, 'ink-1': r.background, 'ink-2': mix(r.background, r.lighter_background, 0.35), 'ink-3': mix(r.background, r.lighter_background, 0.65), 'ink-4': r.lighter_background, 'ink-5': mix(r.lighter_background, fg, 0.1) });
  } else {
    Object.assign(T, { 'ink-0': r.darker_background, 'ink-1': r.dark_background, 'ink-2': r.background, 'ink-3': mix(r.background, r.darker_background, 0.5), 'ink-4': r.darker_background, 'ink-5': r.lighter_background });
  }
  T['line-1'] = mix(T['ink-2'], fg, dark ? 0.08 : 0.1);
  T['line-2'] = mix(T['ink-2'], fg, dark ? 0.15 : 0.2);
  T['tx-1'] = fg;
  T['tx-2'] = ensure(mix(fg, T['ink-1'], 0.3), T['ink-2'], 7, dark);
  T['tx-3'] = ensure(r.muted, T['ink-2'], 4.5, dark);
  T['tx-4'] = mix(fg, T['ink-1'], 0.62);
  T.acc = acc;
  T['acc-hi'] = dark ? ensure(mix(acc, '#ffffff', 0.2), T['ink-2'], 4.5, true) : ensure(acc, T['ink-2'], 4.5, false);
  T['acc-ink'] = contrast(acc, '#ffffff') >= contrast(acc, r.darker_background) ? '#ffffff' : r.darker_background;
  T['acc-soft'] = alpha(acc, dark ? 0.16 : 0.12);
  T['acc-line'] = alpha(acc, 0.45);
  let sel = r.selection;
  if (contrast(fg, sel) < 4.5) sel = mix(T['ink-1'], r.selection, 0.3);
  T.sel = sel;
  T.ok = ensure(r.green, T['ink-2'], 3, dark); T['ok-soft'] = alpha(T.ok, 0.14);
  T.warn = ensure(r.yellow, T['ink-2'], 3, dark); T['warn-soft'] = alpha(T.warn, 0.14);
  T.bad = ensure(r.red, T['ink-2'], 3, dark); T['bad-soft'] = alpha(T.bad, 0.14);
  const who = ACTORS[dark ? 'dark' : 'light'];
  T['who-gui'] = ensure(who.gui, T['ink-2'], 4.5, dark);
  T['who-cli'] = ensure(who.cli, T['ink-2'], 4.5, dark);
  T['who-agent'] = ensure(who.agent, T['ink-2'], 4.5, dark);
  T['who-sys'] = ensure(who.sys, T['ink-2'], 4.5, dark);
  T.focus = dark ? mix(acc, '#ffffff', 0.25) : acc;
  T['focus-halo'] = alpha(acc, 0.35);
  T['code-s'] = ensure(r.orange, T['ink-0'], 4.5, dark);
  T['color-scheme'] = dark ? 'dark' : 'light';
  return T;
}
const varStyle = (T) => Object.entries(T).map(([k, v]) => (k === 'color-scheme' ? `color-scheme:${v}` : `--${k}:${v}`)).join(';');

const THEME_LIST = [
  { id: 'builtin', name: 'Quiver dark', note: 'fallback' },
  { id: 'jade-horse', name: 'jade-horse', note: 'current Omarchy theme' },
  { id: 'black-flag-resynced', name: 'black-flag-resynced', note: 'legacy schema' },
  { id: 'meta-braille-light', name: 'meta-braille-light', note: 'light' },
];
const THEMES = {};
Object.entries(THEMES_TOML).forEach(([k, v]) => { THEMES[k] = parseToml(v); });
let THEME = 'builtin';
try { THEME = localStorage.getItem('quiver-theme') || 'builtin'; } catch (e) { /* storage blocked */ }
if (THEME !== 'builtin' && !THEMES[THEME]) THEME = 'builtin';

function applyTheme() {
  const el = document.getElementById('stage-in');
  const keep = el.style.transform;
  el.setAttribute('style', THEME === 'builtin' ? '' : varStyle(deriveTokens(THEMES[THEME])));
  el.style.transform = keep;
  document.querySelectorAll('.theme-row button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.theme === THEME)));
}
function renderThemeRow() {
  const sw = (id) => {
    const t = id === 'builtin' ? null : normalise(THEMES[id]);
    const cs = t ? [t.darker_background, t.background, t.accent, t.foreground] : ['#0a0c10', '#0f1218', '#7b93ff', '#eceff5'];
    return `<span class="sw">${cs.map((c) => `<i style="background:${c}"></i>`).join('')}</span>`;
  };
  document.getElementById('theme-row').innerHTML = `<span class="lbl">App theme</span>${THEME_LIST.map((t) => `<button type="button" data-theme="${t.id}" aria-pressed="${t.id === THEME}">${sw(t.id)}${t.name}<span style="color:var(--pg-tx3)">${t.note}</span></button>`).join('')}`;
}

/* ---------- Theme screen: one screen under two Omarchy palettes ---------- */
function scrTheme() {
  const a = deriveTokens(THEMES['black-flag-resynced']), b = deriveTokens(THEMES['meta-braille-light']);
  const ra = normalise(THEMES['black-flag-resynced']);
  const mini = (T, label) => `<div style="position:relative;width:720px;height:450px;overflow:hidden">
      <div style="position:absolute;left:0;top:0;width:1440px;height:900px;transform:scale(.5);transform-origin:0 0;${varStyle(T)}">${scrGame()}</div>
      <span style="position:absolute;left:14px;bottom:14px;z-index:5;font:600 11.5px var(--f-mono);padding:4px 10px;border-radius:999px;background:rgba(0,0,0,.72);color:#fff">${label}</span></div>`;
  const rows = [
    ['ink-0', 'dark_background', 'Top bar, side list'],
    ['ink-1', 'background', 'App background'],
    ['ink-2', 'mix → lighter_background', 'Cards'],
    ['ink-4', 'lighter_background', 'Buttons, inputs'],
    ['line-2', 'mix(ink-2, foreground)', 'Control borders'],
    ['tx-1', 'foreground', 'Text'],
    ['tx-3', 'muted, ≥ 4.5:1', 'Meta text'],
    ['acc', 'accent', 'Play, focus, progress'],
    ['acc-ink', 'auto (contrast)', 'Text on accent'],
    ['sel', 'selection, ≥ 4.5:1 with text', 'Selected rows'],
    ['ok / warn / bad', 'green / yellow / red', 'Status'],
    ['who-agent', 'fixed, contrast-checked', 'Actor colors'],
  ];
  const chip = (T, k) => { const key = k.split(' / ')[0]; const v = T[key]; return `<span class="row" style="gap:6px"><i style="width:14px;height:14px;border-radius:4px;background:${v};box-shadow:inset 0 0 0 1px rgba(127,127,127,.35);display:block"></i><span class="mono muted" style="font-size:10.5px">${v.startsWith('#') ? v : 'rgba'}</span></span>`; };
  return `<div class="app" style="flex-direction:row">
    <div class="col" style="width:720px;flex:none;border-right:1px solid var(--line-1)">${mini(a, 'black-flag-resynced · dark · legacy schema')}<div style="height:0;border-top:1px solid var(--line-1)"></div>${mini(b, 'meta-braille-light · light')}</div>
    <div class="col scroll" style="flex:1;padding:26px 28px;gap:16px">
      <div class="col" style="gap:4px"><div style="font-family:var(--f-disp);font-stretch:110%;font-weight:700;font-size:20px">Quiver follows your Omarchy theme</div>
        <div class="dim" style="font-size:12.5px">Reads <span class="mono">~/.local/state/omarchy/current/theme/colors.toml</span> and restyles when <span class="mono">theme.generation</span> changes. Both previews on the left are computed from the real files by the mapping below.</div></div>
      <table class="tbl" style="font-size:12px"><thead><tr><th>Token</th><th>From colors.toml</th><th>black-flag</th><th>braille-light</th></tr></thead><tbody>
        ${rows.map(([k, from, use]) => `<tr><td><div class="mono" style="font-weight:600">${k}</div><div class="muted" style="font-size:11px">${use}</div></td><td class="dim mono" style="font-size:11px">${from}</td><td>${chip(a, k)}</td><td>${chip(b, k)}</td></tr>`).join('')}
      </tbody></table>
      <div class="col" style="gap:6px;font-size:12px" class="dim">
        <div class="row" style="gap:8px;align-items:flex-start"><span style="color:var(--acc-hi)">${ic('info', 'sm')}</span><span class="dim">black-flag-resynced uses the older schema (<span class="mono">color0…15</span>, <span class="mono">selection_background</span>, no <span class="mono">mode</span>). Quiver infers the mode from the background and derives the missing steps. Its brass selection fails contrast with the text, so selected rows use a 30% tint instead: <span class="mono">${ra.selection} → ${a.sel}</span>.</span></div>
        <div class="row" style="gap:8px;align-items:flex-start"><span style="color:var(--acc-hi)">${ic('info', 'sm')}</span><span class="dim">Hero art keeps light text on a dark scrim in every theme. Actor colors never follow the theme, and they get darker in light themes to keep 4.5:1.</span></div>
      </div>
    </div></div>`;
}
