import QtQuick
import QtQuick.Shapes

// The bar icon: Quiver's mark (a Q whose tail is an arrow leaving the ring), in one ink, with
// the ring as the progress track. Drawn on a 16-unit box from the mark's 100-unit artwork
// (ring centre (50,52), radius 30, its gap at the lower right where the arrow leaves).
//
// The arrow is the download arrow, so downloading needs no inner glyph. Other states add a
// small corner badge, so they read without colour too:
//   pause  paused          bang   failed          check  verifying
//   (stalled: the ring is dashed; waiting: the whole mark dims via the bar button)
Item {
  id: root

  property real fraction: 0
  property color color: "white"
  property bool broken: false
  // "", "pause", "bang" or "check".
  property string badge: ""
  // 0..1: how strongly the unfilled part of the ring shows.
  property real trackStrength: 0.32

  // The artwork's own bounds (about 12..98 of its 100 units) fill the box.
  readonly property real u: width / 86
  // The ring runs clockwise from straight down (90°) round to the gap's edge (16°): 286°.
  readonly property real arcStart: 90
  readonly property real arcSweep: 286
  readonly property real done: Math.max(0, Math.min(1, fraction))

  width: 16
  height: 16
  implicitWidth: 16
  implicitHeight: 16

  // Fixed bounds for the bar's icon fitting.
  Rectangle {
    anchors.fill: parent
    color: "transparent"
  }

  Shape {
    anchors.fill: parent
    preferredRendererType: Shape.CurveRenderer
    antialiasing: true
    transform: Translate { x: -12 * root.u; y: -12 * root.u }

    // The unfilled ring.
    ShapePath {
      strokeColor: Qt.rgba(root.color.r, root.color.g, root.color.b, root.trackStrength)
      strokeWidth: 14 * root.u
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      strokeStyle: root.broken ? ShapePath.DashLine : ShapePath.SolidLine
      dashPattern: [0.9, 0.9]
      PathAngleArc {
        centerX: 50 * root.u; centerY: 52 * root.u
        radiusX: 30 * root.u; radiusY: 30 * root.u
        startAngle: root.arcStart; sweepAngle: root.arcSweep
      }
    }
    // Progress along it.
    ShapePath {
      strokeColor: root.broken ? "transparent" : root.color
      strokeWidth: 14 * root.u
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      PathAngleArc {
        centerX: 50 * root.u; centerY: 52 * root.u
        radiusX: 30 * root.u; radiusY: 30 * root.u
        startAngle: root.arcStart; sweepAngle: Math.max(0.01, root.arcSweep * root.done)
      }
    }
    // The tail: an arrow leaving through the gap.
    ShapePath {
      strokeColor: root.color
      strokeWidth: 14 * root.u
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      startX: 44 * root.u; startY: 46 * root.u
      PathLine { x: 70 * root.u; y: 72 * root.u }
    }
    ShapePath {
      strokeColor: "transparent"
      fillColor: root.color
      startX: 92 * root.u; startY: 94 * root.u
      PathLine { x: 85 * root.u; y: 61 * root.u }
      PathLine { x: 59 * root.u; y: 87 * root.u }
      PathLine { x: 92 * root.u; y: 94 * root.u }
    }
  }

  // The state badge, top right, over a small knock-out so it reads on the ring.
  Item {
    visible: root.badge !== ""
    width: 7 * root.width / 16
    height: width
    anchors.top: parent.top
    anchors.right: parent.right
    anchors.topMargin: -root.width * 0.06
    anchors.rightMargin: -root.width * 0.06

    Row {
      visible: root.badge === "pause"
      anchors.centerIn: parent
      spacing: parent.width * 0.18
      Rectangle { width: parent.parent.width * 0.22; height: parent.parent.height * 0.8; radius: width / 3; color: root.color }
      Rectangle { width: parent.parent.width * 0.22; height: parent.parent.height * 0.8; radius: width / 3; color: root.color }
    }
    Column {
      visible: root.badge === "bang"
      anchors.centerIn: parent
      spacing: parent.height * 0.1
      Rectangle { anchors.horizontalCenter: parent.horizontalCenter; width: parent.parent.width * 0.26; height: parent.parent.height * 0.55; radius: width / 2; color: root.color }
      Rectangle { anchors.horizontalCenter: parent.horizontalCenter; width: parent.parent.width * 0.26; height: width; radius: width / 2; color: root.color }
    }
    Shape {
      visible: root.badge === "check"
      anchors.fill: parent
      preferredRendererType: Shape.CurveRenderer
      ShapePath {
        strokeColor: root.color
        strokeWidth: parent.width * 0.2
        fillColor: "transparent"
        capStyle: ShapePath.RoundCap
        joinStyle: ShapePath.RoundJoin
        startX: parent.width * 0.12; startY: parent.height * 0.55
        PathLine { x: parent.width * 0.4; y: parent.height * 0.82 }
        PathLine { x: parent.width * 0.9; y: parent.height * 0.18 }
      }
    }
  }
}
