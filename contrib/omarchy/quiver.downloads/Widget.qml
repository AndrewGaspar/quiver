// Quiver's downloads in the bar: a progress ring with the percentage (and/or
// speed) while something downloads, a quiet mark while the queue waits or is
// paused, an urgent one when a download failed or stalled, and nothing at all
// when the queue is empty. Click for the queue, with pause/resume and a way
// into Quiver.
//
// The queue lives in this plugin's service (Service.qml), which exists once
// however many bars there are; this only draws it and forwards clicks.

import QtQuick
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

Panel {
  id: root
  moduleName: "quiver.downloads"
  ipcTarget: "quiver.downloads.panel"

  // --- the service -------------------------------------------------------------

  property var service: null
  function lookupService() {
    var shell = root.bar ? root.bar.shell : null
    var s = shell && typeof shell.serviceFor === "function" ? shell.serviceFor("quiver.downloads") : null
    if (s !== root.service) root.service = s
  }
  onBarChanged: lookupService()
  Component.onCompleted: lookupService()
  // The service can come up after the bar widget; look until it is there.
  Timer {
    interval: 1000
    repeat: true
    running: root.service === null
    onTriggered: root.lookupService()
  }

  readonly property bool notifySetting: root.setting("notify", true) !== false
  readonly property string quiverPathSetting: String(root.setting("quiverPath", "") || "")
  onNotifySettingChanged: if (root.service) root.service.notify = root.notifySetting
  onQuiverPathSettingChanged: if (root.service) root.service.quiverPath = root.quiverPathSetting
  onServiceChanged: if (root.service) {
    root.service.notify = root.notifySetting
    root.service.quiverPath = root.quiverPathSetting
  }

  // --- what to show --------------------------------------------------------------

  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }
  property alias strings: strings

  readonly property var summary: root.service ? root.service.summary : Model.summarize([], {})
  readonly property string phase: root.service ? root.service.phase : "down"
  readonly property bool verifyingOnly: {
    var a = root.summary.active
    if (a.length === 0) return false
    for (var i = 0; i < a.length; i++) if (a[i].state !== "verifying") return false
    return true
  }
  readonly property bool hasFailed: root.summary.failed.length > 0
  // A vertical bar has room for the ring only (like the built-in widgets' labels).
  readonly property bool vertical: !!root.bar && root.bar.vertical === true

  // "percent" | "speed" | "both" | "none"
  readonly property string labelMode: {
    var m = String(root.setting("label", "percent"))
    return m === "speed" || m === "both" || m === "none" ? m : "percent"
  }
  readonly property bool showWhenIdle: root.setting("showWhenIdle", false) === true
  readonly property bool shown: root.service !== null
    && (root.showWhenIdle || (root.phase !== "idle" && root.phase !== "down" && root.phase !== "unavailable"))

  readonly property color barColor: root.bar ? root.bar.barForeground : Commons.Color.foreground
  readonly property color barUrgent: root.bar ? root.bar.urgent : Commons.Color.urgent
  readonly property bool urgentState: root.phase === "stalled" || root.phase === "failed"

  function num(v, d) { return Commons.I18n.formatNumber(v, d) }
  function rateText(n) { return Model.rateText(n, root.num) }
  function bytesText(n) { return Model.bytes(n, root.num) }
  function percentText(fraction) { return strings.t("progress.percent").arg(root.num(Model.percent(fraction), 0)) }
  function etaText(seconds) {
    if (seconds < 0) return ""
    var p = Model.durationParts(seconds)
    if (p.kind === "hm") return strings.t("eta.hm").arg(root.num(p.h, 0)).arg(root.num(p.m, 0))
    if (p.kind === "m") return strings.t("eta.m").arg(root.num(p.m, 0))
    return strings.t("eta.lt1m")
  }

  // The mark inside the ring.
  function markFor(phase) {
    if (phase === "active" || phase === "stalled") return root.verifyingOnly ? "check" : "arrow"
    if (phase === "failed") return "bang"
    if (phase === "paused") return "pause"
    if (phase === "queued") return "dots"
    return "none"
  }

  readonly property string labelText: {
    if (root.vertical || root.labelMode === "none") return ""
    if (root.phase !== "active" && root.phase !== "stalled") return ""
    var pct = root.percentText(root.summary.fraction)
    // Near zero, the speed says "stalled" rather than a number.
    var speed = root.phase === "stalled" ? strings.t("bar.stalled") : root.rateText(root.summary.speed)
    if (root.labelMode === "speed") return speed
    if (root.labelMode === "both") return pct + " " + speed
    return pct
  }

  readonly property string phaseText: {
    switch (root.phase) {
    case "active": return root.verifyingOnly ? strings.t("phase.verifying") : strings.t("phase.active")
    case "stalled": return strings.t("phase.stalled")
    case "failed": return strings.t("phase.failed")
    case "paused": return strings.t("phase.paused")
    case "queued": return strings.t("phase.queued")
    case "down": return strings.t("phase.down")
    case "unavailable": return strings.t("phase.unavailable")
    }
    return strings.t("phase.idle")
  }

  readonly property string tooltip: {
    var s = root.summary
    if (root.phase === "active" || root.phase === "stalled") {
      var lines = []
      for (var i = 0; i < s.active.length && i < 3; i++) lines.push(s.active[i].name)
      var detail = [root.phaseText, root.percentText(s.fraction)]
      if (root.phase === "active") {
        detail.push(root.rateText(s.speed))
        var eta = root.etaText(s.eta)
        if (eta !== "") detail.push(eta)
      }
      lines.push(detail.join(" · "))
      if (s.waiting.length > 0) lines.push(strings.tn("queue.count", s.count).arg(root.num(s.count, 0)))
      return lines.join("\n")
    }
    if (root.phase === "failed") {
      var f = s.failed[0]
      return root.phaseText + "\n" + f.name + (f.error !== "" ? "\n" + f.error : "")
    }
    if (root.phase === "paused" || root.phase === "queued")
      return root.phaseText + "\n" + strings.tn("queue.count", s.count).arg(root.num(s.count, 0))
    if (root.phase === "idle") return strings.t("tooltip.idle")
    return root.phaseText
  }

  // --- the bar ----------------------------------------------------------------------

  visible: root.shown
  implicitWidth: root.shown ? button.implicitWidth : 0
  implicitHeight: root.shown ? button.implicitHeight : 0

  onShownChanged: if (!root.shown && root.opened) root.close()

  TextMetrics {
    id: labelMetrics
    font.family: root.bar ? root.bar.fontFamily : Style.font.family
    font.pixelSize: Style.bar.iconFont
    text: root.labelText
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    labelText: root.labelText
    // A label widens the slot by its own width, so the ring stays put as digits change
    // (the bar font is monospace).
    slotSize: root.labelText === "" ? Style.bar.iconSlot
      : Math.ceil(Style.bar.iconSlot + labelMetrics.advanceWidth + Style.space(4))
    tooltipText: root.tooltip
    dimmed: root.phase === "paused" || root.phase === "queued" || root.phase === "idle"
    // The label takes the ring's colour: urgent while stalled.
    active: root.urgentState
    iconComponent: Component {
      Item {
        width: 16
        height: 16
        Ring {
          anchors.fill: parent
          fraction: root.summary.active.length > 0 ? root.summary.fraction : 0
          color: root.urgentState ? root.barUrgent : root.barColor
          mark: root.markFor(root.phase)
          broken: root.phase === "stalled"
          trackStrength: root.phase === "failed" ? 1 : 0.28
        }
        // A failure behind an active download: a small urgent dot on the ring.
        Rectangle {
          visible: root.hasFailed && root.phase !== "failed"
          width: 5
          height: 5
          radius: 2.5
          color: root.barUrgent
          anchors.top: parent.top
          anchors.right: parent.right
        }
      }
    }
    onPressed: function (b) {
      if (!root.service) return
      if (b === Qt.MiddleButton) root.service.openApp()
      else root.toggle()
    }
  }

  // --- the panel -----------------------------------------------------------------------

  property int cursorIndex: 0
  property bool cursorActive: false
  // The key of the row whose remove button is armed (a second press removes).
  property string armedKey: ""
  Timer {
    id: disarm
    interval: 3000
    onTriggered: root.armedKey = ""
  }
  function arm(key) {
    root.armedKey = key
    disarm.restart()
  }

  onOpenedChanged: if (opened) {
    if (root.service) root.service.refresh()
    root.cursorActive = false
    root.cursorIndex = 0
    root.armedKey = ""
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened && root.shown
    focusTarget: panel.loadedContent
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight, Style.space(640))
    onVisibleChanged: if (!visible) contentRequested = false

    content: Body {
      widget: root
      service: root.service
    }
  }
}
