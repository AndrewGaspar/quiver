// node tests/model-test.mjs — pure-model checks for the Quiver downloads widget.
import fs from "node:fs"
import vm from "node:vm"
import assert from "node:assert/strict"
import path from "node:path"
import { fileURLToPath } from "node:url"

const here = path.dirname(fileURLToPath(import.meta.url))
const src = fs.readFileSync(path.join(here, "..", "Model.js"), "utf8").replace(/^\.pragma library\s*$/m, "")
const ctx = { module: { exports: {} } }
vm.runInNewContext(src, ctx)
const M = ctx.module.exports

// The model runs in its own vm realm, so its objects fail deepStrictEqual on prototype.
const eq = (a, b) => assert.equal(JSON.stringify(a), JSON.stringify(b))
let n = 0
const test = (name, fn) => {
  try {
    fn()
  } catch (e) {
    console.error(`not ok - ${name}`)
    throw e
  }
  n++
}

// A queue entry as `quiver queue --json` prints it.
const entry = (extra = {}) => ({
  product_id: 5059, name: "Assassin's Creed® Odyssey", kind: "install", state: "queued", position: 1,
  job_id: 2, language: "en-US", account: null, source: "gui", added: "2026-11-19T21:38:03Z",
  error: null, done_bytes: 0, total_bytes: 0, speed: 0, retries: 0, ...extra,
})
const epic = (extra = {}) => entry({
  product_id: 0, game: "epic:dc9d2e595d0e4650b35d659f90d41059", name: "Alan Wake 2", ...extra,
})
// `throughput` as quiverd sends it: samples oldest first.
const tp = (samples) => ({ game: "x", interval_ms: 1000, at: "2026-11-19T22:58:04Z", samples })
const run = (n, bytes, state = "downloading") => Array.from({ length: n }, () => ({ bytes, state }))
const json = (queue) => JSON.stringify(queue, null, 2)
const items = (...q) => M.parseQueue(json(q)).items
const sum = (...q) => {
  const it = items(...q)
  return M.summarize(it, M.stalledKeys({}, it, 0, 20000))
}

// ---- parsing --------------------------------------------------------------------

test("the live queue (tests/fixtures/live-queue.json) parses, in queue order", () => {
  const r = M.parseQueue(fs.readFileSync(path.join(here, "fixtures", "live-queue.json"), "utf8"))
  assert.equal(r.items.length, 3)
  assert.equal(r.items[0].ref, "epic:dc9d2e595d0e4650b35d659f90d41059")
  assert.equal(r.items[0].state, "downloading")
  assert.equal(r.items[0].samples.length, 60)
  assert.ok(M.rate(r.items[0]) > 1e6, "a real rate")
  // Waiting items carry no samples (and no speed: quiverd keeps a stale one on them).
  assert.equal(r.items[1].samples.length, 0)
  assert.equal(r.items[1].speed, 0)
  assert.equal(M.summarize(r.items, {}).phase, "active")
})

test("garbage and wrong shapes are rejected, not half-read", () => {
  for (const bad of ["", "nope", "[", "null", "42", '"x"', "{}", '{"queue":[]}', "\u0000"])
    assert.equal(M.parseQueue(bad), null, JSON.stringify(bad))
  assert.equal(M.parseQueue(undefined), null)
  assert.equal(M.parseQueue("[" + " ".repeat(M.MAX_TEXT) + "]"), null)
})

test("a queued item's stale speed is not counted", () => {
  // quiverd keeps the last speed on an item it stopped (seen live: queued with 918405).
  assert.equal(items(entry({ state: "queued", speed: 918405 }))[0].speed, 0)
})

test("markup-shaped names stay literal text", () => {
  assert.equal(items(entry({ name: "<img src=x onerror=alert(1)> <b>Bold</b> &amp;" }))[0].name,
    "<img src=x onerror=alert(1)> <b>Bold</b> &amp;")
})

test("control, bidi and zero-width characters are neutralised; long names are cut", () => {
  assert.equal(items(entry({ name: "Evil‮gnp.exe\nsecond\tline​!" }))[0].name, "Evil gnp.exe second line !")
  const long = items(entry({ name: "x".repeat(5000) }))[0].name
  assert.equal(long.length, M.MAX_NAME)
  assert.ok(long.endsWith("…"))
  const err = items(entry({ state: "failed", error: "e".repeat(10000) }))[0].error
  assert.equal(err.length, M.MAX_ERROR)
})

test("a cut never splits a surrogate pair", () => {
  const s = M.cleanText("a" + "😀".repeat(100), 10)
  assert.ok(!/[\ud800-\udbff](?![\udc00-\udfff])/.test(s), s)
})

test("refs: only well-formed ones reach quiver's argv", () => {
  assert.equal(M.gameRef({ product_id: 13504 }), "13504")
  assert.equal(M.gameRef({ product_id: 0, game: "epic:abc_DEF-1.2" }), "epic:abc_DEF-1.2")
  assert.equal(M.gameRef({ product_id: 0, game: "epic:$(rm -rf ~)" }), "")
  assert.equal(M.gameRef({ product_id: 0, game: "--source=evil" }), "")
  assert.equal(M.gameRef({ product_id: 0, game: "-x:y" }), "")
  assert.equal(M.gameRef({ product_id: -3 }), "")
  assert.equal(M.gameRef({ product_id: 1.5 }), "")
  assert.equal(M.gameRef({ product_id: "12; ls" }), "")
  assert.equal(M.gameRef({ product_id: 0, game: { store: "epic" } }), "")
  const r = items(entry({ product_id: 0, game: "bad ref", name: "A" }), entry({ product_id: 0, name: "B" }))
  assert.equal(r.length, 2)
  assert.equal(r[0].ref, "")
  assert.notEqual(r[0].key, r[1].key)
})

test("collections are capped and duplicates dropped", () => {
  const many = Array.from({ length: 500 }, (_, i) => entry({ product_id: i + 1, position: i + 1 }))
  const r = M.parseQueue(json(many))
  assert.equal(r.items.length, M.MAX_ITEMS)
  assert.equal(r.truncated, true)
  assert.equal(items(entry(), entry({ position: 2 })).length, 1)
  const huge = items(epic({ state: "downloading", throughput: tp(run(5000, 7)) }))[0]
  assert.equal(huge.samples.length, M.MAX_SAMPLES)
  const junk = items(epic({ state: "downloading", throughput: { samples: [null, 5, { bytes: -1, state: "<b>" }, { bytes: 9, state: "paused" }] } }))[0]
  eq(junk.samples, [{ bytes: 0, state: "other", retries: 0 }, { bytes: 0, state: "paused", retries: 0 }])
  assert.equal(items(epic({ state: "downloading", throughput: "lots" }))[0].samples.length, 0)
})

test("numbers: negative, NaN, strings and huge values are clamped", () => {
  const it = items(entry({ state: "downloading", done_bytes: -5, total_bytes: "lots", speed: 1e300, retries: NaN }))[0]
  assert.equal(it.done, 0)
  assert.equal(it.total, 0)
  assert.equal(it.speed, 1e18)
  assert.equal(it.retries, 0)
  assert.equal(it.fraction, 0)
})

test("unknown states and kinds fall back", () => {
  const it = items(entry({ state: "exploding", kind: "<script>" }))[0]
  assert.equal(it.state, "queued")
  assert.equal(it.kind, "install")
})

// ---- speed and time left ---------------------------------------------------------------

test("speed: the recent average, not the jumpy instant figure", () => {
  const samples = [...run(40, 1_000_000), ...run(10, 20_000_000), ...run(10, 30_000_000)]
  const it = items(epic({ state: "downloading", speed: 3, throughput: tp(samples) }))[0]
  assert.equal(M.rate(it), 25_000_000)
})

test("speed: pauses and waits are left out, and too few samples fall back to the instant speed", () => {
  const gaps = [...run(15, 0, "paused"), ...run(5, 10_000_000)]
  assert.equal(M.rate(items(epic({ state: "downloading", speed: 1, throughput: tp(gaps) }))[0]), 10_000_000)
  const few = [...run(16, 0, "waiting"), ...run(4, 10_000_000)]
  assert.equal(M.rate(items(epic({ state: "downloading", speed: 7_000_000, throughput: tp(few) }))[0]), 7_000_000)
  assert.equal(M.rate(items(epic({ state: "downloading", speed: 7_000_000 }))[0]), 7_000_000, "a quiverd without throughput")
  assert.equal(M.rate(items(epic({ state: "paused", speed: 7_000_000, throughput: tp(run(30, 5)) }))[0]), 0)
})

test("time left: the app's rules", () => {
  assert.equal(M.eta(25_000_000 * 60, 25_000_000), 60)
  assert.equal(M.eta(1_000_000_000, 99_999), -1, "under 100 kB/s: none")
  assert.equal(M.eta(10_000_000_000_000, 25_000_000), -1, "more than two days: none")
  assert.equal(M.eta(0, 25_000_000), -1)
  assert.equal(M.eta(100, NaN), -1)
})

test("downloading: progress, the average speed and time left add up across active items", () => {
  const s = sum(
    epic({ state: "downloading", position: 1, done_bytes: 500_000_000, total_bytes: 1_000_000_000, speed: 1, throughput: tp(run(30, 10_000_000)) }),
    entry({ state: "verifying", position: 2, done_bytes: 0, total_bytes: 1_000_000_000, speed: 1, throughput: tp(run(30, 15_000_000, "verifying")) }),
    entry({ product_id: 13504, position: 3 }),
  )
  assert.equal(s.phase, "active")
  assert.equal(s.active.length, 2)
  assert.equal(s.waiting.length, 1)
  assert.equal(M.percent(s.fraction), 25)
  assert.equal(s.speed, 25_000_000)
  assert.equal(s.eta, 60)
  assert.equal(s.count, 3)
})

// ---- stalls ----------------------------------------------------------------------------------

test("stalled: 20 s of running samples bringing in about nothing; no giant time left", () => {
  const stalled = epic({
    state: "downloading", done_bytes: 31_699_451_644, total_bytes: 105_865_248_129, speed: 3, retries: 7,
    throughput: tp([...run(40, 25_000_000), ...run(12, 0, "stalled"), ...run(8, 0, "retrying")]),
  })
  const s = sum(stalled, entry({ position: 2 }))
  assert.equal(s.phase, "stalled")
  assert.equal(s.stalledCount, 1)
  assert.equal(s.speed, 0)
  assert.equal(s.eta, -1)
  // Moving again (a few seconds of bytes): not stalled.
  const moving = epic({ state: "downloading", throughput: tp([...run(50, 0, "stalled"), ...run(10, 5_000_000)]) })
  assert.equal(sum(moving).phase, "active")
  // A slow but moving download isn't stalled, it just has no time left.
  const slow = epic({ state: "downloading", done_bytes: 1, total_bytes: 1e10, throughput: tp(run(60, 50_000)) })
  const ss = sum(slow)
  assert.equal(ss.phase, "active")
  assert.equal(ss.speed, 50_000)
  assert.equal(ss.eta, -1)
})

test("not stalled: just started, verifying, or a pause inside the window", () => {
  assert.equal(sum(epic({ state: "downloading", throughput: tp(run(10, 0, "stalled")) })).phase, "active")
  assert.equal(sum(epic({ state: "verifying", throughput: tp(run(60, 0, "verifying")) })).phase, "active")
  assert.equal(sum(epic({ state: "downloading", throughput: tp([...run(50, 0, "stalled"), ...run(1, 0, "paused"), ...run(9, 0, "stalled")]) })).phase, "active")
})

test("stalled only when every active item has stopped", () => {
  const q = items(
    epic({ state: "downloading", position: 1, throughput: tp(run(30, 0, "stalled")) }),
    entry({ state: "downloading", position: 2, throughput: tp(run(30, 5_000_000)) }),
  )
  assert.equal(M.summarize(q, M.stalledKeys({}, q, 0, 20000)).phase, "active")
})

test("a quiverd without throughput: an item that stops moving is stalled after stallMs", () => {
  const at = (done, speed) => items(epic({ state: "downloading", done_bytes: done, total_bytes: 1000, speed }))
  let t = M.track({}, at(100, 5000), 0)
  t = M.track(t, at(200, 5000), 1000)
  eq(M.stalledKeys(t, at(200, 5000), 1000, 20000), {})
  t = M.track(t, at(200, 10), 15000)
  eq(M.stalledKeys(t, at(200, 10), 15000, 20000), {})
  eq(Object.keys(M.stalledKeys(t, at(200, 10), 21000, 20000)), ["epic:dc9d2e595d0e4650b35d659f90d41059"])
  eq(M.stalledKeys(t, at(200, 4096), 21000, 20000), {}, "still reporting speed")
  t = M.track(t, at(300, 2000), 22000)
  eq(M.stalledKeys(t, at(300, 2000), 22000, 20000), {})
  // A phase change (download → verify) restarts the clock.
  let v = M.track({}, items(epic({ state: "downloading", done_bytes: 1000, total_bytes: 1000 })), 0)
  v = M.track(v, items(epic({ state: "verifying", done_bytes: 0, total_bytes: 1000 })), 50000)
  eq(M.stalledKeys(v, items(epic({ state: "verifying", done_bytes: 0, total_bytes: 1000 })), 60000, 20000), {})
  eq(M.track({}, items(entry({ state: "paused" })), 0), {})
})

// ---- phases ----------------------------------------------------------------------------------

test("idle when the queue is empty", () => {
  eq(M.summarize([], {}).phase, "idle")
  eq(sum().phase, "idle")
})

test("nothing active: failed beats queued beats paused", () => {
  const failed = entry({ state: "failed", error: "disk full", position: 1 })
  const paused = epic({ state: "paused", position: 2, throughput: tp(run(60, 0, "paused")) })
  const queued = entry({ product_id: 7, state: "queued", position: 3 })
  assert.equal(sum(failed, paused, queued).phase, "failed")
  assert.equal(sum(paused, queued).phase, "queued")
  assert.equal(sum(paused).phase, "paused")
  const s = sum(failed, paused)
  assert.equal(s.failed.length, 1)
  assert.equal(s.paused, 1)
  assert.equal(s.speed, 0)
})

test("a failure next to an active download keeps the bar active", () => {
  const s = sum(epic({ state: "downloading", done_bytes: 1, total_bytes: 2, speed: 9999 }), entry({ state: "failed", position: 2 }))
  assert.equal(s.phase, "active")
  assert.equal(s.failed.length, 1)
})

test("quiverd down, an old CLI, or something else wrong", () => {
  assert.equal(M.pollFailure(1, "Error: quiverd is not running (No such file or directory (os error 2))\n"), "down")
  assert.equal(M.pollFailure(2, "error: unexpected argument '--no-start' found\n\nUsage: quiver queue --json"), "unavailable")
  assert.equal(M.pollFailure(127, ""), "unavailable")
  assert.equal(M.pollFailure(1, "Error: permission denied"), "error")
})

test("polling: 2 s while something runs, 15 s while still, backing off to a minute while down", () => {
  assert.equal(M.pollInterval("active", 0), 2000)
  assert.equal(M.pollInterval("stalled", 0), 2000)
  for (const p of ["idle", "paused", "queued", "failed"]) assert.equal(M.pollInterval(p, 0), 15000)
  eq([1, 2, 3, 4, 9].map((d) => M.pollInterval("down", d)), [5000, 15000, 30000, 60000, 60000])
})

// ---- formatting ---------------------------------------------------------------------------

test("percent rounds down and stays within 0..100", () => {
  assert.equal(M.percent(0.2999), 29)
  assert.equal(M.percent(0.999), 99)
  assert.equal(M.percent(1), 100)
  assert.equal(M.percent(-1), 0)
  assert.equal(M.percent(NaN), 0)
})

test("sizes and rates as the app writes them, numbers through the formatter", () => {
  assert.equal(M.bytes(0), "0 MB")
  assert.equal(M.bytes(140_694_473), "141 MB")
  assert.equal(M.bytes(31_699_451_644), "31.7 GB")
  assert.equal(M.bytes(105_865_248_129), "106 GB")
  assert.equal(M.bytes(1_210_000_000_000), "1.21 TB")
  assert.equal(M.rateText(25_922_326), "25.9 MB/s")
  assert.equal(M.rateText(184_600_000), "185 MB/s")
  assert.equal(M.rateText(512_000), "512 KB/s")
  const danish = (v, d) => v.toFixed(d).replace(".", ",")
  assert.equal(M.rateText(25_922_326, danish), "25,9 MB/s")
})

test("durations", () => {
  eq(M.durationParts(30), { kind: "lt1m", h: 0, m: 0 })
  eq(M.durationParts(90), { kind: "m", h: 0, m: 2 })
  eq(M.durationParts(3600 + 12 * 60), { kind: "hm", h: 1, m: 12 })
  eq(M.durationParts(59 * 60 + 50), { kind: "hm", h: 1, m: 0 })
})

// ---- notifications --------------------------------------------------------------------------

test("notification text is escaped for markup", () => {
  assert.equal(M.escapeMarkup('<img src="https://example.invalid/t"> & \'x\''),
    "&lt;img src=&quot;https://example.invalid/t&quot;&gt; &amp; &#39;x&#39;")
  assert.equal(M.escapeMarkup(null), "")
})

test("transitions: finished, failed, and removals that aren't reported", () => {
  const dl = items(
    epic({ state: "downloading", done_bytes: 995, total_bytes: 1000, speed: 5000 }),
    entry({ state: "downloading", position: 2, done_bytes: 10, total_bytes: 1000 }),
    entry({ product_id: 9, state: "queued", position: 3 }),
  )
  // Alan Wake 2 left the queue at 99.5%: finished. Odyssey left at 1%: removed. Product 9 failed.
  const after = items(entry({ product_id: 9, state: "failed", error: "<b>no space</b>", position: 1 }))
  const t = M.transitions(dl, after, {})
  eq(t.map((x) => [x.type, x.item.ref]), [["finished", "epic:dc9d2e595d0e4650b35d659f90d41059"], ["failed", "9"]])
  assert.equal(t[1].item.error, "<b>no space</b>")
  eq(M.transitions(dl, after, { "epic:dc9d2e595d0e4650b35d659f90d41059": true }).map((x) => x.type), ["failed"])
  eq(M.transitions(null, after, {}), [])
  eq(M.transitions(after, after, {}), [])
})

// ---- bulk actions --------------------------------------------------------------------------------

test("pause all: waiting items first, then active; resume all: paused only", () => {
  const q = items(
    epic({ state: "downloading", position: 1, done_bytes: 1, total_bytes: 2 }),
    entry({ state: "queued", position: 2 }),
    entry({ product_id: 7, state: "paused", position: 3 }),
    entry({ product_id: 8, state: "failed", position: 4 }),
    entry({ product_id: 0, game: "bad ref", state: "queued", position: 5 }),
  )
  eq(M.pauseTargets(q), ["5059", "epic:dc9d2e595d0e4650b35d659f90d41059"])
  eq(M.resumeTargets(q), ["7"])
})

// ---- now playing, recent games -------------------------------------------------------------------

// A line as `quiver playing --json` prints it.
const playingLine = (extra = {}) => JSON.stringify({
  connected: true,
  playing: [{
    ref: "3539", name: "Assassin's Creed Origins", short: "AC Origins", started: "2026-11-21T19:00:00Z",
    profile: "Pharos", via: ["launcher"], session: "s-pzfkk4bl2v",
    stream: { peer: "roth", client: "moonlight", state: "streaming" },
    icon: "/home/u/.local/share/quiver/icons/3095113d1c9698957e4fb4e2ce087e0d.png",
  }],
  recent: [
    { ref: "420", name: "Far Cry 4", short: "Far Cry 4", last_played: "2026-11-21T18:46:01.9Z" },
    { ref: "epic:dc9d2e595d0e4650b35d659f90d41059", name: "Alan Wake 2", short: "Alan Wake 2", last_played: "2026-11-20T10:00:00Z" },
  ],
  ...extra,
})

test("playing: a snapshot parses, with refs, sessions and streams checked", () => {
  const p = M.parsePlaying(playingLine())
  assert.equal(p.connected, true)
  assert.equal(p.playing.length, 1)
  const g = p.playing[0]
  eq([g.ref, g.short, g.profile, g.session, g.stream.peer, g.stream.client, g.stream.state],
    ["3539", "AC Origins", "Pharos", "s-pzfkk4bl2v", "roth", "moonlight", "streaming"])
  assert.equal(g.started, Date.parse("2026-11-21T19:00:00Z"))
  eq(p.recent.map((r) => r.ref), ["420", "epic:dc9d2e595d0e4650b35d659f90d41059"])
  assert.equal(p.recent[1].icon, "")
  assert.equal(M.parsePlaying(playingLine({ connected: false })).connected, false)
})

test("playing: bad refs, sessions, paths and markup never get through", () => {
  const p = M.parsePlaying(JSON.stringify({
    connected: true,
    playing: [
      { ref: "3539; rm -rf ~", name: "x", started: "2026-11-21T19:00:00Z" },
      { ref: "epic:abc", name: "<img src=x onerror=alert(1)>‮", short: "", session: "s-$(id)", via: ["launcher", "Bad Holder", 3],
        stream: { peer: "", client: "x" }, icon: "relative.png", started: "nonsense" },
    ],
    recent: [
      { ref: "epic:abc", name: "dupe of playing" },
      { ref: "0", name: "zero" },
      { ref: "steam:620", name: "Portal 2", icon: "/x/../etc/passwd.png" },
      { ref: "steam:440", name: "TF2", icon: "/home/u/.cache/quiver/art/a.jpg" },
    ],
  }))
  eq(p.playing.map((g) => g.ref), ["epic:abc"])
  const g = p.playing[0]
  assert.equal(g.name, "<img src=x onerror=alert(1)>")
  assert.equal(g.short, g.name)
  eq([g.session, g.via, g.stream, g.icon, g.started], ["", ["launcher"], null, "", 0])
  eq(p.recent.map((r) => [r.ref, r.icon]), [["steam:620", ""], ["steam:440", "/home/u/.cache/quiver/art/a.jpg"]])
  for (const bad of ["", "[]", "nope", "null", "{\"playing\": 3}"]) {
    const v = M.parsePlaying(bad)
    assert.ok(v === null || (v.playing.length === 0 && v.recent.length === 0), bad)
  }
})

test("playing: collections are capped", () => {
  const many = Array.from({ length: 50 }, (_, i) => ({ ref: String(i + 1), name: `g${i}` }))
  const p = M.parsePlaying(JSON.stringify({ playing: many, recent: many }))
  assert.equal(p.playing.length, M.MAX_PLAYING)
  assert.equal(p.recent.length, M.MAX_RECENT)
  assert.equal(p.recent[0].ref, String(M.MAX_PLAYING + 1))
})

test("ago and follow retry", () => {
  const now = Date.parse("2026-11-21T20:00:00Z")
  eq(M.agoParts(now - 30e3, now), { kind: "now", n: 0 })
  eq(M.agoParts(now - 5 * 60e3, now), { kind: "m", n: 5 })
  eq(M.agoParts(now - 3 * 3600e3, now), { kind: "h", n: 3 })
  eq(M.agoParts(now - 50 * 3600e3, now), { kind: "d", n: 2 })
  eq(M.agoParts(0, now), { kind: "now", n: 0 })
  eq([1, 2, 3, 4, 9].map((d) => M.followRetry("down", d)), [5000, 15000, 30000, 60000, 60000])
  assert.equal(M.followRetry("unavailable", 1), 60000)
})

console.log(`ok - ${n} model tests`)
