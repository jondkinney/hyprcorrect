import QtQuick
import Quickshell
import Quickshell.Io

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
    return String(value || "")
      .replace(/[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/g, "")
      .substring(0, limit)
  }

  function cleanProvider(value) {
    var provider = cleanText(value, 24)
    return provider === "spellbook" || provider === "languagetool" || provider === "llm"
      ? provider
      : "spellbook"
  }

  function applySnapshot(rawLine) {
    var line = String(rawLine || "")
    if (line.length === 0 || line.length > 8192) {
      lastError = "Hyprcorrect returned an invalid status frame"
      return
    }

    var status
    try {
      status = JSON.parse(line)
    } catch (error) {
      lastError = "Hyprcorrect returned unreadable status"
      return
    }
    if (Number(status.schema_version) !== 1) {
      lastError = "Update Hyprcorrect for this companion version"
      return
    }

    var hotkeys = status.hotkeys && typeof status.hotkeys === "object" ? status.hotkeys : ({})
    paused = status.paused === true
    reviewStartsInVim = status.review_starts_in_vim === true
    languagetoolEnabled = status.languagetool_enabled === true
    llmConfigured = status.llm_configured === true
    defaultProvider = cleanProvider(status.default_provider)
    smartProvider = cleanProvider(status.smart_provider)
    fixWord = cleanText(hotkeys.fix_word, 96)
    fixSentence = cleanText(hotkeys.fix_sentence, 96)
    review = cleanText(hotkeys.review, 96)
    reviewLlm = cleanText(hotkeys.review_llm, 96)
    lastError = cleanText(status.error, 180)
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
