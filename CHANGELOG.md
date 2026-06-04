# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3](https://github.com/jondkinney/hyprcorrect/compare/v0.2.2...v0.2.3) - 2026-06-04

### Other

- update Cargo.lock dependencies
- bump kanso to 0.1.1 for the macOS-tuned scroll feel

## [0.2.2](https://github.com/jondkinney/hyprcorrect/compare/v0.2.1...v0.2.2) - 2026-06-03

### Other

- rustfmt the prefs footer overlay
- depend on published kanso 0.1.0 instead of the path dep
- *(prefs)* reset section scroll to top on navigation
- *(prefs)* rework the action footer — "Quit" + stale-daemon relaunch overlay
- *(prefs)* pad the content bottom to match the sides
- *(review)* kinetic scroll momentum
- *(prefs)* kinetic scroll_view + searchable app-picker combo
- *(prefs)* route running-app blocklist picker through kanso app_picker
- *(prefs)* route editable_combo through kanso
- *(prefs)* route info_icon through kanso::widgets::info_icon
- *(review)* bring the review popup under the kanso theme
- *(prefs)* adopt kanso widgets + decouple font install
- *(kanso)* styles+scrollbar, font dedup, dirty footer
- *(kanso)* use kanso's control treatment for inputs/buttons
- *(kanso)* delegate text/nav helpers to the design system

## [0.2.1](https://github.com/jondkinney/hyprcorrect/compare/v0.2.0...v0.2.1) - 2026-05-31

### Fixed

- *(macos)* gate the daemon review-llm flow to Linux

## [0.2.0](https://github.com/jondkinney/hyprcorrect/compare/v0.1.3...v0.2.0) - 2026-05-31

### Added

- *(providers)* wire all LLM backends + LanguageTool fallback toggle
- *(review)* key glyphs, per-command vim help, open prefs on Ask LLM
- *(prefs)* vim-start review option; tiling; n-gram + UI polish
- *(prefs)* multi-provider LLM config with tabs, per-backend keys
- *(review)* escalate to the LLM on demand + LanguageTool n-gram install
- *(review)* word-diff column alignment, adaptive width, wrap line-height
- *(review)* responsive popup, revert-to-original, suggestion list
- *(review)* gather ranked per-word backup suggestions
- *(review)* per-option word definitions (offline default, online opt-in)
- *(prefs)* one-click "Download n-grams" button
- *(languagetool)* request the picky rule level
- *(emit)* type newlines as Shift+Enter so multi-line edits don't submit
- *(prefs)* remove the "Clear" links from the Hotkeys page
- *(review)* Add word definition line
- *(prefs)* cap floating window width at 900 via compositor resize
- *(prefs)* editable combo opens below the field; styling pass
- *(prefs)* show app-downloaded n-grams (read-only) with a Remove option
- *(prefs)* track n-grams separately from the LanguageTool container
- *(review)* vim spell-suggest (z=), multi-line fix, Esc, field wrap
- *(review)* column-align the Original in vim mode too
- *(review)* picking a suggestion advances to the next correction
- *(vimedit)* undo, redo, and repeat
- *(review)* align each correction directly under the word it replaces
- *(vimedit)* Home and End keys
- *(review)* backup-suggestion dropdown under the focused field
- *(vimedit)* virtualedit-style vertical navigation
- *(review)* squiggle underlines + borderless auto-sized fields
- *(review)* inline word-edit and Ctrl+E vim modes in the review popup

### Fixed

- *(review)* hide "Ask LLM" when the correction is already the LLM's
- *(prefs)* code-fence class-name caption; drop dead maxsize rule
- *(prefs)* tile by default; cap floating width; persist n-gram folder
- *(prefs)* vernier-style code pills; dropdown width; portal float
- *(hotkeys)* record chord modifiers in canonical CTRL+SHIFT+ALT+SUPER order
- *(prefs)* widen blocklist app picker so its Add button aligns
- *(review)* correct vim caret/squiggle anchoring under the 1.5× line-height
- *(review)* match vim mode's vertical spacing + line-height to word mode
- *(review)* keep the vim caret tight against a word after cw
- *(prefs)* render "Reload n-grams" as a framed button
- *(prefs)* hard-cap floating width via runtime max_size (no overshoot)
- *(prefs)* uniform 30px controls, fixed float, 900px cap, privacy polish
- *(prefs)* equal control heights, balanced padding, Save on key row
- *(prefs)* non-focused border on inputs; right-align Save provider
- *(prefs)* dropdown width, flush scrollbar, combo height, bar padding
- *(prefs)* stop combos overflowing the right margin; n-gram Browse + details
- *(prefs)* cap form column width so inputs don't clip on wide windows
- *(prefs)* correct LanguageTool URL label — it's the base URL
- *(review)* size the popup wide enough to keep the sentence on one line
- *(review)* fold trailing punctuation into a word's column

### Other

- *(prefs)* drop the floating-width cap entirely
- *(prefs)* tidy Providers page with info icons + framed Remove
- *(prefs)* clarify the "Reload n-grams" tooltip
- *(prefs)* move the n-gram folder field below the n-grams controls
- *(review)* roomier action buttons with a filled primary

## [0.1.3](https://github.com/jondkinney/hyprcorrect/compare/v0.1.2...v0.1.3) - 2026-05-28

### Added

- *(app)* add install-desktop subcommand for cargo-install users

### Fixed

- *(app)* gate install-desktop to Linux so macOS builds

### Other

- *(app)* install desktop integration once, not every daemon start

## [0.1.2](https://github.com/jondkinney/hyprcorrect/compare/v0.1.1...v0.1.2) - 2026-05-28

### Added

- *(core)* bind fix_sentence and review by default

## [0.1.1](https://github.com/jondkinney/hyprcorrect/compare/v0.1.0...v0.1.1) - 2026-05-28

### Fixed

- *(icon)* make SVG square so AppStream/Flatpak accept it

## [0.1.0](https://github.com/jondkinney/hyprcorrect/releases/tag/v0.1.0) - 2026-05-27

### Added

- *(linux)* publish brand icon + .desktop to XDG launcher paths
- bundled brand icon drives prefs sidebar + SNI tray
- *(core+linux)* sentence context for LanguageTool word-fix
- *(linux)* provider-aware correction toasts
- *(core+linux+ui)* LanguageTool provider integration
- *(core+linux)* hybrid nearby-word fallback with end-anchored emit
- *(core)* LLM-driven fix-word with sentence context
- *(core)* caret-aware buffer + mid-buffer correction emit path
- *(linux)* float the review popup via hyprctl windowrule
- *(ipc)* daemon singleton + prefs auto-spawns the daemon

### Other

- cargo fmt
- M1 Linux: keyboard-driven correction MVP ([#1](https://github.com/jondkinney/hyprcorrect/pull/1))
- scaffold the workspace (M0)
