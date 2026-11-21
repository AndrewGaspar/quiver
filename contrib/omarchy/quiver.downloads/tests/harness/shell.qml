// Render harness for the Quiver downloads widget (run by tests/render.sh, which
// links the shell's Ui/Commons and this plugin next to this file). It drives
// the real Service, Widget and Body with canned queues (what `quiver queue
// --json` prints), checks what the bar would show for each state, and saves
// screenshots of the bar (horizontal and vertical) and the panel. What is
// playing comes in as `quiver playing --json` lines. The poll and follow
// scenarios run the Service's own processes against stand-ins: fake-quiver
// (serves tests/fixtures/live-queue.json and playing.json), fake-quiver-down
// (quiverd isn't running) and fake-quiver-old (a CLI without --no-start or
// `playing`). Nothing here starts quiverd or the app, or sends notifications.

import QtQuick
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "plugin" as Plugin
import "plugin/Model.js" as Model

ShellRoot {
  id: h

  readonly property string outDir: Quickshell.env("HARNESS_OUT")
  readonly property string fake: Quickshell.env("HARNESS_FAKE_QUIVER")
  readonly property string fakeDown: h.fake + "-down"
  readonly property string fakeOld: h.fake + "-old"
  readonly property var initialSettings: ({ notify: false, quiverPath: h.fake })
  property var failures: []
  property int checks: 0

  function check(cond, msg) {
    h.checks += 1
    if (!cond) {
      h.failures.push(h.current + ": " + msg)
      console.log("FAIL " + h.current + ": " + msg)
    }
  }

  // ---- canned queue ------------------------------------------------------------

  function entry(extra) {
    var e = {
      product_id: 5059, name: "Assassin's Creed® Odyssey", kind: "install", state: "queued", position: 1,
      job_id: 2, language: "en-US", account: null, source: "gui", added: "2026-11-19T21:38:03Z",
      error: null, done_bytes: 0, total_bytes: 0, speed: 0, retries: 0
    }
    for (var k in extra) e[k] = extra[k]
    return e
  }
  function aw2(extra) {
    var e = h.entry({ product_id: 0, game: "epic:dc9d2e595d0e4650b35d659f90d41059", name: "Alan Wake 2" })
    for (var k in extra) e[k] = extra[k]
    return e
  }
  function line(queue) {
    return JSON.stringify(queue, null, 2)
  }
  // `throughput` as quiverd sends it: n one-second samples of `bytes`.
  function tp(n, bytes, state) {
    var samples = []
    for (var i = 0; i < n; i++) samples.push({ bytes: bytes, state: state || "downloading" })
    return { game: "x", interval_ms: 1000, at: "2026-11-19T22:58:04Z", samples: samples }
  }
  readonly property var odyssey: h.entry({ position: 2, done_bytes: 140694473, total_bytes: 102383586228 })
  readonly property var valhalla: h.entry({ product_id: 13504, name: "Assassin's Creed® Valhalla", position: 3 })
  readonly property var markup: h.entry({ product_id: 4242, name: "<img src=x onerror=alert(1)> <b>Not bold</b> &amp;", position: 4 })
  // A line of `quiver playing --json`: Origins running for 72 min with the Pharos profile,
  // streaming to roth through Moonlight, and three recent games.
  function playingLine(game, extra) {
    var g = {
      ref: "3539", name: "Assassin's Creed Origins", short: "AC Origins",
      started: new Date(Date.now() - 72 * 60000).toISOString(), profile: "Pharos", via: ["launcher"],
      session: "s-pzfkk4bl2v", stream: { peer: "roth", client: "moonlight", state: "streaming" },
      icon: h.iconDir + "/odyssey.png"
    }
    for (var k in game) g[k] = game[k]
    var v = {
      connected: true,
      playing: [g],
      recent: [
        { ref: "420", name: "Far Cry 4", short: "Far Cry 4", last_played: new Date(Date.now() - 14 * 60000).toISOString(), icon: h.iconDir + "/farcry.png" },
        { ref: "epic:dc9d2e595d0e4650b35d659f90d41059", name: "Alan Wake 2", short: "Alan Wake 2", last_played: new Date(Date.now() - 3 * 3600000).toISOString() },
        { ref: "5059", name: "Assassin's Creed Odyssey", short: "AC Odyssey", last_played: new Date(Date.now() - 26 * 3600000).toISOString(), icon: h.iconDir + "/odyssey.png" }
      ]
    }
    for (var e in extra) v[e] = extra[e]
    return JSON.stringify(v)
  }
  readonly property string iconDir: Quickshell.env("HARNESS_ICONS")
  readonly property string longTitle: "Tom Clancy's Rainbow Six Extraction Deluxe Edition"

  readonly property var downloading: h.aw2({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 3, retries: 2, throughput: h.tp(60, 25922326) })

  readonly property var scenarios: [
    {
      name: "downloading",
      apply: function () { service.handleQueue(h.line([h.downloading, h.odyssey, h.valhalla, h.markup])) },
      expect: function () {
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(hw.visible, "bar widget visible")
        h.check(hw.labelText === "29%", "label '" + hw.labelText + "'")
        h.check(vw.labelText === "", "vertical bar has no label")
        h.check(hw.markFor(hw.phase) === "arrow", "arrow mark")
        h.check(body.rows.length === 4, "4 rows, got " + body.rows.length)
        h.check(body.rows[3].name === "<img src=x onerror=alert(1)> <b>Not bold</b> &amp;", "markup name kept literal")
        h.check(hw.tooltip.indexOf("Alan Wake 2") === 0, "tooltip names the download: " + hw.tooltip)
      }
    },
    {
      name: "label-both",
      settings: { label: "both" },
      apply: function () { service.handleQueue(h.line([h.downloading, h.odyssey])) },
      expect: function () { h.check(hw.labelText === "29% 25.9 MB/s", "label '" + hw.labelText + "'") }
    },
    {
      name: "verifying",
      apply: function () {
        service.handleQueue(h.line([h.aw2({ state: "verifying", position: 1, done_bytes: 80000000000, total_bytes: 105865248129, speed: 900000000, throughput: h.tp(30, 900000000, "verifying") }), h.valhalla]))
      },
      expect: function () {
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(hw.markFor(hw.phase) === "check", "check mark")
        h.check(hw.labelText === "75%", "label '" + hw.labelText + "'")
      }
    },
    {
      name: "stalled",
      apply: function () {
        var t = h.tp(60, 0, "stalled")
        for (var i = 0; i < 40; i++) t.samples[i] = { bytes: 25000000, state: "downloading" }
        for (var j = 52; j < 60; j++) t.samples[j] = { bytes: 0, state: "retrying", retries: 1 }
        service.handleQueue(h.line([h.aw2({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 2, retries: 7, throughput: t }), h.odyssey]))
      },
      expect: function () {
        h.check(hw.phase === "stalled", "phase " + hw.phase)
        h.check(hw.urgentState, "urgent")
        h.check(hw.labelText === "29%", "label '" + hw.labelText + "'")
        h.check(body.heroMeta.indexOf("left") === -1, "no time left while stalled: " + body.heroMeta)
      }
    },
    {
      name: "stalled-speed-label",
      settings: { label: "speed" },
      apply: function () {
        service.handleQueue(h.line([h.aw2({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 2, throughput: h.tp(60, 0, "stalled") })]))
      },
      expect: function () { h.check(hw.labelText === "Stalled", "label '" + hw.labelText + "'") }
    },
    {
      name: "paused",
      apply: function () {
        service.handleQueue(h.line([h.aw2({ state: "paused", position: 1, done_bytes: 31699451644, total_bytes: 105865248129 }), h.entry({ state: "paused", position: 2 })]))
      },
      expect: function () {
        h.check(hw.phase === "paused", "phase " + hw.phase)
        h.check(hw.visible, "visible")
        h.check(hw.labelText === "", "no label")
        h.check(hw.markFor(hw.phase) === "pause", "pause mark")
        h.check(service.canResumeAll && !service.canPauseAll, "resume all offered")
      }
    },
    {
      name: "queued",
      apply: function () { service.handleQueue(h.line([h.odyssey, h.valhalla])) },
      expect: function () {
        h.check(hw.phase === "queued", "phase " + hw.phase)
        h.check(hw.markFor(hw.phase) === "dots", "dots mark")
      }
    },
    {
      name: "failed",
      apply: function () {
        service.handleQueue(h.line([
          h.aw2({ state: "failed", position: 1, done_bytes: 31699451644, total_bytes: 105865248129,
                  error: "writing Data/Win64/content_a.bin: No space left on device (os error 28); free 74.2 GiB on /home/ajg/Games and resume" }),
          h.entry({ state: "paused", position: 2 })
        ]))
      },
      expect: function () {
        h.check(hw.phase === "failed", "phase " + hw.phase)
        h.check(hw.urgentState, "urgent")
        h.check(hw.markFor(hw.phase) === "bang", "bang mark")
        h.check(hw.tooltip.indexOf("No space left") !== -1, "tooltip has the error")
      }
    },
    {
      name: "active-and-failed",
      apply: function () {
        service.handleQueue(h.line([h.downloading, h.entry({ state: "failed", position: 2, error: "manifest download failed: HTTP 403" })]))
      },
      expect: function () {
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(hw.hasFailed, "failure badge")
      }
    },
    {
      name: "action-error",
      apply: function () {
        service.handleQueue(h.line([h.entry({ product_id: 99999, name: "Far Cry® 6", state: "paused", position: 1 })]))
        service.resume("99999")
      },
      wait: 900,
      expect: function () {
        h.check(service.lastError.indexOf("resume 99999") !== -1, "action error shown: " + service.lastError)
        h.check(service.lastError.indexOf("not in the queue") !== -1, "stderr reason shown")
      }
    },
    {
      name: "idle-hidden",
      settings: { showWhenIdle: false },
      apply: function () { service.handleQueue(h.line([])) },
      expect: function () {
        h.check(hw.phase === "idle", "phase " + hw.phase)
        h.check(!hw.visible, "hidden when idle with showWhenIdle off")
      }
    },
    {
      name: "idle",
      apply: function () { service.handleQueue(h.line([])) },
      expect: function () {
        h.check(hw.visible, "the mark stays by default")
        h.check(hw.labelText === "", "no label: '" + hw.labelText + "'")
        h.check(!hw.queueBusy, "plain mark")
        h.check(hw.tooltip === "Quiver: nothing playing or downloading", "tooltip: " + hw.tooltip)
      }
    },
    {
      name: "down",
      apply: function () { service.handleFailure("down", "Error: quiverd is not running (No such file or directory (os error 2))") },
      expect: function () {
        h.check(hw.phase === "down", "phase " + hw.phase)
        h.check(hw.visible, "the mark stays while quiverd is down")
      }
    },
    {
      name: "down-hidden",
      settings: { showWhenIdle: false },
      apply: function () { service.handleFailure("down", "Error: quiverd is not running (No such file or directory (os error 2))") },
      expect: function () { h.check(!hw.visible, "hidden while quiverd is down with showWhenIdle off") }
    },
    // ---- now playing, recent games
    {
      name: "playing",
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({}))
      },
      expect: function () {
        h.check(hw.visible, "visible")
        h.check(hw.labelText === "AC Origins", "label '" + hw.labelText + "'")
        h.check(!hw.titleOverflows, "a short title fits")
        h.check(vw.labelText === "", "vertical bar has no label")
        h.check(hw.tooltip.indexOf("Now playing: Assassin's Creed Origins · 1 h 12 min") === 0, "tooltip: " + hw.tooltip)
        h.check(hw.tooltip.indexOf("Streaming to roth (Moonlight)") !== -1, "tooltip has the stream: " + hw.tooltip)
        h.check(body.playingRows.length === 1, "one playing card")
        h.check(body.playingDetail(body.playingRows[0]) === "1 h 12 min · Profile Pharos", "detail: " + body.playingDetail(body.playingRows[0]))
        h.check(body.recentRows.length === 3, "three recent games, got " + body.recentRows.length)
        h.check(body.targetCount === 3 + 3 + 2, "cursor targets: " + body.targetCount)
        h.check(body.heroMeta === "Playing AC Origins", "hero: " + body.heroMeta)
      }
    },
    {
      name: "playing-downloading",
      apply: function () {
        service.handleQueue(h.line([h.downloading, h.odyssey]))
        service.handlePlaying(h.playingLine({ stream: null, profile: null }))
      },
      expect: function () {
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(hw.labelText === "AC Origins", "the title wins: '" + hw.labelText + "'")
        h.check(hw.tooltip.indexOf("Alan Wake 2") !== -1, "tooltip has the download: " + hw.tooltip)
        h.check(body.rowsStart === 3 && body.recentStart === 5, "targets: " + body.rowsStart + "/" + body.recentStart)
      }
    },
    {
      name: "playing-label-percent",
      settings: { label: "percent" },
      apply: function () {
        service.handleQueue(h.line([h.downloading]))
        service.handlePlaying(h.playingLine({}))
      },
      expect: function () { h.check(hw.labelText === "29%", "label '" + hw.labelText + "'") }
    },
    {
      name: "playing-long-title",
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({ ref: "6000", name: h.longTitle, short: h.longTitle, stream: { peer: "roth", client: "quiver", state: "kept" }, icon: "" }))
      },
      expect: function () {
        h.check(hw.labelText === h.longTitle, "the whole title: '" + hw.labelText + "'")
        h.check(hw.titleOverflows, "cut to the slot")
        h.check(!hw.titleScrolling, "still until hovered")
        h.check(hw.tooltip.indexOf("Kept for a resume from roth (Quiver)") !== -1, "kept stream: " + hw.tooltip)
      }
    },
    {
      name: "playing-marquee",
      marquee: true,
      wait: 2400,
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({ ref: "6000", name: h.longTitle, short: h.longTitle, icon: "" }))
      },
      expect: function () { h.check(hw.titleScrolling, "scrolls on hover") }
    },
    {
      name: "playing-markup",
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({ name: "<b>Bold</b> <img src=x>", short: "<b>B</b>", profile: "<i>p</i>" }))
      },
      expect: function () {
        h.check(hw.labelText === "<b>B</b>", "label kept literal")
        h.check(body.playingRows[0].name === "<b>Bold</b> <img src=x>", "name kept literal")
      }
    },
    {
      name: "recent-only",
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({}, { playing: [] }))
      },
      expect: function () {
        h.check(hw.labelText === "", "no label")
        h.check(body.recentRows.length === 3, "recent games listed")
        h.check(body.playingRows.length === 0, "nothing playing")
        h.check(body.heroMeta === "Nothing to download", "hero: " + body.heroMeta)
      }
    },
    {
      name: "follow-disconnected",
      apply: function () {
        service.handleQueue(h.line([]))
        service.handlePlaying(h.playingLine({}))
        service.handlePlaying(JSON.stringify({ connected: false, playing: [], recent: [] }))
      },
      expect: function () {
        h.check(!service.followConnected, "disconnected")
        h.check(service.playing.length === 0, "nothing playing while quiverd is away")
        h.check(service.recent.length === 3, "recent games kept")
      }
    },
    {
      name: "unavailable",
      settings: { showWhenIdle: true },
      apply: function () { service.handleFailure("unavailable", "error: unexpected argument '--no-start' found") },
      expect: function () { h.check(hw.phase === "unavailable", "phase " + hw.phase) }
    },
    // The Service's own polling, against the stand-ins.
    {
      name: "poll-live",
      apply: function () { h.pollWith(h.fake) },
      wait: 1500,
      expect: function () {
        h.check(service.connected && service.downs === 0, "connected")
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(service.items.length === 3, "3 items, got " + service.items.length)
        h.check(service.summary.speed > 1000000, "a speed from the samples: " + service.summary.speed)
        h.check(!poll().running, "the poll finished")
      }
    },
    {
      name: "poll-down",
      settings: { showWhenIdle: false },
      apply: function () { h.pollWith(h.fakeDown) },
      wait: 1500,
      expect: function () {
        h.check(hw.phase === "down", "phase " + hw.phase)
        h.check(service.downs === 1, "one failed poll: " + service.downs)
        h.check(service.downReason.indexOf("quiverd is not running") === 0, "reason: " + service.downReason)
        h.check(!hw.visible, "hidden while quiverd is down")
      }
    },
    {
      name: "poll-old-cli",
      settings: { showWhenIdle: true },
      apply: function () { h.pollWith(h.fakeOld) },
      wait: 1500,
      expect: function () {
        h.check(hw.phase === "unavailable", "phase " + hw.phase)
        h.check(service.downReason.indexOf("--no-start") !== -1, "reason: " + service.downReason)
      }
    },
    // The follow (`quiver playing --follow`), against the stand-ins.
    {
      name: "follow-live",
      apply: function () { h.followWith(h.fake) },
      wait: 1200,
      expect: function () {
        h.check(service.followConnected, "connected")
        h.check(service.playing.length === 1 && service.playing[0].short === "Alan Wake 2", "playing from the follow")
        h.check(service.recent.length === 2, "recent from the follow")
        h.check(hw.labelText === "Alan Wake 2", "label '" + hw.labelText + "'")
        h.check(follow() && follow().running, "the follow keeps running")
      }
    },
    {
      name: "follow-old-cli",
      apply: function () { h.followWith(h.fakeOld) },
      wait: 1200,
      expect: function () {
        h.check(!service.followConnected, "not connected")
        h.check(service.followDowns === 1, "one failed follow: " + service.followDowns)
        h.check(service.playing.length === 0, "nothing playing")
        h.check(follow() && !follow().running, "not stuck running")
      }
    },
    {
      name: "poll-missing-cli",
      settings: { showWhenIdle: true },
      apply: function () { h.pollWith("/nonexistent/quiver") },
      wait: 1500,
      expect: function () {
        h.check(hw.phase === "unavailable", "phase " + hw.phase)
        h.check(!poll().running, "not stuck running")
      }
    }
  ]

  // The Service's poll Process, found by its command.
  function poll() {
    for (var i = 0; i < service.data.length; i++) {
      var c = service.data[i]
      if (c && c.command !== undefined && c.command.indexOf("--no-start") !== -1) return c
    }
    return null
  }
  // The Service's follow Process, found by its command.
  function follow() {
    for (var i = 0; i < service.data.length; i++) {
      var c = service.data[i]
      if (c && c.command !== undefined && c.command.indexOf("playing") !== -1) return c
    }
    return null
  }
  function followWith(path) {
    service.quiverPath = path
    service.followDowns = 0
    service.following = true
    service.startFollow()
  }
  function pollWith(path) {
    service.quiverPath = path
    service.downs = 0
    service.pollNow()
  }

  property int step: -1
  property string current: ""

  function next() {
    h.step += 1
    if (h.step >= h.scenarios.length) return h.bulkActions()
    var sc = h.scenarios[h.step]
    h.current = sc.name
    var s = { notify: false, quiverPath: h.fake }
    if (sc.settings) for (var k in sc.settings) s[k] = sc.settings[k]
    hw.settings = s
    vw.settings = s
    hw.previewMarquee = !!sc.marquee
    // The panel is on screen here: action errors show in it, not as notifications.
    service.panelOpen = true
    // The follow runs only in its own scenarios.
    service.following = false
    var f = h.follow()
    if (f && f.running) f.running = false
    service.playing = []
    service.recent = []
    service.followConnected = false
    service.lastError = ""
    service.unavailable = false
    service.downs = 0
    service.quiverPath = h.fake
    service.tracked = ({})
    service.stalled = ({})
    sc.apply()
    settle.interval = sc.wait || 350
    settle.restart()
  }

  Timer {
    id: settle
    onTriggered: {
      var sc = h.scenarios[h.step]
      sc.expect()
      h.capture(sc.name)
    }
  }

  function save(item, file, then) {
    if (h.outDir === "") return then()
    item.grabToImage(function (r) {
      if (!r.saveToFile(h.outDir + "/" + file)) h.check(false, "could not save " + file)
      then()
    })
  }

  function capture(name) {
    h.save(hstrip, name + "-bar.png", function () {
      h.save(vstrip, name + "-vbar.png", function () {
        h.save(card, name + "-panel.png", function () { Qt.callLater(h.next) })
      })
    })
  }

  // Pause all: queued items first, then the active one; bad refs never reach argv.
  function bulkActions() {
    h.current = "bulk"
    service.quiverPath = h.fake
    service.handleQueue(h.line([h.downloading, h.odyssey, h.valhalla, h.entry({ product_id: 0, game: "epic:$(touch /tmp/pwned)", name: "Evil", position: 5 })]))
    service.pauseAll()
    // Play a recent game (and one that isn't recent: nothing runs), stop the running one,
    // and Sight over it; each ref goes through as its own argument.
    service.handlePlaying(h.playingLine({}))
    service.play("420")
    service.play("99")
    service.stop("3539")
    service.stop("420")
    service.sight("3539")
    service.openApp("downloads")
    bulkDone.start()
  }
  Timer {
    id: bulkDone
    interval: 1500
    onTriggered: {
      h.check(!service.busy, "bulk actions finished")
      h.finish()
    }
  }

  function finish() {
    console.log("CHECKS " + h.checks)
    console.log(h.failures.length === 0 ? "RESULT pass" : "RESULT fail " + h.failures.length)
    Qt.quit()
  }

  // ---- the plugin under test ------------------------------------------------------

  Plugin.Service {
    id: service
    quiverPath: h.fake
    notify: false
    polling: false
    following: false
  }

  QtObject {
    id: shellApi
    function serviceFor(id) { return id === "quiver.downloads" ? service : null }
  }

  component FakeBar: QtObject {
    property var shell: shellApi
    property bool vertical: false
    property int barSize: Style.bar.sizeHorizontal
    property string position: "top"
    property color foreground: Commons.Color.foreground
    property color barForeground: Commons.Color.bar.text
    property color background: Commons.Color.bar.background
    property color urgent: Commons.Color.bar.active
    property string fontFamily: Style.font.family
    property bool foregroundAnimationEnabled: false
    property var activePopout: null
    function registerClickTarget(t) {}
    function unregisterClickTarget(t) {}
    function showTooltip(t, x) {}
    function hideTooltip(t) {}
    function requestPopout(o) {}
    function releasePopout(o) {}
    function moduleWidgets(id) { return [] }
    function switchPanelFrom(o, d) { return false }
    function notePopoutCard(c) {}
    function popoutCardFor(o) { return null }
    function targetBelongsToWindow(t, w) { return true }
  }
  FakeBar { id: hbar }
  FakeBar { id: vbar; vertical: true; barSize: Style.bar.sizeVertical; position: "right" }

  FloatingWindow {
    implicitWidth: 520
    implicitHeight: 1500
    color: Commons.Color.background

    Column {
      x: 12
      y: 12
      spacing: 12

      // A slice of the bar: a neighbour glyph for scale, then the widget.
      Rectangle {
        id: hstrip
        width: 260
        height: hbar.barSize
        color: Commons.Color.bar.background
        Row {
          anchors.right: parent.right
          anchors.rightMargin: 8
          height: parent.height
          Plugin.Widget { id: hw; bar: hbar; settings: h.initialSettings; height: parent.height }
          BarIconButton { bar: hbar; text: "󰖩" }
          BarIconButton { bar: hbar; text: "󰕾" }
        }
      }

      Rectangle {
        id: vstrip
        width: vbar.barSize
        height: 110
        color: Commons.Color.bar.background
        Column {
          anchors.horizontalCenter: parent.horizontalCenter
          y: 8
          BarIconButton { bar: vbar; text: "󰖩" }
          Plugin.Widget { id: vw; bar: vbar; settings: h.initialSettings; width: vbar.barSize }
          BarIconButton { bar: vbar; text: "󰕾" }
        }
      }

      // The panel's card as KeyboardPanel draws it (its height capped as the shell does).
      Rectangle {
        id: card
        readonly property int pad: Style.spacing.popupPadding
        width: Style.space(400) + pad * 2
        height: body.implicitHeight + pad * 2
        color: Commons.Color.popups.background
        border.color: Commons.Color.popups.border
        border.width: Math.max(1, Style.space(2))
        radius: Style.cornerRadius
        Item {
          x: card.pad
          y: card.pad
          width: card.width - card.pad * 2
          height: card.height - card.pad * 2
          Plugin.Body { id: body; widget: hw; service: service }
        }
      }
    }
  }

  Timer {
    interval: 600
    running: true
    onTriggered: h.next()
  }
}
