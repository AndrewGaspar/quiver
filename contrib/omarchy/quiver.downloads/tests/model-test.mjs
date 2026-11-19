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
const line = (queue, settings = { max_active: 1, bandwidth_limit: 26214400, auto_update: true }) =>
  JSON.stringify({ connected: true, queue, settings })

// ---- parsing --------------------------------------------------------------------

test("a connected watch line parses, in queue order", () => {
  const r = M.parseLine(line([
    entry({ position: 2 }),
    epic({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 25922326 }),
  ]))
  assert.equal(r.connected, true)
  assert.equal(r.items.length, 2)
  assert.equal(r.items[0].ref, "epic:dc9d2e595d0e4650b35d659f90d41059")
  assert.equal(r.items[0].state, "downloading")
  assert.equal(r.items[0].speed, 25922326)
  assert.equal(r.items[1].ref, "5059")
  eq(r.settings, { maxActive: 1, limit: 26214400, autoUpdate: true })
})

test("a bare array (quiver queue --json) reads as connected", () => {
  const r = M.parseLine(JSON.stringify([entry()]))
  assert.equal(r.connected, true)
  assert.equal(r.items.length, 1)
  assert.equal(r.settings, null)
})

test("the pretty multi-line --json output parses too", () => {
  const r = M.parseLine(JSON.stringify([entry()], null, 2))
  assert.equal(r.items.length, 1)
})

test("a down line carries the reason", () => {
  const r = M.parseLine('{"connected":false,"error":"quiverd is not running (No such file or directory (os error 2))"}')
  assert.equal(r.connected, false)
  assert.match(r.error, /not running/)
  assert.equal(r.items.length, 0)
})

test("garbage and wrong shapes are rejected, not half-read", () => {
  for (const bad of ["", "nope", "{", "null", "42", '"x"', '{"connected":true}', '{"queue":"x"}', "\u0000"])
    assert.equal(M.parseLine(bad), null, JSON.stringify(bad))
  assert.equal(M.parseLine(undefined), null)
  assert.equal(M.parseLine("[" + " ".repeat(M.MAX_LINE) + "]"), null)
})

test("a queued item's stale speed is not counted", () => {
  // quiverd keeps the last speed on an item it stopped (seen live: queued with 918405).
  const r = M.parseLine(line([entry({ state: "queued", speed: 918405 })]))
  assert.equal(r.items[0].speed, 0)
})

test("markup-shaped names stay literal text", () => {
  const r = M.parseLine(line([entry({ name: '<img src=x onerror=alert(1)> <b>Bold</b> &amp;' })]))
  assert.equal(r.items[0].name, "<img src=x onerror=alert(1)> <b>Bold</b> &amp;")
})

test("control, bidi and zero-width characters are neutralised; long names are cut", () => {
  const r = M.parseLine(line([entry({ name: "Evil\u202egnp.exe\nsecond\tline\u200b!" })]))
  assert.equal(r.items[0].name, "Evil gnp.exe second line !")
  const long = M.parseLine(line([entry({ name: "x".repeat(5000) })])).items[0].name
  assert.equal(long.length, M.MAX_NAME)
  assert.ok(long.endsWith("…"))
  const err = M.parseLine(line([entry({ state: "failed", error: "e".repeat(10000) })])).items[0].error
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
  const r = M.parseLine(line([entry({ product_id: 0, game: "bad ref", name: "A" }), entry({ product_id: 0, name: "B" })]))
  assert.equal(r.items.length, 2)
  assert.equal(r.items[0].ref, "")
  assert.notEqual(r.items[0].key, r.items[1].key)
})

test("collections are capped and duplicates dropped", () => {
  const many = Array.from({ length: 500 }, (_, i) => entry({ product_id: i + 1, position: i + 1 }))
  const r = M.parseLine(line(many))
  assert.equal(r.items.length, M.MAX_ITEMS)
  assert.equal(r.truncated, true)
  const dup = M.parseLine(line([entry(), entry({ position: 2 })]))
  assert.equal(dup.items.length, 1)
})

test("numbers: negative, NaN, strings and huge values are clamped", () => {
  const r = M.parseLine(line([entry({ state: "downloading", done_bytes: -5, total_bytes: "lots", speed: 1e300, retries: NaN })]))
  const it = r.items[0]
  assert.equal(it.done, 0)
  assert.equal(it.total, 0)
  assert.equal(it.speed, 1e18)
  assert.equal(it.retries, 0)
  assert.equal(it.fraction, 0)
})

test("unknown states and kinds fall back", () => {
  const it = M.parseLine(line([entry({ state: "exploding", kind: "<script>" })])).items[0]
  assert.equal(it.state, "queued")
  assert.equal(it.kind, "install")
})

test("settings are bounded", () => {
  eq(M.parseSettings({ max_active: 0, bandwidth_limit: null, auto_update: "yes" }), { maxActive: 1, limit: 0, autoUpdate: false })
  eq(M.parseSettings({ max_active: 1000 }), { maxActive: 64, limit: 0, autoUpdate: false })
  assert.equal(M.parseSettings([1]), null)
})

// ---- summary -----------------------------------------------------------------------

const items = (...q) => M.parseLine(line(q)).items

test("idle when the queue is empty", () => {
  eq(M.summarize([], {}).phase, "idle")
  eq(M.summarize(items(), {}).phase, "idle")
})

test("active: progress, speed and ETA add up across active items", () => {
  const q = items(
    epic({ state: "downloading", position: 1, done_bytes: 50, total_bytes: 100, speed: 10 * 1024 }),
    entry({ state: "verifying", position: 2, done_bytes: 25, total_bytes: 100, speed: 10 * 1024 }),
    entry({ product_id: 13504, position: 3 }),
  )
  const s = M.summarize(q, {})
  assert.equal(s.phase, "active")
  assert.equal(s.active.length, 2)
  assert.equal(s.waiting.length, 1)
  assert.equal(s.fraction, 75 / 200)
  assert.equal(s.speed, 20 * 1024)
  assert.equal(s.eta, 1)
  assert.equal(s.count, 3)
})

test("the live queue: Alan Wake 2 downloading, two queued", () => {
  const q = items(
    entry({ position: 1, state: "queued", done_bytes: 140694473, total_bytes: 102383586228, speed: 918405 }),
    epic({ position: 2, state: "downloading", done_bytes: 31699451644, total_bytes: 105865248129, speed: 25922326 }),
    entry({ product_id: 13504, name: "Assassin's Creed® Valhalla", position: 3 }),
  )
  const s = M.summarize(q, {})
  assert.equal(s.phase, "active")
  assert.equal(M.percent(s.fraction), 29)
  assert.equal(s.speed, 25922326)
  eq(M.durationParts(s.eta), { kind: "m", h: 0, m: 48 })
  assert.equal(s.queued, 2)
})

test("stalled only when every active item has stopped", () => {
  const q = items(
    epic({ state: "downloading", position: 1, done_bytes: 50, total_bytes: 100, speed: 0 }),
    entry({ state: "downloading", position: 2, done_bytes: 50, total_bytes: 100, speed: 5000 }),
  )
  assert.equal(M.summarize(q, { [q[0].key]: true }).phase, "active")
  assert.equal(M.summarize(q, { [q[0].key]: true, [q[1].key]: true }).phase, "stalled")
})

test("nothing active: failed beats queued beats paused", () => {
  const failed = entry({ state: "failed", error: "disk full", position: 1 })
  const paused = epic({ state: "paused", position: 2 })
  const queued = entry({ product_id: 7, state: "queued", position: 3 })
  assert.equal(M.summarize(items(failed, paused, queued), {}).phase, "failed")
  assert.equal(M.summarize(items(paused, queued), {}).phase, "queued")
  assert.equal(M.summarize(items(paused), {}).phase, "paused")
  const s = M.summarize(items(failed, paused), {})
  assert.equal(s.failed.length, 1)
  assert.equal(s.paused, 1)
})

test("a failure next to an active download keeps the bar active", () => {
  const s = M.summarize(items(epic({ state: "downloading", done_bytes: 1, total_bytes: 2, speed: 9999 }), entry({ state: "failed", position: 2 })), {})
  assert.equal(s.phase, "active")
  assert.equal(s.failed.length, 1)
})

// ---- stall tracking -------------------------------------------------------------------

test("an item that stops moving is stalled after stallMs, and recovers when it moves", () => {
  const at = (done, speed) => items(epic({ state: "downloading", done_bytes: done, total_bytes: 1000, speed }))
  let t = M.track({}, at(100, 5000), 0)
  t = M.track(t, at(200, 5000), 1000)
  eq(M.stalledKeys(t, at(200, 5000), 1000, 20000), {})
  t = M.track(t, at(200, 10), 15000)
  eq(M.stalledKeys(t, at(200, 10), 15000, 20000), {})
  eq(Object.keys(M.stalledKeys(t, at(200, 10), 21000, 20000)), ["epic:dc9d2e595d0e4650b35d659f90d41059"])
  // Still reporting speed (the EWMA hasn't decayed): not stalled yet.
  eq(M.stalledKeys(t, at(200, 4096), 21000, 20000), {})
  t = M.track(t, at(300, 2000), 22000)
  eq(M.stalledKeys(t, at(300, 2000), 22000, 20000), {})
})

test("a phase change (download → verify) restarts the clock", () => {
  let t = M.track({}, items(epic({ state: "downloading", done_bytes: 1000, total_bytes: 1000 })), 0)
  t = M.track(t, items(epic({ state: "verifying", done_bytes: 0, total_bytes: 1000 })), 50000)
  eq(M.stalledKeys(t, items(epic({ state: "verifying", done_bytes: 0, total_bytes: 1000 })), 60000, 20000), {})
})

test("inactive items are not tracked", () => {
  eq(M.track({}, items(entry({ state: "paused" })), 0), {})
})

// ---- formatting ---------------------------------------------------------------------------

test("percent rounds down and stays within 0..100", () => {
  assert.equal(M.percent(0.2999), 29)
  assert.equal(M.percent(0.29), 29)
  assert.equal(M.percent(0.999), 99)
  assert.equal(M.percent(1), 100)
  assert.equal(M.percent(-1), 0)
  assert.equal(M.percent(NaN), 0)
})

test("bytes and rates in binary units, numbers through the formatter", () => {
  assert.equal(M.bytes(0), "0 B")
  assert.equal(M.bytes(1023), "1023 B")
  assert.equal(M.bytes(31699451644), "29.5 GiB")
  assert.equal(M.rate(25922326), "24.7 MiB/s")
  const danish = (v, d) => v.toFixed(d).replace(".", ",")
  assert.equal(M.rate(25922326, danish), "24,7 MiB/s")
})

test("ETA and durations", () => {
  assert.equal(M.eta(100, 0), -1)
  assert.equal(M.eta(100, 100), -1)            // under 1 KiB/s: no telling
  assert.equal(M.eta(0, 4096), -1)
  assert.equal(M.eta(4096 * 90, 4096), 90)
  assert.equal(M.eta(1e15, 2048), -1)          // centuries
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
  // Alan Wake 2 left the queue at 99.5%: finished. Odyssey left at 1%: removed.
  // Product 9 failed.
  const after = items(entry({ product_id: 9, state: "failed", error: "<b>no space</b>", position: 1 }))
  const t = M.transitions(dl, after, {})
  eq(t.map((x) => [x.type, x.item.ref]), [["finished", "epic:dc9d2e595d0e4650b35d659f90d41059"], ["failed", "9"]])
  assert.equal(t[1].item.error, "<b>no space</b>")
  // Something we removed ourselves isn't "finished" even if it was nearly done.
  eq(M.transitions(dl, after, { "epic:dc9d2e595d0e4650b35d659f90d41059": true }).map((x) => x.type), ["failed"])
  // No previous snapshot (startup): nothing to report.
  eq(M.transitions(null, after, {}), [])
  // Still failed: reported once.
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

console.log(`ok - ${n} model tests`)
