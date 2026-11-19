// The downloads panel: what is downloading now, what is next, and the actions.
// Built when the panel first opens (KeyboardPanel's lazy content); `widget` is
// the bar widget (strings, formatting, cursor state) and `service` the queue.
//
// Keys: j/k or arrows move, Enter pauses/resumes/retries the row (or presses
// the button under the cursor), x removes the row (press twice), p pauses or
// resumes everything, o opens Quiver, Esc closes.

import QtQuick
import QtQuick.Layouts
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

PanelKeyCatcher {
  id: body

  property var widget: null
  property var service: null

  // How many waiting items are listed before "and N more".
  readonly property int maxWaiting: 6

  readonly property var strings: widget ? widget.strings : null
  function t(key) { return body.strings ? body.strings.t(key) : key }
  function tn(key, n) { return body.strings ? body.strings.tn(key, n) : key }

  readonly property var summary: widget ? widget.summary : Model.summarize([], {})
  readonly property string phase: widget ? widget.phase : "down"
  readonly property bool connected: !!service && service.connected
  readonly property var activeRows: summary.active
  readonly property var waitingRows: summary.waiting.slice(0, maxWaiting)
  readonly property int hiddenWaiting: Math.max(0, summary.waiting.length - maxWaiting)
    + (service && service.truncated ? 1 : 0)
  readonly property var rows: activeRows.concat(waitingRows)
  // Cursor targets: every row, then the two buttons.
  readonly property int targetCount: rows.length + 2
  readonly property int toggleIndex: rows.length
  readonly property int openIndex: rows.length + 1

  readonly property color fg: widget && widget.bar ? widget.bar.foreground : Commons.Color.foreground
  readonly property color dim: Qt.darker(fg, 1.4)
  readonly property color urgent: Commons.Color.urgent
  readonly property string fontFamily: widget && widget.bar ? widget.bar.fontFamily : Style.font.family

  implicitHeight: column.implicitHeight
  anchors.fill: parent

  function cursorAt(i) { return !!widget && widget.cursorActive && widget.cursorIndex === i }
  function point(i) {
    if (!widget) return
    widget.cursorActive = true
    widget.cursorIndex = i
  }
  function move(dy) {
    if (!widget) return
    if (!widget.cursorActive) { widget.cursorActive = true; return }
    widget.cursorIndex = Math.max(0, Math.min(targetCount - 1, widget.cursorIndex + dy))
  }

  // The row's main action: pause what runs or waits, resume what is paused, retry what failed.
  function primaryVerb(item) {
    if (!item || item.ref === "") return ""
    if (item.state === "paused") return "resume"
    if (item.state === "failed") return "retry"
    return "pause"
  }
  function primary(item) {
    if (!service || !item) return
    var v = primaryVerb(item)
    if (v === "pause") service.pause(item.ref)
    else if (v !== "") service.resume(item.ref)
  }
  function removeRow(item) {
    if (!service || !widget || !item || item.ref === "") return
    if (widget.armedKey === item.key) {
      widget.armedKey = ""
      service.remove(item.ref)
    } else {
      widget.arm(item.key)
    }
  }
  function activate() {
    if (!widget) return
    var i = widget.cursorIndex
    if (i < rows.length) primary(rows[i])
    else if (i === toggleIndex) service && service.toggleAll()
    else if (i === openIndex) openApp()
  }
  function openApp() {
    if (service) service.openApp()
    if (widget) widget.close()
  }

  onMoveRequested: function (dx, dy) { body.move(dy !== 0 ? dy : dx) }
  onActivateRequested: activate()
  onCloseRequested: if (widget) widget.close()
  onTabRequested: function (direction) { if (widget) widget.switchPanel(direction) }
  onDeleteRequested: {
    if (widget && widget.cursorActive && widget.cursorIndex < rows.length) removeRow(rows[widget.cursorIndex])
  }
  onTextKey: function (t) {
    var k = String(t).toLowerCase()
    if (k === "p") service && service.toggleAll()
    else if (k === "o") openApp()
    else if (k === "r") service && service.refresh()
  }

  // A line of detail for a row: sizes, speed, time left, retries.
  function detailFor(item) {
    var parts = []
    var stalled = !!service && !!service.stalled[item.key]
    var state = stalled ? t("state.stalled") : t("state." + item.state)
    if (item.kind !== "install") parts.push(t("kind." + item.kind))
    // The section says it is downloading; other states (verifying, stalled, queued…) are named.
    if (!(item.state === "downloading" && !stalled)) parts.push(state)
    if (item.total > 0) {
      parts.push(t("progress.of").arg(widget.bytesText(item.done)).arg(widget.bytesText(item.total)))
    }
    if (Model.isActive(item) && !stalled && item.speed > 0) {
      parts.push(widget.rateText(item.speed))
      var eta = widget.etaText(Model.eta(item.total - item.done, item.speed))
      if (eta !== "") parts.push(eta)
    }
    if (item.retries > 0) parts.push(tn("retries", item.retries).arg(widget.num(item.retries, 0)))
    return parts.join(" · ")
  }

  readonly property string heroMeta: {
    var s = summary
    if (phase === "active") {
      var bits = [widget.phaseText, widget.rateText(s.speed)]
      var eta = widget.etaText(s.eta)
      if (eta !== "") bits.push(eta)
      return bits.join(" · ")
    }
    if (phase === "idle") return t("phase.idle")
    return widget ? widget.phaseText : ""
  }

  readonly property string settingsLine: {
    var st = service ? service.settings : null
    if (!st || !connected) return ""
    var bits = [tn("settings.parallel", st.maxActive).arg(widget.num(st.maxActive, 0))]
    bits.push(st.limit > 0 ? t("settings.limit").arg(widget.rateText(st.limit)) : t("settings.noLimit"))
    return bits.join(" · ")
  }

  Column {
    id: column
    width: parent.width
    spacing: Style.space(12)

    PanelHero {
      id: hero
      width: parent.width
      title: body.t("title")
      meta: body.heroMeta
      detail: body.summary.active.length > 0 ? body.widget.percentText(body.summary.fraction)
        : (body.summary.count > 0 ? body.widget.num(body.summary.count, 0) : "")
      foreground: body.fg
      fontFamily: body.fontFamily

      // PanelHero has its own `root`; the icon reads what it needs off the hero.
      readonly property real ringFraction: body.summary.active.length > 0 ? body.summary.fraction : 0
      readonly property color ringColor: body.phase === "stalled" || body.phase === "failed" ? body.urgent : body.fg
      readonly property string ringMark: body.widget ? body.widget.markFor(body.phase) : "none"
      readonly property bool ringBroken: body.phase === "stalled"
      iconOpacity: body.phase === "paused" || body.phase === "queued" || body.phase === "idle"
        || body.phase === "down" || body.phase === "unavailable" ? 0.5 : 1

      iconComponent: Component {
        Ring {
          width: Style.font.display * 1.4
          height: width
          fraction: hero.ringFraction
          color: hero.ringColor
          mark: hero.ringMark
          broken: hero.ringBroken
          trackStrength: body.phase === "failed" ? 1 : 0.28
        }
      }
    }

    // Why there's nothing to show, or what just went wrong.
    Text {
      textFormat: Text.PlainText
      visible: text !== ""
      width: parent.width
      text: {
        if (!body.service) return ""
        if (body.service.lastError !== "") return body.service.lastError
        if (body.phase === "unavailable")
          return body.t("unavailable.detail") + (body.service.cliError !== "" ? "\n" + body.service.cliError : "")
        if (body.phase === "down")
          return body.t("down.detail") + (body.service.downReason !== "" ? "\n" + body.service.downReason : "")
        if (body.phase === "stalled") return body.t("stalled.detail")
        return ""
      }
      color: body.service && (body.service.lastError !== "" || body.phase === "stalled") ? body.urgent : body.dim
      font.family: body.fontFamily
      font.pixelSize: Style.font.caption
      wrapMode: Text.Wrap
      maximumLineCount: 5
      elide: Text.ElideRight
    }

    PanelSeparator {
      visible: body.rows.length > 0
      foreground: body.fg
    }

    Column {
      id: activeList
      visible: body.activeRows.length > 0
      width: parent.width
      spacing: Style.space(6)

      PanelSectionHeader {
        text: body.t("section.active")
        foreground: body.fg
        fontFamily: body.fontFamily
      }

      Repeater {
        model: body.activeRows
        ItemRow {
          required property var modelData
          required property int index
          width: activeList.width
          item: modelData
          rowIndex: index
        }
      }
    }

    Column {
      id: waitingList
      visible: body.waitingRows.length > 0
      width: parent.width
      spacing: Style.space(6)

      PanelSectionHeader {
        text: body.t("section.next")
        foreground: body.fg
        fontFamily: body.fontFamily
      }

      Repeater {
        model: body.waitingRows
        ItemRow {
          required property var modelData
          required property int index
          width: waitingList.width
          item: modelData
          rowIndex: body.activeRows.length + index
        }
      }

      Text {
        textFormat: Text.PlainText
        visible: body.hiddenWaiting > 0
        text: body.tn("section.more", body.hiddenWaiting).arg(body.widget ? body.widget.num(body.hiddenWaiting, 0) : body.hiddenWaiting)
        color: body.dim
        font.family: body.fontFamily
        font.pixelSize: Style.font.caption
        leftPadding: Style.space(10)
      }
    }

    Text {
      textFormat: Text.PlainText
      visible: body.connected && body.rows.length === 0
      text: body.t("queue.empty")
      color: body.dim
      font.family: body.fontFamily
      font.pixelSize: Style.font.bodySmall
    }

    PanelSeparator {
      foreground: body.fg
    }

    RowLayout {
      width: parent.width
      spacing: Style.space(8)

      Button {
        id: toggleButton
        Layout.fillWidth: true
        readonly property bool resuming: !!body.service && !body.service.canPauseAll && body.service.canResumeAll
        visible: !!body.service && body.connected && (body.service.canPauseAll || body.service.canResumeAll)
        text: body.service && body.service.busy ? body.t("action.working")
          : (resuming ? body.t("action.resumeAll") : body.t("action.pauseAll"))
        iconText: resuming ? "󰐊" : "󰏤"
        hasCursor: body.cursorAt(body.toggleIndex)
        foreground: body.fg
        fontFamily: body.fontFamily
        onHovered: function (on) { if (on) body.point(body.toggleIndex) }
        onClicked: if (body.service) body.service.toggleAll()
      }

      Button {
        Layout.fillWidth: true
        text: body.t("action.open")
        iconText: "󰏌"
        hasCursor: body.cursorAt(body.openIndex)
        foreground: body.fg
        fontFamily: body.fontFamily
        onHovered: function (on) { if (on) body.point(body.openIndex) }
        onClicked: body.openApp()
      }
    }

    Text {
      textFormat: Text.PlainText
      visible: text !== ""
      width: parent.width
      text: body.settingsLine
      color: body.dim
      font.family: body.fontFamily
      font.pixelSize: Style.font.caption
      elide: Text.ElideRight
    }
  }

  // One queue item: name, a progress bar, a detail line, and its actions.
  component ItemRow: CursorSurface {
    id: row
    property var item: null
    property int rowIndex: 0

    readonly property bool active: Model.isActive(item)
    readonly property bool failed: !!item && item.state === "failed"
    readonly property bool stalled: !!item && !!body.service && !!body.service.stalled[item.key]
    readonly property bool armed: !!item && !!body.widget && body.widget.armedKey === item.key
    readonly property string verb: body.primaryVerb(item)

    hasCursor: body.cursorAt(rowIndex)
    foreground: body.fg
    implicitHeight: rowColumn.implicitHeight + Style.space(12)
    opacity: !!item && (item.state === "paused" || item.state === "queued") && !hasCursor ? 0.75 : 1

    MouseArea {
      anchors.fill: parent
      hoverEnabled: true
      acceptedButtons: Qt.NoButton
      onEntered: body.point(row.rowIndex)
    }

    RowLayout {
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      anchors.leftMargin: Style.space(10)
      anchors.rightMargin: Style.space(6)
      spacing: Style.space(8)

      ColumnLayout {
        id: rowColumn
        Layout.fillWidth: true
        spacing: Style.space(4)

        Text {
          textFormat: Text.PlainText
          Layout.fillWidth: true
          text: row.item ? row.item.name : ""
          color: body.fg
          font.family: body.fontFamily
          font.pixelSize: Style.font.body
          elide: Text.ElideRight
          maximumLineCount: 1
        }

        // Progress: for active items, and for waiting ones that have a start.
        Rectangle {
          visible: !!row.item && row.item.total > 0 && (row.active || row.item.done > 0)
          Layout.fillWidth: true
          implicitHeight: Style.space(3)
          radius: height / 2
          color: Qt.rgba(body.fg.r, body.fg.g, body.fg.b, 0.15)

          Rectangle {
            anchors.left: parent.left
            anchors.top: parent.top
            anchors.bottom: parent.bottom
            width: parent.width * (row.item ? row.item.fraction : 0)
            radius: parent.radius
            color: row.stalled || row.failed ? body.urgent : body.fg
            opacity: row.active ? 1 : 0.5
            Behavior on width {
              NumberAnimation { duration: Style.duration(200); easing.type: Easing.OutCubic }
            }
          }
        }

        Text {
          textFormat: Text.PlainText
          Layout.fillWidth: true
          text: row.item && body.widget ? body.detailFor(row.item) : ""
          color: row.stalled ? body.urgent : body.dim
          font.family: body.fontFamily
          font.pixelSize: Style.font.caption
          elide: Text.ElideRight
          wrapMode: Text.Wrap
          maximumLineCount: 2
        }

        Text {
          textFormat: Text.PlainText
          visible: row.failed && !!row.item && row.item.error !== ""
          Layout.fillWidth: true
          text: row.item ? row.item.error : ""
          color: body.urgent
          font.family: body.fontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.Wrap
          maximumLineCount: 3
          elide: Text.ElideRight
        }
      }

      PanelActionButton {
        visible: row.verb !== ""
        Layout.alignment: Qt.AlignVCenter
        iconText: row.verb === "pause" ? "󰏤" : (row.verb === "retry" ? "󰑐" : "󰐊")
        tooltipText: row.verb === "pause" ? body.t("action.pause")
          : (row.verb === "retry" ? body.t("action.retry") : body.t("action.resume"))
        foreground: body.fg
        fontFamily: body.fontFamily
        enabled: !!body.service && !body.service.busy
        onHovered: function (on) { if (on) body.point(row.rowIndex) }
        onClicked: body.primary(row.item)
      }

      PanelActionButton {
        visible: !!row.item && row.item.ref !== ""
        Layout.alignment: Qt.AlignVCenter
        iconText: row.armed ? "󰄬" : "󰅖"
        tooltipText: row.armed ? body.t("action.confirmRemove") : body.t("action.remove")
        foreground: row.armed ? body.urgent : body.fg
        hoverColor: body.urgent
        fontFamily: body.fontFamily
        enabled: !!body.service && !body.service.busy
        onHovered: function (on) { if (on) body.point(row.rowIndex) }
        onClicked: body.removeRow(row.item)
      }
    }
  }
}
