import QtQuick
import QtTest
import "../CompanionSanitizer.js" as Sanitizer

TestCase {
  name: "CompanionSanitizer"

  function test_frame_limit_and_malformed_values() {
    compare(Sanitizer.parseSnapshot("").ok, false)
    var exact = '{"schema_version":1}'
    exact += " ".repeat(8192 - exact.length)
    compare(Sanitizer.parseSnapshot(exact).ok, true)
    compare(Sanitizer.parseSnapshot("x".repeat(8193)).ok, false)
    compare(Sanitizer.parseSnapshot("{").ok, false)
    compare(Sanitizer.parseSnapshot("null").ok, false)
    compare(Sanitizer.parseSnapshot("[]").ok, false)
    compare(Sanitizer.parseSnapshot("true").ok, false)
  }

  function test_external_fields_are_plain_bounded_and_validated() {
    var hostile = "<b>unsafe</b>\n\u202e" + "x".repeat(200)
    var parsed = Sanitizer.parseSnapshot(JSON.stringify({
      schema_version: 1,
      paused: true,
      default_provider: "$(touch /tmp/nope)",
      smart_provider: "llm",
      hotkeys: {
        fix_word: hostile,
        fix_sentence: "CTRL+S"
      },
      error: hostile
    }))
    compare(parsed.ok, true)
    compare(parsed.paused, true)
    compare(parsed.defaultProvider, "spellbook")
    compare(parsed.smartProvider, "llm")
    verify(parsed.fixWord.indexOf("<b>unsafe</b>") === 0)
    verify(parsed.fixWord.indexOf("\n") === -1)
    verify(parsed.fixWord.indexOf("\u202e") === -1)
    compare(parsed.fixWord.length, 96)
    compare(parsed.error.length, 180)
  }

  function test_non_object_hotkeys_fall_back_safely() {
    var parsed = Sanitizer.parseSnapshot(JSON.stringify({
      schema_version: 1,
      hotkeys: ["CTRL+X"]
    }))
    compare(parsed.ok, true)
    compare(parsed.fixWord, "")
  }
}
