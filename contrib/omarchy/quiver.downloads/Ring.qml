import QtQuick
import QtQuick.Shapes

// Download progress as a ring with a mark inside, drawn on a 16-unit box and
// scaled by whoever shows it (the bar fits it to its icon size).
//
// Each state has its own shape, so it reads without colour too:
//   arrow   downloading          check   verifying
//   dots    waiting to start     pause   paused
//   bang    failed               (stalled: the arrow on a broken, dashed ring)
Item {
  id: root

  property real fraction: 0
  property color color: "white"
  property string mark: "arrow"
  property bool broken: false
  // 0..1: how strongly the unfilled part of the ring shows.
  property real trackStrength: 0.28

  readonly property real unit: width / 16
  readonly property real stroke: 1.7 * unit
  readonly property real radius: (width - stroke) / 2
  readonly property real sweep: Math.max(0, Math.min(1, fraction)) * 360

  width: 16
  height: 16
  implicitWidth: 16
  implicitHeight: 16

  // Fixed bounds for the bar's icon fitting: the ring's box, whatever is drawn.
  Rectangle {
    anchors.fill: parent
    color: "transparent"
  }

  Shape {
    anchors.fill: parent
    preferredRendererType: Shape.CurveRenderer
    antialiasing: true

    // Track.
    ShapePath {
      fillColor: "transparent"
      strokeColor: Qt.rgba(root.color.r, root.color.g, root.color.b, root.broken ? 0.9 : root.trackStrength)
      strokeWidth: root.stroke
      strokeStyle: root.broken ? ShapePath.DashLine : ShapePath.SolidLine
      dashPattern: [1.2, 1.4]
      capStyle: ShapePath.FlatCap
      PathAngleArc {
        centerX: root.width / 2
        centerY: root.height / 2
        radiusX: root.radius
        radiusY: root.radius
        startAngle: 0
        sweepAngle: 360
      }
    }

    // Progress, clockwise from the top (mirrored layouts keep a clock's sense).
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.sweep > 0 && !root.broken ? root.color : "transparent"
      strokeWidth: root.stroke
      capStyle: ShapePath.RoundCap
      PathAngleArc {
        centerX: root.width / 2
        centerY: root.height / 2
        radiusX: root.radius
        radiusY: root.radius
        startAngle: -90
        sweepAngle: root.sweep
      }
    }

    // Arrow: a stem and a chevron.
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.mark === "arrow" ? root.color : "transparent"
      strokeWidth: 1.5 * root.unit
      capStyle: ShapePath.RoundCap
      joinStyle: ShapePath.RoundJoin
      startX: 8 * root.unit; startY: 4.6 * root.unit
      PathLine { x: 8 * root.unit; y: 11 * root.unit }
      PathMove { x: 5.3 * root.unit; y: 8.4 * root.unit }
      PathLine { x: 8 * root.unit; y: 11.1 * root.unit }
      PathLine { x: 10.7 * root.unit; y: 8.4 * root.unit }
    }

    // Check.
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.mark === "check" ? root.color : "transparent"
      strokeWidth: 1.5 * root.unit
      capStyle: ShapePath.RoundCap
      joinStyle: ShapePath.RoundJoin
      startX: 5.2 * root.unit; startY: 8.3 * root.unit
      PathLine { x: 7.2 * root.unit; y: 10.3 * root.unit }
      PathLine { x: 10.8 * root.unit; y: 6.2 * root.unit }
    }

    // Pause bars.
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.mark === "pause" ? root.color : "transparent"
      strokeWidth: 1.6 * root.unit
      capStyle: ShapePath.RoundCap
      startX: 6.5 * root.unit; startY: 5.6 * root.unit
      PathLine { x: 6.5 * root.unit; y: 10.4 * root.unit }
      PathMove { x: 9.5 * root.unit; y: 5.6 * root.unit }
      PathLine { x: 9.5 * root.unit; y: 10.4 * root.unit }
    }

    // Exclamation: a bar and a dot.
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.mark === "bang" ? root.color : "transparent"
      strokeWidth: 1.7 * root.unit
      capStyle: ShapePath.RoundCap
      startX: 8 * root.unit; startY: 4.7 * root.unit
      PathLine { x: 8 * root.unit; y: 8.6 * root.unit }
      PathMove { x: 8 * root.unit; y: 11.2 * root.unit }
      PathLine { x: 8 * root.unit; y: 11.25 * root.unit }
    }

    // Three dots: waiting.
    ShapePath {
      fillColor: "transparent"
      strokeColor: root.mark === "dots" ? root.color : "transparent"
      strokeWidth: 1.7 * root.unit
      capStyle: ShapePath.RoundCap
      startX: 5 * root.unit; startY: 8 * root.unit
      PathLine { x: 5.05 * root.unit; y: 8 * root.unit }
      PathMove { x: 8 * root.unit; y: 8 * root.unit }
      PathLine { x: 8.05 * root.unit; y: 8 * root.unit }
      PathMove { x: 11 * root.unit; y: 8 * root.unit }
      PathLine { x: 11.05 * root.unit; y: 8 * root.unit }
    }
  }
}
