import QtQuick
import Quickshell
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "io.github.jondkinney.hyprcorrect"

  property bool opened: false
  readonly property color popupForeground: bar ? bar.foreground : Color.popups.text
  readonly property var providerOptions: {
    var options = [{ value: "spellbook", label: "Spellbook · offline" }]
    if (hyprcorrect.languagetoolEnabled
        || hyprcorrect.defaultProvider === "languagetool"
        || hyprcorrect.smartProvider === "languagetool")
      options.push({ value: "languagetool", label: hyprcorrect.languagetoolEnabled ? "LanguageTool" : "LanguageTool · disabled" })
    if (hyprcorrect.llmConfigured
        || hyprcorrect.defaultProvider === "llm"
        || hyprcorrect.smartProvider === "llm")
      options.push({ value: "llm", label: hyprcorrect.llmConfigured ? "LLM" : "LLM · setup needed" })
    return options
  }
  readonly property var hotkeyRows: [
    { label: "Fix last word", value: hyprcorrect.fixWord },
    { label: "Fix sentence", value: hyprcorrect.fixSentence },
    { label: "Review correction", value: hyprcorrect.review },
    { label: "Escalate review", value: hyprcorrect.reviewLlm }
  ]

  function open() { opened = true }
  function close() { opened = false }
  function closeForPopoutSwitch() { close() }
  function toggle() { opened ? close() : open() }

  function shortcutDisplay(canonical) {
    var value = String(canonical || "").substring(0, 96)
    if (value === "") return "Unbound"
    var glyphs = {
      "CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Alt", "SUPER": "Super",
      "ENTER": "Enter", "ESC": "Esc", "TAB": "Tab", "SPACE": "Space",
      "UP": "↑", "DOWN": "↓", "LEFT": "←", "RIGHT": "→"
    }
    return value.split("+").map(function(token) { return glyphs[token] || token }).join(" + ")
  }

  visible: hyprcorrect.connected
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  HyprcorrectService {
    id: hyprcorrect
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    active: root.opened || hyprcorrect.paused
    dimmed: hyprcorrect.paused
    tooltipText: hyprcorrect.paused ? "Hyprcorrect · paused" : "Hyprcorrect · correcting"
    onPressed: function(mouseButton) {
      if (mouseButton === Qt.MiddleButton) hyprcorrect.togglePause()
      else if (mouseButton === Qt.RightButton) hyprcorrect.openPreferences()
      else root.toggle()
    }

    iconComponent: Component {
      Image {
        source: Qt.resolvedUrl("../../assets/icons/hicolor/scalable/apps/hyprcorrect.svg")
        sourceSize.width: width * 2
        sourceSize.height: height * 2
        fillMode: Image.PreserveAspectFit
        smooth: true
        opacity: hyprcorrect.paused ? 0.5 : 1.0
      }
    }
  }

  PopupCard {
    id: popup
    anchorItem: button
    bar: root.bar
    owner: root
    open: root.opened
    contentWidth: popup.fittedContentWidth(Style.space(360))
    contentHeight: popup.fittedContentHeight(content.implicitHeight, Style.space(680))

    Column {
      id: content
      anchors.fill: parent
      spacing: Style.spacing.md

      Row {
        width: parent.width
        spacing: Style.spacing.md

        Image {
          width: Style.space(34)
          height: width
          source: Qt.resolvedUrl("../../assets/icons/hicolor/scalable/apps/hyprcorrect.svg")
          sourceSize.width: width * 2
          sourceSize.height: height * 2
          fillMode: Image.PreserveAspectFit
          smooth: true
        }

        Column {
          width: parent.width - Style.space(34) - parent.spacing
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.spacing.xxs

          Text {
            width: parent.width
            text: "Hyprcorrect"
            textFormat: Text.PlainText
            color: root.popupForeground
            font.family: Style.font.family
            font.pixelSize: Style.font.title
            font.bold: true
          }

          Text {
            width: parent.width
            text: hyprcorrect.paused ? "Keyboard capture is paused" : "Watching for correction shortcuts"
            textFormat: Text.PlainText
            color: root.popupForeground
            opacity: 0.62
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
          }
        }
      }

      Button {
        width: parent.width
        text: hyprcorrect.paused ? "Resume keyboard capture" : "Pause keyboard capture"
        iconText: hyprcorrect.paused ? "󰐊" : "󰏤"
        foreground: root.popupForeground
        bordered: true
        focusable: true
        leftAlign: true
        enabled: !hyprcorrect.busy
        onClicked: hyprcorrect.togglePause()
      }

      PanelSectionHeader {
        text: "CORRECTION ROUTING"
        textFormat: Text.PlainText
        foreground: root.popupForeground
      }

      Dropdown {
        width: parent.width
        label: "Quick word fixes"
        value: hyprcorrect.defaultProvider
        options: root.providerOptions
        foreground: root.popupForeground
        enabled: !hyprcorrect.busy
        onChanged: function(provider) { hyprcorrect.setDefaultProvider(provider) }
      }

      Dropdown {
        width: parent.width
        label: "Sentences and review"
        value: hyprcorrect.smartProvider
        options: root.providerOptions
        foreground: root.popupForeground
        enabled: !hyprcorrect.busy
        onChanged: function(provider) { hyprcorrect.setSmartProvider(provider) }
      }

      Toggle {
        width: parent.width
        label: "Start review in Vim mode"
        description: "Ctrl+E still switches between Vim and word editing."
        checked: hyprcorrect.reviewStartsInVim
        foreground: root.popupForeground
        enabled: !hyprcorrect.busy
        onClicked: hyprcorrect.setVimMode(!hyprcorrect.reviewStartsInVim)
      }

      PanelSectionHeader {
        text: "CURRENT KEYBINDINGS"
        textFormat: Text.PlainText
        foreground: root.popupForeground
      }

      Repeater {
        model: root.hotkeyRows

        delegate: Row {
          required property var modelData
          width: content.width
          spacing: Style.spacing.md

          Text {
            width: parent.width * 0.42
            text: modelData.label
            textFormat: Text.PlainText
            color: root.popupForeground
            opacity: 0.64
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
            elide: Text.ElideRight
          }

          Text {
            width: parent.width * 0.58 - parent.spacing
            text: root.shortcutDisplay(modelData.value)
            textFormat: Text.PlainText
            color: root.popupForeground
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
            horizontalAlignment: Text.AlignRight
            elide: Text.ElideLeft
          }
        }
      }

      Text {
        visible: hyprcorrect.lastError !== "" || hyprcorrect.actionError !== ""
        width: parent.width
        text: hyprcorrect.actionError || hyprcorrect.lastError
        textFormat: Text.PlainText
        color: Color.urgent
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        wrapMode: Text.WordWrap
        maximumLineCount: 3
        elide: Text.ElideRight
      }

      Rectangle {
        width: parent.width
        height: Math.max(1, Style.normalBorderWidth)
        color: root.popupForeground
        opacity: 0.16
      }

      Button {
        width: parent.width
        text: "Open full Preferences"
        iconText: "󰒓"
        foreground: root.popupForeground
        bordered: true
        focusable: true
        leftAlign: true
        enabled: !hyprcorrect.busy
        onClicked: hyprcorrect.openPreferences()
      }

      Text {
        width: parent.width
        text: "API keys, provider endpoints, LanguageTool setup, hotkey recording, reset behavior, and app privacy stay in Preferences."
        textFormat: Text.PlainText
        color: root.popupForeground
        opacity: 0.5
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        wrapMode: Text.WordWrap
      }
    }
  }
}
