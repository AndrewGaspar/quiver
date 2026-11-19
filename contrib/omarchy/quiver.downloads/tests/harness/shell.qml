// Render harness for the Quiver downloads widget (run by tests/render.sh, which
// links the shell's Ui/Commons and this plugin next to this file). It drives
// the real Service, Widget and Body with canned watch lines, checks what the
// bar would show for each state, and saves screenshots of the bar (horizontal
// and vertical) and the panel. Nothing here starts quiverd or the app, or sends
// notifications: `quiver` is tests/harness/fake-quiver.

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
    return JSON.stringify({ connected: true, queue: queue, settings: { max_active: 1, bandwidth_limit: 26214400, auto_update: true } })
  }
  readonly property var odyssey: h.entry({ position: 2, done_bytes: 140694473, total_bytes: 102383586228 })
  readonly property var valhalla: h.entry({ product_id: 13504, name: "Assassin's Creed® Valhalla", position: 3 })
  readonly property var markup: h.entry({ product_id: 4242, name: "<img src=x onerror=alert(1)> <b>Not bold</b> &amp;", position: 4 })
  readonly property var downloading: h.aw2({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 25922326, retries: 2 })

  // Make every active item look like it stopped a minute ago.
  function ageTracking() {
    var t = {}
    for (var k in service.tracked) t[k] = { done: service.tracked[k].done, since: Date.now() - 60000, state: service.tracked[k].state }
    service.tracked = t
    service.stalled = Model.stalledKeys(t, service.items, Date.now(), service.stallMs)
  }

  readonly property var scenarios: [
    {
      name: "downloading",
      apply: function () { service.handleLine(h.line([h.downloading, h.odyssey, h.valhalla, h.markup])) },
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
      apply: function () { service.handleLine(h.line([h.downloading, h.odyssey])) },
      expect: function () { h.check(hw.labelText === "29% 24.7 MiB/s", "label '" + hw.labelText + "'") }
    },
    {
      name: "verifying",
      apply: function () {
        service.handleLine(h.line([h.aw2({ state: "verifying", position: 1, done_bytes: 80000000000, total_bytes: 105865248129, speed: 900000000 }), h.valhalla]))
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
        service.handleLine(h.line([h.aw2({ state: "downloading", position: 1, done_bytes: 31699451644, total_bytes: 105865248129, speed: 0, retries: 7 }), h.odyssey]))
        h.ageTracking()
      },
      expect: function () {
        h.check(hw.phase === "stalled", "phase " + hw.phase)
        h.check(hw.urgentState, "urgent")
        h.check(hw.labelText === "29%", "label '" + hw.labelText + "'")
      }
    },
    {
      name: "paused",
      apply: function () {
        service.handleLine(h.line([h.aw2({ state: "paused", position: 1, done_bytes: 31699451644, total_bytes: 105865248129 }), h.entry({ state: "paused", position: 2 })]))
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
      apply: function () { service.handleLine(h.line([h.odyssey, h.valhalla])) },
      expect: function () {
        h.check(hw.phase === "queued", "phase " + hw.phase)
        h.check(hw.markFor(hw.phase) === "dots", "dots mark")
      }
    },
    {
      name: "failed",
      apply: function () {
        service.handleLine(h.line([
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
        service.handleLine(h.line([h.downloading, h.entry({ state: "failed", position: 2, error: "manifest download failed: HTTP 403" })]))
      },
      expect: function () {
        h.check(hw.phase === "active", "phase " + hw.phase)
        h.check(hw.hasFailed, "failure badge")
      }
    },
    {
      name: "action-error",
      apply: function () {
        service.handleLine(h.line([h.entry({ product_id: 99999, name: "Far Cry® 6", state: "paused", position: 1 })]))
        service.resume("99999")
      },
      wait: 900,
      expect: function () {
        h.check(service.lastError.indexOf("resume 99999") !== -1, "action error shown: " + service.lastError)
        h.check(service.lastError.indexOf("not in the queue") !== -1, "stderr reason shown")
      }
    },
    {
      name: "idle",
      apply: function () { service.handleLine(h.line([])) },
      expect: function () {
        h.check(hw.phase === "idle", "phase " + hw.phase)
        h.check(!hw.visible, "hidden when idle")
      }
    },
    {
      name: "idle-shown",
      settings: { showWhenIdle: true },
      apply: function () { service.handleLine(h.line([])) },
      expect: function () { h.check(hw.visible, "showWhenIdle keeps it") }
    },
    {
      name: "down",
      apply: function () { service.handleLine('{"connected":false,"error":"quiverd is not running (No such file or directory (os error 2))"}') },
      expect: function () {
        h.check(hw.phase === "down", "phase " + hw.phase)
        h.check(!hw.visible, "hidden while quiverd is down")
      }
    },
    {
      name: "unavailable",
      settings: { showWhenIdle: true },
      apply: function () {
        service.connected = false
        service.unavailable = true
        service.cliError = "error: unexpected argument '--watch' found"
      },
      expect: function () { h.check(hw.phase === "unavailable", "phase " + hw.phase) }
    }
  ]

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
    service.lastError = ""
    service.unavailable = false
    service.cliError = ""
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
    service.handleLine(h.line([h.downloading, h.odyssey, h.valhalla, h.entry({ product_id: 0, game: "epic:$(touch /tmp/pwned)", name: "Evil", position: 5 })]))
    service.pauseAll()
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
    implicitHeight: 1000
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

      // The panel's card as KeyboardPanel draws it.
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
