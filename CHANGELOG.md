# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/jondkinney/hyprcorrect/compare/v0.1.2...v0.1.3) - 2026-05-28

### Added

- *(core)* bind fix_sentence and review by default

### Other

- update Cargo.lock dependencies

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
