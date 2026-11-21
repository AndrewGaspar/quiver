// Quiver for the whole shell: the download queue, what is being played and the
// recently played games. One instance, however many bars there are; the bar
// widget (Widget.qml) only draws what this holds.
//
// ## Where the data comes from
//
// Playing and recent games: `quiver playing --follow --json --no-start`, one
// long-running process that prints a snapshot line whenever quiverd says
// something changed (a game started or stopped, a stream, the library), so
// nothing polls for it. It keeps running across quiverd restarts (saying
// "connected": false meanwhile); when it ends (quiverd wasn't running, an old
// quiver without `playing`) it is started again after 5 s, backing off to a
// minute. Running time is counted here from the start time.
//
// The queue:
// `quiver queue --json --no-start`, one at a time, never overlapping: every
// 2 s while something downloads or verifies, every 15 s while the queue sits
// still, and backing off from 5 s to once a minute while quiverd is down (or
// the CLI can't answer). --no-start means the widget never starts quiverd.
// Each run is a fresh process, so a new quiver build is picked up as soon as
// it is installed. Speed and time left come from the throughput samples each
// item carries (Model.js has the rules, the app's).
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
  // The app beside the CLI when the CLI's path is set, else on PATH.
  readonly property string quiverApp: root.quiverPath !== "" && root.quiverPath.indexOf("/") !== -1
    ? root.quiverPath.replace(/[^/]*$/, "quiver-app") : "quiver-app"
  // For a quiverd without throughput samples: how long an active download may
  // sit without new bytes before it counts as stalled.
  readonly property int stallMs: 20000

  // --- state -----------------------------------------------------------------

  // quiverd answered the last poll.
  property bool connected: false
  // Why the queue can't be shown: quiverd's error, or the CLI's.
  property string downReason: ""
  // This quiver can't answer (no --no-start, not installed).
  property bool unavailable: false
  // Polls in a row that failed.
  property int downs: 0

  property var items: []
  property bool truncated: false
  property var tracked: ({})
  property var stalled: ({})
  readonly property var summary: Model.summarize(root.items, root.stalled)
  readonly property string phase: !root.connected ? (root.unavailable ? "unavailable" : "down") : root.summary.phase

  // Keys this widget removed, so their disappearance isn't read as finished.
  property var removedKeys: ({})
  property bool haveSnapshot: false

  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

  // --- polling -----------------------------------------------------------------

  function handleQueue(text) {
    var parsed = Model.parseQueue(String(text || ""))
    if (!parsed) return root.handleFailure("error", "quiver queue --json: unexpected output")
    var now = Date.now()
    var changes = Model.transitions(root.haveSnapshot ? root.items : null, parsed.items, root.removedKeys)
    root.downs = 0
    root.unavailable = false
    root.connected = true
    root.downReason = ""
    root.items = parsed.items
    root.truncated = parsed.truncated
    root.tracked = Model.track(root.tracked, parsed.items, now)
    root.stalled = Model.stalledKeys(root.tracked, parsed.items, now, root.stallMs)
    root.haveSnapshot = true
    root.forgetRemoved()
    if (root.notify) for (var i = 0; i < changes.length; i++) root.announce(changes[i])
  }

  // kind: "down" | "unavailable" | "error" (Model.pollFailure).
  function handleFailure(kind, reason) {
    root.downs += 1
    root.connected = false
    root.unavailable = kind === "unavailable"
    root.downReason = Model.cleanText(reason, Model.MAX_ERROR).replace(/^Error:\s*/, "")
    root.items = []
    root.tracked = ({})
    root.stalled = ({})
    root.haveSnapshot = false
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

  property double lastPollAt: 0

  Process {
    id: poll
    command: [root.quiver, "queue", "--json", "--no-start"]
    environment: ({ QUIVER_SOURCE: "omarchy-bar" })
    stdout: StdioCollector { id: pollOut; waitForEnd: true }
    stderr: StdioCollector { id: pollErr; waitForEnd: true }
    property bool started: false
    onStarted: {
      started = true
      watchdog.restart()
    }
    // A quiver that can't be started reports no exit, only that it isn't running.
    onRunningChanged: if (!running && !started) {
      root.handleFailure("unavailable", "can't run " + root.quiver)
      root.scheduleNext()
    }
    onExited: function (exitCode) {
      watchdog.stop()
      if (exitCode === 0) root.handleQueue(pollOut.text)
      else root.handleFailure(Model.pollFailure(exitCode, pollErr.text), String(pollErr.text || "").split("\n")[0] || ("exit " + exitCode))
      root.scheduleNext()
    }
  }

  // A poll that hangs (quiverd wedged) is given up on.
  Timer {
    id: watchdog
    interval: 20000
    onTriggered: {
      if (!poll.running) return
      poll.running = false
      root.handleFailure("error", "quiver queue didn't answer in 20 s")
      root.scheduleNext()
    }
  }

  // Off in the render harness, which feeds the queue (and what is playing) in itself.
  property bool polling: true
  property bool following: true

  // --- now playing, recent games ---------------------------------------------------

  // The follow's last snapshot (Model.parsePlaying).
  property var playing: []
  property var recent: []
  // The follow is running and quiverd is there.
  property bool followConnected: false
  property int followDowns: 0
  readonly property bool isPlaying: root.playing.length > 0
  // Now, a minute at a time while a game runs (running times are in minutes).
  property double now: Date.now()
  Timer {
    interval: 15000
    repeat: true
    running: root.isPlaying
    onTriggered: root.now = Date.now()
  }
  onIsPlayingChanged: root.now = Date.now()

  function handlePlaying(line) {
    var parsed = Model.parsePlaying(String(line || ""))
    if (!parsed) return
    root.followDowns = 0
    root.followConnected = parsed.connected
    root.playing = parsed.connected ? parsed.playing : []
    // While quiverd is away the follow keeps the last recent games; so does this.
    if (parsed.connected || parsed.recent.length > 0) root.recent = parsed.recent
    root.now = Date.now()
  }

  Process {
    id: follow
    command: [root.quiver, "playing", "--follow", "--json", "--no-start", "--recent", "6"]
    environment: ({ QUIVER_SOURCE: "omarchy-bar" })
    stdout: SplitParser { onRead: function (line) { root.handlePlaying(line) } }
    stderr: StdioCollector { id: followErr; waitForEnd: true }
    property bool started: false
    onStarted: started = true
    onRunningChanged: if (!running && !started) root.followEnded("unavailable")
    onExited: function (exitCode) {
      root.followEnded(Model.pollFailure(exitCode, followErr.text))
    }
  }

  function followEnded(kind) {
    root.followConnected = false
    root.playing = []
    root.followDowns += 1
    if (!root.following) return
    followRetry.interval = Model.followRetry(kind, root.followDowns)
    followRetry.restart()
  }

  Timer {
    id: followRetry
    onTriggered: root.startFollow()
  }

  function startFollow() {
    if (follow.running || !root.following) return
    follow.started = false
    follow.running = true
  }

  // A new CLI path: follow with that one.
  onQuiverChanged: if (follow.running) {
    follow.running = false
  } else if (root.following && root.followDowns > 0) {
    followRetry.interval = 0
    followRetry.restart()
  }

  // Belt and braces: whatever happened, polling never stops for good.
  Timer {
    interval: 60000
    repeat: true
    running: root.polling
    onTriggered: if (!poll.running && !nextPoll.running) root.pollNow()
  }

  Timer {
    id: nextPoll
    onTriggered: root.pollNow()
  }

  function pollNow() {
    if (poll.running) return
    root.lastPollAt = Date.now()
    poll.started = false
    poll.running = true
  }

  function scheduleNext() {
    if (!root.polling) return
    nextPoll.interval = Model.pollInterval(root.phase, root.downs)
    nextPoll.restart()
  }

  // Look again soon (the panel opened, an action ran), but never sooner than
  // 2 s after the last poll started.
  function refresh() {
    if (poll.running || !root.polling) return
    nextPoll.interval = Math.max(0, 2000 - (Date.now() - root.lastPollAt))
    nextPoll.restart()
  }

  Component.onCompleted: {
    if (root.polling) root.pollNow()
    root.startFollow()
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
    if (root.pending.length === 0) return root.refresh()
    var args = root.pending[0]
    root.pending = root.pending.slice(1)
    action.verb = args.join(" ")
    action.command = [root.quiver].concat(args)
    action.started = false
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

  // Quiver on a page (downloads, library): Hyprland focuses an open Quiver
  // window (and runs nothing when there is none), and `quiver-app --page P`
  // turns the open app to it, or starts one there.
  function openApp(page) {
    var p = page === "library" || page === "activity" ? page : "downloads"
    Quickshell.execDetached(["omarchy-launch-or-focus", "^quiver$", "true"])
    Quickshell.execDetached(["uwsm-app", "--", root.quiverApp, "--page", p])
  }

  function playingFor(ref) {
    for (var i = 0; i < root.playing.length; i++) if (root.playing[i].ref === ref) return root.playing[i]
    return null
  }
  function recentFor(ref) {
    for (var i = 0; i < root.recent.length; i++) if (root.recent[i].ref === ref) return root.recent[i]
    return null
  }

  // Play a recent game as the app's Play does (its default profile), without
  // following it: quiverd runs it.
  function play(ref) {
    var g = root.recentFor(ref)
    if (g && Model.playRef(g.ref) !== "") enqueue([["play", g.ref, "--detach"]])
  }
  function stop(ref) {
    var g = root.playingFor(ref)
    if (g && Model.playRef(g.ref) !== "") enqueue([["stop", g.ref]])
  }
  // The in-game Sight panel over that game (or the only one).
  function sight(ref) {
    var g = root.playingFor(ref)
    if (!g) return
    enqueue([g.session !== "" ? ["sight", "toggle", "--session", g.session] : ["sight", "toggle"]])
  }

  // The panel is open (set by the bar widget): action errors show there, and
  // as a notification otherwise.
  property bool panelOpen: false

  Process {
    id: action
    property string verb: ""
    property bool started: false
    environment: ({ QUIVER_SOURCE: "omarchy-bar" })
    onStarted: started = true
    onRunningChanged: if (!running && !started) {
      root.lastError = strings.t("error.action").arg(action.verb).arg("can't run " + root.quiver)
      clearError.restart()
      root.pending = []
      root.runNext()
    }
    stderr: StdioCollector {
      id: actionErr
      waitForEnd: true
    }
    onExited: function (exitCode) {
      if (exitCode !== 0) {
        var why = Model.cleanText(actionErr.text, Model.MAX_ERROR).replace(/^Error:\s*/, "")
        root.lastError = strings.t("error.action").arg(action.verb).arg(why !== "" ? why : String(exitCode))
        clearError.restart()
        if (!root.panelOpen) Quickshell.execDetached(["notify-send", "--app-name=Quiver", "--icon=quiver",
          "--urgency=normal", strings.t("notify.actionFailed"), Model.escapeMarkup(root.lastError)])
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
    function playing(): string {
      return JSON.stringify({
        connected: root.followConnected,
        playing: root.playing.map(function (g) {
          return { ref: g.ref, name: g.name, started: g.started, profile: g.profile, stream: g.stream }
        }),
        recent: root.recent.map(function (g) { return { ref: g.ref, name: g.name } })
      })
    }
    function refresh(): void { root.refresh() }
    function pauseAll(): void { root.pauseAll() }
    function resumeAll(): void { root.resumeAll() }
    function open(): void { root.openApp("downloads") }
  }
}
