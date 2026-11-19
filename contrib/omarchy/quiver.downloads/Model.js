.pragma library

// Pure helpers for the Quiver downloads widget. No Qt types: Service.qml feeds
// in the lines `quiver queue --watch` prints, and tests/model-test.mjs loads
// this file under node.
//
// A watch line is one JSON object:
//   {"connected":true,"queue":[QueueEntry…],"settings":{max_active,bandwidth_limit,auto_update}}
//   {"connected":false,"error":"quiverd is not running (…)"}
// A bare array (what `quiver queue --json` prints) is read as a connected queue.
//
// Everything in a line is untrusted: names and errors come from stores and
// manifests. Strings are cleaned and bounded here, refs are checked against a
// strict pattern before they are ever passed to `quiver` as an argument, and
// collections are capped.

var MAX_LINE = 1048576
var MAX_ITEMS = 64
var MAX_NAME = 120
var MAX_ERROR = 400
// Bytes per second under which an active download counts as not moving.
var SLOW = 1024

var ACTIVE = { downloading: true, verifying: true }
var STATES = { downloading: true, verifying: true, queued: true, paused: true, failed: true, done: true }
var REF = /^[a-z]{2,12}:[A-Za-z0-9._-]{1,128}$/
var KIND = /^[a-z_]{1,16}$/

// ---- text ---------------------------------------------------------------------

// One line of display text: control, bidi-override and zero-width characters
// become spaces, runs of whitespace collapse, and long text is cut with an
// ellipsis. Markup-shaped text is kept as it is: every sink renders it as
// plain text (see escapeMarkup for the one that does not).
function cleanText(value, max) {
  if (value === null || value === undefined) return ""
  var s = String(value)
  if (s.length > max * 4) s = s.slice(0, max * 4)
  s = s.replace(/[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u2028-\u202e\u2060-\u2069\ufeff]/g, " ")
  s = s.replace(/\s+/g, " ").trim()
  if (s.length > max) {
    var cut = max - 1
    // Don't split a surrogate pair.
    var c = s.charCodeAt(cut - 1)
    if (c >= 0xd800 && c <= 0xdbff) cut -= 1
    s = s.slice(0, cut).replace(/\s+$/, "") + "…"
  }
  return s
}

// Notification bodies are markup (the freedesktop spec, and Omarchy's daemon
// renders a subset of it), so dynamic text going there is escaped.
function escapeMarkup(value) {
  return String(value === null || value === undefined ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;")
}

// ---- parsing ------------------------------------------------------------------

function count(value) {
  var n = Number(value)
  if (!isFinite(n) || n < 0) return 0
  return Math.min(Math.floor(n), 1e18)
}

// The game as `quiver` takes it: `epic:<app>` for another store's game, the
// product id for a Ubisoft one. "" when neither is well-formed (no actions).
function gameRef(raw) {
  if (!raw || typeof raw !== "object") return ""
  var g = raw.game
  if (typeof g === "string" && REF.test(g)) return g
  var pid = Number(raw.product_id)
  if (isFinite(pid) && pid === Math.floor(pid) && pid > 0 && pid < 1e9) return String(pid)
  return ""
}

function parseEntry(raw, index) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null
  var ref = gameRef(raw)
  var state = typeof raw.state === "string" && STATES[raw.state] ? raw.state : "queued"
  var kind = typeof raw.kind === "string" && KIND.test(raw.kind) ? raw.kind : "install"
  var done = count(raw.done_bytes)
  var total = count(raw.total_bytes)
  var name = cleanText(raw.name, MAX_NAME)
  var position = count(raw.position)
  return {
    key: ref !== "" ? ref : "item:" + index,
    ref: ref,
    name: name !== "" ? name : (ref !== "" ? ref : "?"),
    kind: kind,
    state: state,
    position: position > 0 ? position : index + 1,
    done: done,
    total: total,
    fraction: total > 0 ? Math.min(1, done / total) : 0,
    speed: ACTIVE[state] ? count(raw.speed) : 0,
    retries: count(raw.retries),
    error: cleanText(raw.error, MAX_ERROR)
  }
}

function parseSettings(raw) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null
  var max = count(raw.max_active)
  return {
    maxActive: Math.max(1, Math.min(max || 1, 64)),
    limit: count(raw.bandwidth_limit),
    autoUpdate: raw.auto_update === true
  }
}

// One watch line → { connected, error, items, settings, truncated }, or null
// when the line is not something the widget understands (it keeps the last
// good state then).
function parseLine(text) {
  if (typeof text !== "string") return null
  if (text.length === 0 || text.length > MAX_LINE) return null
  var v
  try {
    v = JSON.parse(text)
  } catch (e) {
    return null
  }
  if (Array.isArray(v)) v = { connected: true, queue: v }
  if (!v || typeof v !== "object") return null
  if (v.connected === false) {
    return { connected: false, error: cleanText(v.error, MAX_ERROR), items: [], settings: null, truncated: false }
  }
  if (!Array.isArray(v.queue)) return null
  var items = []
  var seen = {}
  var n = Math.min(v.queue.length, MAX_ITEMS)
  for (var i = 0; i < n; i++) {
    var e = parseEntry(v.queue[i], i)
    if (!e || seen[e.key]) continue
    seen[e.key] = true
    items.push(e)
  }
  items.sort(function (a, b) { return a.position - b.position })
  return {
    connected: true,
    error: "",
    items: items,
    settings: parseSettings(v.settings),
    truncated: v.queue.length > MAX_ITEMS
  }
}

// ---- progress over time ---------------------------------------------------------

function isActive(item) {
  return !!item && ACTIVE[item.state] === true
}

// When each active item last moved: { key: { done, since } }. `since` is the
// time its byte count last went up (or it became active).
function track(previous, items, now) {
  var next = {}
  for (var i = 0; i < items.length; i++) {
    var it = items[i]
    if (!isActive(it)) continue
    var p = previous ? previous[it.key] : null
    if (p && p.state === it.state && it.done <= p.done) next[it.key] = { done: p.done, since: p.since, state: it.state }
    else next[it.key] = { done: it.done, since: now, state: it.state }
  }
  return next
}

// Keys of active items that have not moved for stallMs (and aren't moving
// now). A download quiverd is retrying slices of stays active, so this is how
// a dead connection shows.
function stalledKeys(tracked, items, now, stallMs) {
  var out = {}
  for (var i = 0; i < items.length; i++) {
    var it = items[i]
    if (!isActive(it)) continue
    var t = tracked ? tracked[it.key] : null
    if (t && now - t.since >= stallMs && it.speed < SLOW) out[it.key] = true
  }
  return out
}

// ---- summary --------------------------------------------------------------------

// What the bar shows. phase is one of:
//   idle     nothing queued
//   active   something downloading or verifying
//   stalled  everything active has stopped moving
//   failed   nothing active, something failed
//   paused   nothing active or waiting, something paused
//   queued   nothing active, something waiting to start
function summarize(items, stalled) {
  var s = {
    phase: "idle",
    active: [],
    waiting: [],
    failed: [],
    paused: 0,
    queued: 0,
    done: 0,
    total: 0,
    fraction: 0,
    speed: 0,
    eta: -1,
    retries: 0,
    stalledCount: 0,
    count: 0
  }
  items = items || []
  for (var i = 0; i < items.length; i++) {
    var it = items[i]
    if (it.state === "done") continue
    s.count += 1
    if (isActive(it)) {
      s.active.push(it)
      s.done += it.done
      s.total += it.total
      s.speed += it.speed
      s.retries += it.retries
      if (stalled && stalled[it.key]) s.stalledCount += 1
    } else {
      if (it.state === "failed") s.failed.push(it)
      else if (it.state === "paused") s.paused += 1
      else s.queued += 1
      s.waiting.push(it)
    }
  }
  s.fraction = s.total > 0 ? Math.min(1, s.done / s.total) : 0
  s.eta = eta(s.total - s.done, s.speed)
  if (s.active.length > 0) s.phase = s.stalledCount === s.active.length ? "stalled" : "active"
  else if (s.failed.length > 0) s.phase = "failed"
  else if (s.queued > 0) s.phase = "queued"
  else if (s.paused > 0) s.phase = "paused"
  return s
}

// Whole percent, rounded down so 100 only shows once it is done.
function percent(fraction) {
  var f = Number(fraction)
  if (!isFinite(f) || f <= 0) return 0
  return Math.min(100, Math.floor(f * 100 + 1e-9))
}

// Seconds left at this speed, or -1 when there's no telling.
function eta(remaining, speed) {
  if (!(speed >= SLOW) || !(remaining > 0)) return -1
  var secs = Math.ceil(remaining / speed)
  return secs > 360000 ? -1 : secs
}

// A duration in the pieces the strings need: { kind: "hm" | "m" | "lt1m", h, m }.
function durationParts(seconds) {
  var s = Math.max(0, Math.floor(Number(seconds) || 0))
  if (s < 60) return { kind: "lt1m", h: 0, m: 0 }
  var mins = Math.round(s / 60)
  if (mins < 60) return { kind: "m", h: 0, m: mins }
  return { kind: "hm", h: Math.floor(mins / 60), m: mins % 60 }
}

// Bytes in binary units as Quiver writes them ("29.5 GiB"). `fmt(value,
// decimals)` writes the number in the shell's language.
var UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"]
function bytes(n, fmt) {
  var v = Math.max(0, Number(n) || 0)
  var u = 0
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024
    u += 1
  }
  var f = fmt || function (x, d) { return x.toFixed(d) }
  return (u === 0 ? f(Math.round(v), 0) : f(v, 1)) + " " + UNITS[u]
}

function rate(n, fmt) {
  return bytes(n, fmt) + "/s"
}

// ---- changes worth a notification -------------------------------------------------

// Compare two snapshots of the queue: [{ type: "finished" | "failed", item }].
// An item that leaves the queue while it was active and essentially complete
// finished; one that leaves early was removed or cancelled (not reported).
// `ignore` holds keys this widget removed itself.
function transitions(before, after, ignore) {
  var out = []
  if (!before || !after) return out
  var now = {}
  for (var i = 0; i < after.length; i++) now[after[i].key] = after[i]
  for (var j = 0; j < before.length; j++) {
    var b = before[j]
    var a = now[b.key]
    if (!a) {
      if (ignore && ignore[b.key]) continue
      if (isActive(b) && (b.state === "verifying" || (b.total > 0 && b.fraction >= 0.99))) out.push({ type: "finished", item: b })
      continue
    }
    if (a.state === "failed" && b.state !== "failed") out.push({ type: "failed", item: a })
  }
  return out
}

// ---- bulk actions -----------------------------------------------------------------

// Refs to pause, in an order that never lets the next item start in between:
// waiting items first, then the active ones.
function pauseTargets(items) {
  var waiting = [], active = []
  for (var i = 0; i < (items || []).length; i++) {
    var it = items[i]
    if (it.ref === "") continue
    if (it.state === "queued") waiting.push(it.ref)
    else if (isActive(it)) active.push(it.ref)
  }
  return waiting.concat(active)
}

// Paused items, in queue order (failed ones are retried one at a time).
function resumeTargets(items) {
  var out = []
  for (var i = 0; i < (items || []).length; i++) {
    var it = items[i]
    if (it.ref !== "" && it.state === "paused") out.push(it.ref)
  }
  return out
}

if (typeof module !== "undefined") {
  module.exports = {
    MAX_LINE: MAX_LINE, MAX_ITEMS: MAX_ITEMS, MAX_NAME: MAX_NAME, MAX_ERROR: MAX_ERROR, SLOW: SLOW,
    cleanText: cleanText, escapeMarkup: escapeMarkup, gameRef: gameRef, parseEntry: parseEntry,
    parseSettings: parseSettings, parseLine: parseLine, isActive: isActive, track: track,
    stalledKeys: stalledKeys, summarize: summarize, percent: percent, eta: eta,
    durationParts: durationParts, bytes: bytes, rate: rate, transitions: transitions,
    pauseTargets: pauseTargets, resumeTargets: resumeTargets
  }
}
