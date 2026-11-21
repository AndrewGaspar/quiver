// Quiver in the bar: its mark, always there (unless showWhenIdle is off), in the
// bar's one ink. While a game runs, its short title beside the mark; while
// something downloads, the mark's ring is the progress track (with the
// percentage or speed beside it when no game runs), and paused, failed and
// stalled downloads have their own marks. Click for the Quiver panel: now
// playing, recent games to launch, the download queue, and a way into Quiver.
//
// Everything lives in this plugin's service (Service.qml), which exists once
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
    root.service.panelOpen = root.opened
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
  // A vertical bar has room for the mark only (like the built-in widgets' labels).
  readonly property bool vertical: !!root.bar && root.bar.vertical === true

  readonly property var playing: root.service ? root.service.playing : []
  readonly property var recent: root.service ? root.service.recent : []
  readonly property var nowPlaying: root.playing.length > 0 ? root.playing[0] : null
  readonly property double now: root.service ? root.service.now : Date.now()
  // Something in the queue to draw on the ring.
  readonly property bool downloading: root.phase === "active" || root.phase === "stalled"
  readonly property bool queueBusy: root.phase !== "idle" && root.phase !== "down" && root.phase !== "unavailable"

  // "playing" | "percent" | "speed" | "both" | "none"
  readonly property string labelMode: {
    var m = String(root.setting("label", "playing"))
    return m === "percent" || m === "speed" || m === "both" || m === "none" ? m : "playing"
  }
  readonly property bool showWhenIdle: root.setting("showWhenIdle", true) !== false
  // The widest the playing title gets in the bar, in characters of the bar font.
  readonly property int titleChars: {
    var n = Math.floor(Number(root.setting("titleWidth", 18)))
    return isFinite(n) ? Math.max(6, Math.min(40, n)) : 18
  }
  readonly property bool shown: root.service !== null
    && (root.showWhenIdle || root.queueBusy || root.nowPlaying !== null)

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
  // How long a game has been running: "1 h 12 min", "8 min", "just started".
  function runningText(startedMs) {
    if (!(startedMs > 0)) return ""
    var p = Model.durationParts((root.now - startedMs) / 1000)
    if (p.kind === "hm") return strings.t("running.hm").arg(root.num(p.h, 0)).arg(root.num(p.m, 0))
    if (p.kind === "m") return strings.t("running.m").arg(root.num(p.m, 0))
    return strings.t("running.lt1m")
  }
  // "Streaming to roth (Moonlight)", "Kept for a resume on roth", "Starting a stream to roth".
  function streamText(stream) {
    if (!stream) return ""
    var via = stream.client === "moonlight" ? "Moonlight" : "Quiver"
    if (stream.state === "kept") return strings.t("stream.kept").arg(stream.peer).arg(via)
    if (stream.state === "starting") return strings.t("stream.starting").arg(stream.peer).arg(via)
    return strings.t("stream.streaming").arg(stream.peer).arg(via)
  }
  function agoText(ms) {
    var p = Model.agoParts(ms, root.now)
    if (p.kind === "m") return strings.t("ago.m").arg(root.num(p.n, 0))
    if (p.kind === "h") return strings.t("ago.h").arg(root.num(p.n, 0))
    if (p.kind === "d" && p.n === 1) return strings.t("ago.yesterday")
    if (p.kind === "d") return strings.tn("ago.d", p.n).arg(root.num(p.n, 0))
    return strings.t("ago.now")
  }

  // The mark inside the panel's download ring.
  function markFor(phase) {
    if (phase === "active" || phase === "stalled") return root.verifyingOnly ? "check" : "arrow"
    if (phase === "failed") return "bang"
    if (phase === "paused") return "pause"
    if (phase === "queued") return "dots"
    return "none"
  }

  // The bar's download label (percent, speed or both), while something downloads.
  readonly property string downloadLabel: {
    if (!root.downloading) return ""
    var pct = root.percentText(root.summary.fraction)
    // Near zero, the speed says "stalled" rather than a number.
    var speed = root.phase === "stalled" ? strings.t("bar.stalled") : root.rateText(root.summary.speed)
    if (root.labelMode === "speed") return speed
    if (root.labelMode === "both") return pct + " " + speed
    return pct
  }
  // "playing" shows the running game's title, and the percentage while only downloading.
  readonly property bool labelIsTitle: root.labelMode === "playing" && root.nowPlaying !== null
  readonly property string labelText: {
    if (root.vertical || root.labelMode === "none") return ""
    if (root.labelIsTitle) return root.nowPlaying.short
    return root.downloadLabel
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

  readonly property string downloadsTooltip: {
    var s = root.summary
    if (root.downloading) {
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
    if (root.phase === "idle") return root.nowPlaying ? "" : strings.t("tooltip.idle")
    return root.phaseText
  }

  readonly property string tooltip: {
    var lines = []
    for (var i = 0; i < root.playing.length; i++) {
      var g = root.playing[i]
      var bits = [strings.t("tooltip.playing").arg(g.name)]
      var run = root.runningText(g.started)
      if (run !== "") bits.push(run)
      lines.push(bits.join(" · "))
      if (g.stream) lines.push(root.streamText(g.stream))
    }
    if (root.downloadsTooltip !== "") lines.push(root.downloadsTooltip)
    return lines.join("\n")
  }

  // --- the bar ----------------------------------------------------------------------

  visible: root.shown
  implicitWidth: root.shown ? button.implicitWidth : 0
  implicitHeight: root.shown ? button.implicitHeight : 0

  onShownChanged: if (!root.shown && root.opened) root.close()

  // The render harness: scroll the title as if the pointer rested on it.
  property bool previewMarquee: false
  readonly property bool titleOverflows: button.overflows
  readonly property bool titleScrolling: button.marquee

  TextMetrics {
    id: titleCap
    font.family: root.bar ? root.bar.fontFamily : Style.font.family
    font.pixelSize: Style.bar.iconFont
    text: "0".repeat(root.titleChars)
  }

  BarSlot {
    id: button
    anchors.fill: parent
    bar: root.bar
    labelText: root.labelText
    // A title is capped (and scrolls on hover when longer); a download label isn't.
    labelCap: root.labelIsTitle ? Math.ceil(titleCap.advanceWidth) : 0
    scrolls: root.labelIsTitle
    forceMarquee: root.previewMarquee
    tooltipText: root.tooltip
    // Waiting, paused, or quiverd away: quieter, unless a game is running.
    dimmed: root.nowPlaying === null
      && (root.phase === "paused" || root.phase === "queued" || root.phase === "down" || root.phase === "unavailable")
    // The label takes the ring's colour: urgent while stalled.
    active: root.urgentState
    iconComponent: Component {
      Item {
        width: 16
        height: 16
        // Quiver's mark, its ring the progress track while there is a queue
        // (BrandRing.qml); otherwise the plain mark. The panel keeps Ring.
        BrandRing {
          anchors.fill: parent
          plain: !root.queueBusy
          fraction: root.summary.active.length > 0 ? root.summary.fraction : 0
          color: root.urgentState ? root.barUrgent : root.barColor
          broken: root.phase === "stalled"
          badge: root.phase === "paused" ? "pause"
            : root.phase === "failed" ? "bang"
            : root.downloading && root.verifyingOnly ? "check" : ""
          trackStrength: root.phase === "failed" ? 1 : 0.32
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
      if (b === Qt.MiddleButton) root.service.openApp(root.queueBusy ? "downloads" : "library")
      else root.toggle()
    }
  }

  // The bar slot: an optional label, then the mark, as qs.Ui's BarIconButton lays
  // them out, except that a label wider than `labelCap` is cut with an ellipsis
  // and, while the pointer rests on it, scrolls (a marquee) to show the rest.
  // Scrolling only on hover keeps the bar still the rest of the time, like
  // every other widget in it; the tooltip has the whole title too.
  component BarSlot: WidgetButton {
    id: slot

    property Component iconComponent: null
    property string labelText: ""
    // The widest the label gets, in px; 0: as wide as it is.
    property real labelCap: 0
    property bool scrolls: false

    labelVisible: false
    hasVisualContent: true
    fontSize: Style.bar.iconFont

    readonly property real gap: Style.space(4)
    readonly property real canvas: Math.max(Style.bar.iconSize, Style.bar.iconCanvas)
    readonly property bool hasLabel: slot.labelText !== "" && !slot.vertical
    readonly property real textWidth: Math.ceil(fullText.advanceWidth)
    readonly property bool overflows: slot.hasLabel && slot.labelCap > 0 && slot.textWidth > slot.labelCap
    readonly property real labelWidth: !slot.hasLabel ? 0 : (slot.overflows ? slot.labelCap : slot.textWidth)
    // Scrolls once the pointer has rested a moment, and never with animations off.
    property bool lingered: false
    // The render harness scrolls it without a pointer.
    property bool forceMarquee: false
    readonly property bool marquee: slot.overflows && slot.scrolls && Style.duration(1000) > 0
      && (slot.forceMarquee || (slot.lingered && slot.tooltipHovered))
    readonly property real marqueeGap: Style.space(24)
    // Constant motion, so a constant speed (px per second).
    readonly property real marqueeSpeed: 32

    // The label widens the slot by its own width, so the mark stays put as digits change
    // (the bar font is monospace).
    fixedWidth: slot.vertical ? -1 : Math.ceil(Style.bar.iconSlot + (slot.hasLabel ? slot.labelWidth + slot.gap : 0))
    fixedHeight: slot.vertical ? Style.bar.iconSlot : -1

    onTooltipHoveredChanged: {
      slot.lingered = false
      if (slot.tooltipHovered) linger.restart()
      else linger.stop()
    }
    Timer {
      id: linger
      interval: 450
      onTriggered: slot.lingered = true
    }

    TextMetrics {
      id: fullText
      font.family: slot.fontFamily
      font.pixelSize: slot.fontSize
      text: slot.labelText
    }
    TextMetrics {
      id: elidedText
      font: fullText.font
      text: slot.labelText
      elide: Text.ElideRight
      elideWidth: slot.labelCap
    }
    FontMetrics {
      id: figures
      font: fullText.font
    }

    Item {
      id: content
      // Exact coordinates preserve the center of odd-width slots.
      x: (slot.width - width) / 2
      y: (slot.height - height) / 2
      scale: slot.pressScale
      width: slot.canvas + (slot.hasLabel ? slot.labelWidth + slot.gap : 0)
      height: slot.canvas

      Item {
        id: labelBox
        visible: slot.hasLabel
        width: slot.labelWidth
        height: parent.height
        clip: slot.overflows

        Row {
          id: track
          spacing: slot.marqueeGap
          // Centre the figure height, as BarIconButton does, so the label lines up with the mark.
          y: (parent.height - height) / 2
            + (figures.descent - figures.ascent + figures.tightBoundingRect("0").height) / 2

          Text {
            textFormat: Text.PlainText
            text: slot.overflows && !slot.marquee ? elidedText.elidedText : slot.labelText
            font: fullText.font
            color: slot.active && slot.useActiveColor ? slot.activeColor : slot.foreground
            renderType: Text.NativeRendering
          }
          // The title again, so the loop runs on without a jump.
          Text {
            visible: slot.marquee
            textFormat: Text.PlainText
            text: slot.labelText
            font: fullText.font
            color: slot.active && slot.useActiveColor ? slot.activeColor : slot.foreground
            renderType: Text.NativeRendering
          }
        }

        SequentialAnimation {
          id: scroll
          running: slot.marquee
          loops: Animation.Infinite
          onRunningChanged: if (!running) track.x = 0
          PauseAnimation { duration: 600 }
          NumberAnimation {
            target: track
            property: "x"
            from: 0
            to: -(slot.textWidth + slot.marqueeGap)
            duration: Math.round((slot.textWidth + slot.marqueeGap) / slot.marqueeSpeed * 1000)
            easing.type: Easing.Linear
          }
        }
      }

      BarIconContent {
        x: slot.hasLabel ? slot.labelWidth + slot.gap : 0
        width: slot.canvas
        height: slot.canvas
        sourceComponent: slot.iconComponent
        sourceSize: Style.bar.iconCanvas
      }
    }
  }

  // --- the panel -----------------------------------------------------------------------

  property int cursorIndex: 0
  property bool cursorActive: false
  // The key of the row whose remove (or stop) button is armed (a second press does it).
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

  onOpenedChanged: {
    if (root.service) root.service.panelOpen = root.opened
    if (opened) {
      if (root.service) root.service.refresh()
      root.cursorActive = false
      root.cursorIndex = 0
      root.armedKey = ""
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened && root.shown
    focusTarget: panel.loadedContent
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight, Style.space(720))
    onVisibleChanged: if (!visible) contentRequested = false

    content: Body {
      widget: root
      service: root.service
    }
  }
}
