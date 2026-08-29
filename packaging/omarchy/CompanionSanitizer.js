.pragma library

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

function parseSnapshot(rawLine) {
  var line = String(rawLine || "")
  if (line.length === 0 || line.length > 8192)
    return { ok: false, error: "Hyprcorrect returned an invalid status frame" }

  var status
  try {
    status = JSON.parse(line)
  } catch (error) {
    return { ok: false, error: "Hyprcorrect returned unreadable status" }
  }
  if (status === null || typeof status !== "object" || Array.isArray(status))
    return { ok: false, error: "Hyprcorrect returned an invalid status object" }
  if (Number(status.schema_version) !== 1)
    return { ok: false, error: "Update Hyprcorrect for this companion version" }

  var hotkeys = status.hotkeys !== null
      && typeof status.hotkeys === "object"
      && !Array.isArray(status.hotkeys)
    ? status.hotkeys
    : ({})
  return {
    ok: true,
    paused: status.paused === true,
    reviewStartsInVim: status.review_starts_in_vim === true,
    languagetoolEnabled: status.languagetool_enabled === true,
    llmConfigured: status.llm_configured === true,
    defaultProvider: cleanProvider(status.default_provider),
    smartProvider: cleanProvider(status.smart_provider),
    fixWord: cleanText(hotkeys.fix_word, 96),
    fixSentence: cleanText(hotkeys.fix_sentence, 96),
    review: cleanText(hotkeys.review, 96),
    reviewLlm: cleanText(hotkeys.review_llm, 96),
    error: cleanText(status.error, 180)
  }
}
