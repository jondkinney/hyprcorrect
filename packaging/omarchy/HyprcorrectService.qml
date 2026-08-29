import QtQuick
import Quickshell
import Quickshell.Io
import "CompanionSanitizer.js" as Sanitizer

Item {
  id: root

  property bool enabled: true
  property bool connected: false
  property bool paused: false
  property bool reviewStartsInVim: false
  property bool languagetoolEnabled: false
  property bool llmConfigured: false
  property string defaultProvider: "spellbook"
  property string smartProvider: "spellbook"
  property string fixWord: ""
  property string fixSentence: ""
  property string review: ""
  property string reviewLlm: ""
  property string lastError: ""
  property string actionError: ""
  readonly property bool busy: actionProcess.running
  readonly property string bridgePath: localPath(Qt.resolvedUrl("bin/hyprcorrect-companion"))

  function localPath(url) {
    var value = String(url || "")
    return value.indexOf("file://") === 0
      ? decodeURIComponent(value.substring(7))
      : value
  }

  function cleanText(value, limit) {
    return Sanitizer.cleanText(value, limit)
  }

  function cleanProvider(value) {
    return Sanitizer.cleanProvider(value)
  }

  function applySnapshot(rawLine) {
    var status = Sanitizer.parseSnapshot(rawLine)
    if (!status.ok) {
      lastError = status.error
      return
    }
    paused = status.paused
    reviewStartsInVim = status.reviewStartsInVim
    languagetoolEnabled = status.languagetoolEnabled
    llmConfigured = status.llmConfigured
    defaultProvider = status.defaultProvider
    smartProvider = status.smartProvider
    fixWord = status.fixWord
    fixSentence = status.fixSentence
    review = status.review
    reviewLlm = status.reviewLlm
    lastError = status.error
    connected = true
  }

  function runAction(args) {
    if (busy || !connected || !(args instanceof Array) || args.length === 0)
      return false

    actionError = ""
    actionProcess.command = [bridgePath].concat(args)
    actionProcess.running = true
    return true
  }

  function setDefaultProvider(provider) {
    provider = cleanProvider(provider)
    if (provider === defaultProvider)
      return
    runAction(["set-default", provider])
  }

  function setSmartProvider(provider) {
    provider = cleanProvider(provider)
    if (provider === smartProvider)
      return
    runAction(["set-smart", provider])
  }

  function setVimMode(enabled) {
    runAction(["set-vim", enabled === true ? "true" : "false"])
  }

  function togglePause() {
    runAction(["toggle-pause"])
  }

  function openPreferences() {
    if (!busy)
      runAction(["open-prefs"])
  }

  Process {
    id: watchProcess
    running: root.enabled
    command: [root.bridgePath, "watch"]

    stdout: SplitParser {
      onRead: function(line) { root.applySnapshot(line) }
    }

    stderr: SplitParser {
      onRead: function(line) {
        var message = root.cleanText(line, 180)
        if (message !== "") root.lastError = message
      }
    }

    onExited: function(exitCode) {
      root.connected = false
      if (root.enabled) reconnectTimer.restart()
    }
  }

  Process {
    id: actionProcess
    running: false
    command: [root.bridgePath, "watch"]

    stderr: SplitParser {
      onRead: function(line) {
        var message = root.cleanText(line, 180)
        if (message !== "") root.actionError = message
      }
    }

    onExited: function(exitCode) {
      if (exitCode !== 0 && root.actionError === "")
        root.actionError = "Hyprcorrect could not apply that change"
    }
  }

  Timer {
    id: reconnectTimer
    interval: 1000
    repeat: false
    onTriggered: if (root.enabled && !watchProcess.running) watchProcess.running = true
  }
}
