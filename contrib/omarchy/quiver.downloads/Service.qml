// Quiver's download queue for the whole shell: one instance, however many
// bars there are. The bar widget (Widget.qml) only draws what this holds.
//
// ## Where the data comes from
//
// One long-running `quiver queue --watch`, which prints the queue as a JSON
// line whenever quiverd reports a change (at most one a second while a
// download runs, nothing while the queue sits still). It connects to a
// running quiverd only: while there is none it prints a "not connected" line
// and looks again every 5 s inside that same process, so this widget never
// starts quiverd and never spawns processes on a timer. If the watch process
// itself exits (an older CLI without --watch, quiver not installed), it is
// started again with a growing back-off, up to once a minute.
//
// Actions run `quiver pause|resume <game>` and `quiver queue remove <game>`
// one at a time, with each game's ref passed as its own argument (never
// through a shell), after Model.js has checked it against a strict pattern.

import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import "Model.js" as Model

Item {
  id: root

  property var shell: null
  property var manifest: null

  // Set by the bar widget from its settings.
  property bool notify: true
  property string quiverPath: ""

  readonly property string quiver: root.quiverPath !== "" ? root.quiverPath : "quiver"
  // How long an active download may sit without new bytes before it counts as stalled.
  readonly property int stallMs: 20000

  // --- state -----------------------------------------------------------------

  // A watch line arrived at least once since the process started.
  property bool watching: false
  // quiverd answered (the last line said connected).
  property bool connected: false
  // Why the queue can't be shown: quiverd's error, or the CLI's.
  property string downReason: ""
  // The watch process exits straight away: this CLI has no --watch, or no quiver.
  property bool unavailable: false
  property string cliError: ""

  property var items: []
  property var settings: null
  property bool truncated: false
  property var tracked: ({})
  property var stalled: ({})
  readonly property var summary: Model.summarize(root.items, root.stalled)
  readonly property string phase: !root.connected ? (root.unavailable ? "unavailable" : "down") : root.summary.phase

  // Keys this widget removed, so their disappearance isn't read as finished.
  property var removedKeys: ({})
  property bool haveSnapshot: false

  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

  // --- the watch ---------------------------------------------------------------

  function handleLine(text) {
    var parsed = Model.parseLine(String(text || ""))
    if (!parsed) return
    root.watching = true
    root.unavailable = false
    restart.backoff = 0
    if (!parsed.connected) {
      root.connected = false
      root.downReason = parsed.error
      root.items = []
      root.tracked = ({})
      root.stalled = ({})
      root.haveSnapshot = false
      return
    }
    var now = Date.now()
    var before = root.haveSnapshot ? root.items : null
    var changes = Model.transitions(before, parsed.items, root.removedKeys)
    root.connected = true
    root.downReason = ""
    root.items = parsed.items
    root.settings = parsed.settings
    root.truncated = parsed.truncated
    root.tracked = Model.track(root.tracked, parsed.items, now)
    root.stalled = Model.stalledKeys(root.tracked, parsed.items, now, root.stallMs)
    root.haveSnapshot = true
    root.forgetRemoved()
    if (root.notify) for (var i = 0; i < changes.length; i++) root.announce(changes[i])
  }

  // Keys stop being "ours" once they have left the queue.
  function forgetRemoved() {
    var keep = ({})
    var any = false
    for (var i = 0; i < root.items.length; i++) {
      var k = root.items[i].key
      if (root.removedKeys[k]) { keep[k] = true; any = true }
    }
    root.removedKeys = any ? keep : ({})
  }

  Process {
    id: watch
    command: [root.quiver, "queue", "--watch"]
    running: true
    stdout: SplitParser {
      onRead: function (data) { root.handleLine(data) }
    }
    // The first thing it says on stderr is the reason (clap's usage hint follows).
    stderr: SplitParser {
      onRead: function (data) {
        var line = Model.cleanText(data, Model.MAX_ERROR)
        if (line !== "" && root.cliError === "") root.cliError = line
      }
    }
    onRunningChanged: {
      if (running) {
        root.cliError = ""
        return
      }
      // It only ever ends on its own when something is wrong.
      root.connected = false
      root.items = []
      root.haveSnapshot = false
      if (!root.watching) root.unavailable = true
      root.watching = false
      restart.schedule()
    }
  }

  Timer {
    id: restart
    property int backoff: 0
    function schedule() {
      var steps = [2000, 5000, 15000, 60000]
      interval = steps[Math.min(backoff, steps.length - 1)]
      backoff += 1
      start()
    }
    onTriggered: watch.running = true
  }

  // A stalled download stops producing lines (identical lines aren't printed),
  // so look at the clock while anything is active.
  Timer {
    interval: 5000
    repeat: true
    running: root.summary.active.length > 0
    onTriggered: root.stalled = Model.stalledKeys(root.tracked, root.items, Date.now(), root.stallMs)
  }

  function refresh() {
    restart.stop()
    restart.backoff = 0
    if (watch.running) watch.running = false
    else watch.running = true
  }

  // --- actions -----------------------------------------------------------------

  // Commands waiting to run, each an argv tail after `quiver`.
  property var pending: []
  property string lastError: ""
  readonly property bool busy: action.running || root.pending.length > 0

  function enqueue(list) {
    var next = root.pending.slice()
    for (var i = 0; i < list.length; i++) next.push(list[i])
    root.pending = next
    root.lastError = ""
    if (!action.running) runNext()
  }

  function runNext() {
    if (root.pending.length === 0) return
    var args = root.pending[0]
    root.pending = root.pending.slice(1)
    action.verb = args.join(" ")
    action.command = [root.quiver].concat(args)
    action.running = true
  }

  function itemFor(ref) {
    for (var i = 0; i < root.items.length; i++) if (root.items[i].ref === ref) return root.items[i]
    return null
  }

  function pause(ref) { if (root.itemFor(ref)) enqueue([["pause", ref]]) }
  function resume(ref) { if (root.itemFor(ref)) enqueue([["resume", ref]]) }
  function remove(ref) {
    var it = root.itemFor(ref)
    if (!it) return
    var marks = Object.assign({}, root.removedKeys)
    marks[it.key] = true
    root.removedKeys = marks
    enqueue([["queue", "remove", ref]])
  }
  function pauseAll() {
    enqueue(Model.pauseTargets(root.items).map(function (r) { return ["pause", r] }))
  }
  function resumeAll() {
    enqueue(Model.resumeTargets(root.items).map(function (r) { return ["resume", r] }))
  }

  // Pause when anything runs or waits; otherwise resume what is paused.
  readonly property bool canPauseAll: Model.pauseTargets(root.items).length > 0
  readonly property bool canResumeAll: Model.resumeTargets(root.items).length > 0
  function toggleAll() {
    if (root.canPauseAll) pauseAll()
    else if (root.canResumeAll) resumeAll()
  }

  // Quiver's window if it is open, else a new one. The app has no page
  // argument yet, so it opens on whatever page it shows.
  function openApp() {
    Commons.Util.execArgv(["omarchy-launch-or-focus", "^quiver$", "uwsm-app -- quiver.desktop"])
  }

  Process {
    id: action
    property string verb: ""
    environment: ({ QUIVER_SOURCE: "omarchy-bar" })
    stderr: StdioCollector {
      id: actionErr
      waitForEnd: true
    }
    onExited: function (exitCode) {
      if (exitCode !== 0) {
        var why = Model.cleanText(actionErr.text, Model.MAX_ERROR).replace(/^Error:\s*/, "")
        root.lastError = strings.t("error.action").arg(action.verb).arg(why !== "" ? why : String(exitCode))
        clearError.restart()
        // Don't run the rest of a bulk action blind after a failure.
        root.pending = []
      }
      root.runNext()
    }
  }

  Timer {
    id: clearError
    interval: 20000
    onTriggered: root.lastError = ""
  }

  // --- notifications ------------------------------------------------------------

  // Quiver itself sends none, so the widget does: a download that finished,
  // and one that failed. Notification bodies are markup, hence the escaping.
  function announce(change) {
    var it = change.item
    var summary = change.type === "failed" ? strings.t("notify.failed") : strings.t("notify.finished")
    var body = Model.escapeMarkup(it.name)
    if (change.type === "failed" && it.error !== "") body += "\n" + Model.escapeMarkup(it.error)
    Quickshell.execDetached(["notify-send", "--app-name=Quiver", "--icon=quiver",
      "--urgency=" + (change.type === "failed" ? "critical" : "normal"), summary, body])
  }

  // --- IPC -------------------------------------------------------------------------

  ShellIpc {
    target: "quiver.downloads"

    function status(): string {
      var s = root.summary
      return JSON.stringify({
        connected: root.connected,
        phase: root.phase,
        percent: Model.percent(s.fraction),
        speed: s.speed,
        eta: s.eta,
        active: s.active.length,
        queued: s.queued,
        paused: s.paused,
        failed: s.failed.length,
        stalled: s.stalledCount
      })
    }
    function refresh(): void { root.refresh() }
    function pauseAll(): void { root.pauseAll() }
    function resumeAll(): void { root.resumeAll() }
    function open(): void { root.openApp() }
  }
}
