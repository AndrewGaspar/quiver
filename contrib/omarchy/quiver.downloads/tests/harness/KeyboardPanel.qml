import QtQuick

// Harness stand-in for qs.Ui's KeyboardPanel, whose full-screen layer-shell
// window needs a Wayland compositor. Same API surface the widget uses; the
// harness renders the panel's content (Body.qml) on its own instead.
Item {
  property Item anchorItem: null
  property QtObject bar: null
  property var owner: null
  property bool open: false
  property Item focusTarget: null
  property int contentWidth: 0
  property int contentHeight: 0
  property Component content: null
  property bool contentRequested: false
  readonly property Item loadedContent: null
  readonly property real loadedContentHeight: 0

  function fittedContentWidth(width, cap) { return Math.round(width) }
  function fittedContentHeight(height, cap) { return Math.round(height) }

  visible: false
}
