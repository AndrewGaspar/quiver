.pragma library

// Pure helpers for the Quiver downloads widget. No Qt types: Service.qml feeds
// in what `quiver queue --json --no-start` prints, and tests/model-test.mjs
// loads this file under node.
//
// The input is a JSON array of queue entries (QueueEntry in quiver-api), each
// with the item's last minute of throughput when quiverd has it:
//   [{ "product_id": 0, "game": "epic:…", "name": "…", "kind": "install",
//      "state": "downloading", "position": 1, "done_bytes": …, "total_bytes": …,
//      "speed": …, "retries": …, "error": null,
//      "throughput": { "interval_ms": 1000, "samples": [{ "bytes": …, "state": "downloading", "retries"?: … }, …] } }, …]
//
// Everything in it is untrusted: names and errors come from stores and
// manifests. Strings are cleaned and bounded here, refs are checked against a
// strict pattern before they are ever passed to `quiver` as an argument, and
// collections are capped.

var MAX_TEXT = 4194304
var MAX_ITEMS = 64
var MAX_SAMPLES = 60
var MAX_NAME = 120
var MAX_ERROR = 400

// The app's rules for speed and time left (eta_for in
// crates/quiver-app/src/ui/downloads.rs): the average of the last 20 samples
// that ran, once there are 5; the instant speed before that. No time left under
// 100 kB/s or beyond two days.
var RATE_WINDOW = 20
var RATE_MIN_SAMPLES = 5
var ETA_MIN_RATE = 100000
var ETA_MAX_SECS = 2 * 24 * 3600
// Bytes per second under which a running download counts as not moving.
var SLOW = 1000
// Seconds of samples at about nothing before a download shows as stalled.
var STALL_SECS = 20

var ACTIVE = { downloading: true, verifying: true }
var STATES = { downloading: true, verifying: true, queued: true, paused: true, failed: true, done: true }
var SAMPLE_STATES = { downloading: true, stalled: true, retrying: true, verifying: true, paused: true, waiting: true }
// Sample states in which nothing ran: left out of averages, not zeros.
var GAP = { paused: true, waiting: true }
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

// The newest MAX_SAMPLES one-second samples, or [] when there are none.
function parseSamples(raw) {
  if (!raw || typeof raw !== "object" || !Array.isArray(raw.samples)) return []
  var list = raw.samples
  var out = []
  for (var i = Math.max(0, list.length - MAX_SAMPLES); i < list.length; i++) {
    var s = list[i]
    if (!s || typeof s !== "object") continue
    var state = typeof s.state === "string" && SAMPLE_STATES[s.state] ? s.state : "other"
    out.push({ bytes: GAP[state] ? 0 : count(s.bytes), state: state, retries: count(s.retries) })
  }
  return out
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
    // quiverd keeps the last speed on an item it stopped: only an active one's counts.
    speed: ACTIVE[state] ? count(raw.speed) : 0,
    retries: count(raw.retries),
    error: cleanText(raw.error, MAX_ERROR),
    samples: ACTIVE[state] ? parseSamples(raw.throughput) : []
  }
}

// `quiver queue --json` output → { items, truncated }, or null when it is not
// something the widget understands (it keeps the last good state then).
function parseQueue(text) {
  if (typeof text !== "string") return null
  if (text.length === 0 || text.length > MAX_TEXT) return null
  var v
  try {
    v = JSON.parse(text)
  } catch (e) {
    return null
  }
  if (!Array.isArray(v)) return null
  var items = []
  var seen = {}
  var n = Math.min(v.length, MAX_ITEMS)
  for (var i = 0; i < n; i++) {
    var e = parseEntry(v[i], i)
    if (!e || seen[e.key]) continue
    seen[e.key] = true
    items.push(e)
  }
  items.sort(function (a, b) { return a.position - b.position })
  return { items: items, truncated: v.length > MAX_ITEMS }
}

// ---- speed, time left, stalls ----------------------------------------------------

function isActive(item) {
  return !!item && ACTIVE[item.state] === true
}

// Bytes per second an active item is making: the average of the samples that
// ran among the last RATE_WINDOW, once there are RATE_MIN_SAMPLES of them;
// the instant speed before that (or from a quiverd without throughput).
function rate(item) {
  if (!isActive(item)) return 0
  var samples = item.samples || []
  var sum = 0, ran = 0
  for (var i = Math.max(0, samples.length - RATE_WINDOW); i < samples.length; i++) {
    if (GAP[samples[i].state]) continue
    sum += samples[i].bytes
    ran += 1
  }
  return ran >= RATE_MIN_SAMPLES ? Math.floor(sum / ran) : item.speed
}

// Seconds left at this rate, or -1 when it would be a guess: under 100 kB/s
// (a stall's near-zero rate gave "12772993 h" in the app), or over two days.
function eta(remaining, bps) {
  if (!(bps >= ETA_MIN_RATE) || !(remaining > 0)) return -1
  var secs = Math.floor(remaining / bps)
  return secs > ETA_MAX_SECS ? -1 : secs
}

// Stalled from the samples: a download whose last STALL_SECS samples all ran
// and brought in about nothing. null when the samples can't say (too few, or
// a quiverd without throughput); then the clock in track/stalledKeys decides.
function stalledBySamples(item) {
  if (!item || item.state !== "downloading") return item ? false : null
  var samples = item.samples || []
  if (samples.length < STALL_SECS) return null
  var sum = 0
  for (var i = samples.length - STALL_SECS; i < samples.length; i++) {
    if (GAP[samples[i].state]) return false
    sum += samples[i].bytes
  }
  return sum < SLOW * STALL_SECS
}

// When each active item last moved, for a quiverd without throughput:
// { key: { done, since, state } }. `since` is the time its byte count last
// went up (or it became active).
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

// Keys of active items that are stalled: by their samples when those say,
// else not moved for stallMs and not reporting speed now.
function stalledKeys(tracked, items, now, stallMs) {
  var out = {}
  for (var i = 0; i < items.length; i++) {
    var it = items[i]
    if (!isActive(it)) continue
    var bySamples = stalledBySamples(it)
    if (bySamples === true) out[it.key] = true
    if (bySamples !== null) continue
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
      s.retries += it.retries
      if (stalled && stalled[it.key]) s.stalledCount += 1
      else s.speed += rate(it)
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

// How often to ask quiverd, in ms: every 2 s while something runs, every 15 s
// while the queue sits still; backing off to a minute while quiverd is down
// (`downs` failed polls in a row) or the CLI can't answer.
function pollInterval(phase, downs) {
  if (downs > 0) return [5000, 15000, 30000, 60000][Math.min(downs, 4) - 1]
  return phase === "active" || phase === "stalled" ? 2000 : 15000
}

// ---- formatting ---------------------------------------------------------------------

// Whole percent, rounded down so 100 only shows once it is done.
function percent(fraction) {
  var f = Number(fraction)
  if (!isFinite(f) || f <= 0) return 0
  return Math.min(100, Math.floor(f * 100 + 1e-9))
}

// A duration in the pieces the strings need: { kind: "hm" | "m" | "lt1m", h, m }.
function durationParts(seconds) {
  var s = Math.max(0, Math.floor(Number(seconds) || 0))
  if (s < 60) return { kind: "lt1m", h: 0, m: 0 }
  var mins = Math.round(s / 60)
  if (mins < 60) return { kind: "m", h: 0, m: mins }
  return { kind: "hm", h: Math.floor(mins / 60), m: mins % 60 }
}

// Sizes as the app writes them, in decimal units ("48.7 GB", "1.21 TB").
// `fmt(value, decimals)` writes the number in the shell's language.
function plain(x, d) { return x.toFixed(d) }
function bytes(n, fmt) {
  var f = fmt || plain
  var v = Math.max(0, Number(n) || 0)
  if (v >= 1e12) return f(v / 1e12, 2) + " TB"
  var g = v / 1e9
  if (g >= 100) return f(g, 0) + " GB"
  if (g >= 1) return f(g, 1) + " GB"
  return f(v / 1e6, 0) + " MB"
}

// Rates as the app writes them ("84.6 MB/s", "512 KB/s").
function rateText(n, fmt) {
  var f = fmt || plain
  var m = Math.max(0, Number(n) || 0) / 1e6
  if (m >= 100) return f(m, 0) + " MB/s"
  if (m >= 1) return f(m, 1) + " MB/s"
  return f(m * 1000, 0) + " KB/s"
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

// What a failed `quiver queue` run means: "down" when quiverd isn't running,
// "unavailable" when this quiver can't answer (too old for --no-start, not
// installed), else "error".
function pollFailure(exitCode, stderr) {
  var e = String(stderr || "")
  if (/quiverd is not running|quiverd went away|quiverd restarted/.test(e)) return "down"
  if (exitCode === 2 || exitCode === 127 || /unexpected argument|unrecognized subcommand|No such file/.test(e)) return "unavailable"
  return "error"
}

if (typeof module !== "undefined") {
  module.exports = {
    MAX_TEXT: MAX_TEXT, MAX_ITEMS: MAX_ITEMS, MAX_SAMPLES: MAX_SAMPLES, MAX_NAME: MAX_NAME, MAX_ERROR: MAX_ERROR,
    SLOW: SLOW, STALL_SECS: STALL_SECS,
    cleanText: cleanText, escapeMarkup: escapeMarkup, gameRef: gameRef, parseSamples: parseSamples,
    parseEntry: parseEntry, parseQueue: parseQueue, isActive: isActive, rate: rate, eta: eta,
    stalledBySamples: stalledBySamples, track: track, stalledKeys: stalledKeys, summarize: summarize,
    pollInterval: pollInterval, percent: percent, durationParts: durationParts, bytes: bytes,
    rateText: rateText, transitions: transitions, pauseTargets: pauseTargets, resumeTargets: resumeTargets,
    pollFailure: pollFailure
  }
}
