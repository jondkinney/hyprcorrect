# hyprcorrect — Design

**Status:** design / pre-implementation.

## What it is

hyprcorrect is a keyboard-driven spelling/typo corrector for the whole
desktop. Press a hotkey and the word — or sentence — you just typed is
checked and fixed in place, in whatever application has focus, terminals
included. A second hotkey opens a small popup with suggestions you
navigate and apply by keyboard. No mouse, no right-click menus.

It is the fourth in a family of cross-platform Rust desktop utilities
(alongside `tensaku`, `vernier`, `mousehop`) and follows their
conventions. **`vernier` is the structural template.**

Targets: macOS and Linux/Wayland (Hyprland is the primary target), built
together from day one; Windows scaffolded as a stub.

## Goals / non-goals

**Goals**

- Fix the last word / last N words / last sentence with a single
  keypress, in place.
- Work in *any* focused app, terminals included.
- Fully keyboard-driven, including the suggestion popup.
- Pluggable correction backends; offline by default.
- Simple egui config GUI: rebind hotkeys, pick and configure providers.
- One codebase, both platforms.

**Non-goals (for now)**

- Live as-you-type checking or squiggly underlines. hyprcorrect acts
  only on the hotkey.
- Fixing arbitrary, older text far from the caret in terminals — not
  reliably possible (see *Terminals*).
- Grammar rewriting / style. Spelling and typos are the scope; grammar
  is whatever the chosen provider happens to offer.
- Windows as a shipping target initially.

## The core decision: a keystroke buffer

There are three ways to "grab" the word to correct:

| Approach | How | Verdict |
|---|---|---|
| Clipboard + simulated selection | Select-word, copy, read clipboard, paste correction | Universal-ish, but clipboard races, per-app keybindings, and **fails in terminals** |
| Accessibility APIs | Read the focused element's text/selection | Clean where supported; patchy coverage, no working cross-app story on Wayland |
| **Keystroke buffer** | A global listener buffers what was typed; replace via backspace + retype | **Works everywhere, terminals included** |

hyprcorrect keeps its own rolling buffer of recently typed text via a
global key listener. When a hotkey fires it already knows the last word
or sentence — nothing is read back from the focused app. Replacement is
"emit N backspaces, type the correction," which works everywhere.
(This is the same approach Espanso uses.)

This makes the manual two-word-highlight step from the macOS prototype
unnecessary: "fix last 2 words" / "fix last sentence" are just buffer
queries.

**Secondary mode:** when the buffer is empty or untrusted (see *Reset
triggers*), hyprcorrect falls back to selection + clipboard — simulate
word-select, copy, correct, paste, restore — the prototype's method.
Best-effort; does not work in terminals.

### Terminals

Replacement works in terminals: backspace deletes from the shell line
editor, typed characters insert. *Reading* does not — the
selection/clipboard fallback cannot select a shell command line. So in a
terminal only the buffer path is available, which is exactly the "fix
what I just typed" case. Fixing older text already on a terminal line is
out of scope.

## Architecture

A 4-crate workspace, modeled on `vernier`:

```
hyprcorrect/
  Cargo.toml                  # workspace
  crates/
    hyprcorrect-core/         # config, keystroke buffer, replacement
                              #   planning, CorrectionProvider trait +
                              #   spellbook / LLM / LanguageTool impls
    hyprcorrect-platform/     # per-OS capture, synthetic input, hotkeys,
                              #   frontmost-app, tray
                              #   (src/linux/ src/macos/ src/windows/)
    hyprcorrect-ui/           # egui preferences window + suggestion popup
    hyprcorrect/              # the binary / daemon (package & binary: hyprcorrect)
```

Runtime model (like `vernier`): one binary running as a background
daemon — a tray item on Linux (`ksni`), a menu-bar item on macOS
(`NSStatusItem`). The egui preferences window opens on demand; the
suggestion popup appears on the hotkey. Subcommands
(`hyprcorrect`, `hyprcorrect --prefs`, `hyprcorrect fix-word`, …) rather
than `mousehop`'s separate-GUI + IPC split.

## Platform layer

Each capability sits behind a common trait in `hyprcorrect-platform`,
with a per-OS backend:

| Capability | Linux / Wayland | macOS | Windows (stub) |
|---|---|---|---|
| Key capture (observe-only) | `evdev` `/dev/input` + `xkbcommon` for keycode→char | `CGEventTap` (listen) | `WH_KEYBOARD_LL` |
| Synthetic input | `virtual-keyboard-v1` protocol (wtype-style) | `CGEvent` + unicode string | `SendInput` |
| Global hotkey | Hyprland `hyprctl keyword bind` + `SIGUSR1` (today); `ashpd` GlobalShortcuts portal once compositor auto-bind matures | `RegisterEventHotKey` (Carbon) | `RegisterHotKey` |
| Focused app | Hyprland IPC (`activewindow`) | `NSWorkspace.frontmostApplication` | `GetForegroundWindow` |
| Tray / menu bar | `ksni` | `NSStatusItem` | — |

Notes:

- **Linux capture** is `evdev`, observe-only — read events, never grab.
  Needs read access to `/dev/input` (the `input` group); setup detects
  and guides this. Keycodes are mapped to characters through
  `xkbcommon` so the user's layout and dead keys are honored.
- **Linux emulation** uses the `virtual-keyboard-unstable-v1` Wayland
  protocol — the technique `wtype` uses (upload a transient keymap, send
  keysyms) — so it needs no extra daemon or permissions on wlroots
  compositors. `ydotool`/uinput is a documented fallback for non-wlroots.
  `enigo` is evaluated as a single cross-platform emulation crate; if its
  Wayland path is insufficient we keep the direct protocol impl.
- **Hotkeys:** on Hyprland today the daemon adds an inline
  `hyprctl keyword bind` whose `exec` raises `SIGUSR1` on itself —
  Hyprland intercepts the chord so terminals never see it, and the
  daemon manages its own keybind (no `hyprland.conf` edit required).
  The `ashpd` GlobalShortcuts portal is the planned cross-compositor
  route; `xdg-desktop-portal-hyprland` 1.3 doesn't yet honor
  `preferred_trigger`, so we'll revisit it together with other
  compositors. The GUI's hotkey picker writes whichever mechanism is
  active.
- **macOS** uses the OS-provided unicode for both capture
  (`CGEventKeyboardGetUnicodeString`) and typing
  (`CGEventKeyboardSetUnicodeString`), so no manual keymap handling.
  Needs Accessibility + Input Monitoring (TCC); the daemon
  detects/prompts (`mousehop`'s TCC probe/watch are the pattern).
- All backends use **permissively-licensed crates only** (`evdev`,
  `xkbcommon`, `wayland-client`, `signal-hook`, `ksni`, `ashpd`,
  `objc2-*`, `windows`, `enigo`). No lan-mouse/GPL-derived code —
  hyprcorrect is MIT/Apache like `vernier`.

## The keystroke buffer

A bounded, in-memory rolling buffer of characters typed in each focused
element. The daemon keeps **one buffer per window**, keyed by the
compositor's window address; the active buffer is whichever window
currently has focus.

- Printable keys append to the active window's buffer; Backspace pops.
- Queries: last word, last N words, last sentence (sentence = split on
  `.!?` with simple boundary rules — trivial since we hold the literal
  text).
- **Reset triggers** — anything within the focused window that means
  the caret may no longer sit at the buffer's end:
  arrow/Home/End/PageUp-Down, Ctrl+arrows, Enter, Tab, Esc, undo/redo.
  Only the active window's buffer is cleared; other windows' buffers
  are untouched. After a reset "fix last word" does nothing until
  typing resumes — correct and safe (better than corrupting text). The
  selection fallback covers "fix something I didn't just type."
- **Per-window storage:** switching focus does *not* clear buffers, so
  returning to a window and triggering can still fix the last word you
  typed there. Buffers are dropped when their window closes.
- **Inherent limit of the keystroke model:** events the daemon cannot
  observe — mouse clicks inside the window, app-driven edits
  (autocomplete, autocorrect, paste), undo/redo done with a mouse — can
  leave a window's buffer out of sync with what is actually on screen.
  In that state the chord will garble text. The review popup (M4) is
  the safety net: it shows the planned edit before it lands.
- After applying a correction the buffer is rewritten to the corrected
  text so fixes can chain; if anything is uncertain it resets instead.

**Known limitations:** IME composition (the listener sees raw keys, not
composed text — macOS's unicode events soften this; flagged for
non-Latin input), and very fast synthetic typing occasionally dropping
characters in some apps (mitigated by a configurable inter-key delay).

## Replacement mechanics

Given the buffer ends with `<word><trailing-whitespace>` and the caret
is after the whitespace:

1. Let `tail` = the trailing whitespace run, `word` = the word before it.
2. Emit `len(tail) + len(word)` backspaces.
3. Type `correction + tail`.

The caret ends where it started and surrounding spacing is preserved.
This is the clean form of the prototype's "select word+space, strip the
space" trick. "Fix last sentence" is the same over a larger span; the
span never crosses a newline because Enter is a reset trigger.

## Correction providers

A pluggable trait in `hyprcorrect-core`:

```rust
#[async_trait]
trait CorrectionProvider {
    /// `text` is the buffer slice to correct; `ctx` carries the
    /// focused-app id and the user's locale.
    async fn check(&self, text: &str, ctx: &Context) -> Result<Vec<Correction>>;
}

struct Correction {
    span: Range<usize>,         // byte range within `text`
    original: String,
    suggestions: Vec<String>,   // best-first
}
```

Shipped implementations:

| Provider | Locality | Use | Notes |
|---|---|---|---|
| **spellbook** | in-process, offline | bundled default | Pure-Rust, Hunspell-compatible — one dependency. Spell-check + suggestions over the standard en_US dictionary; instant, English. |
| **LLM** (Claude/OpenAI) | network | contextual + sentence | Best at ambiguous cases (`vernuer` → `veneer` vs `vernier`) and whole-sentence fixes; needs an API key; ~1s latency. Reference impl: Anthropic, a fast model (e.g. Haiku) with prompt caching. |
| **LanguageTool** (HTTP) | network (self-host) | optional | POSTs to a configurable `/v2/check` URL. Off until a URL is set — for when you run your own server. No bundled Java. |

**Routing:** "fix last word" → spellbook (instant, local). "fix last
sentence" / "show options" → the configured smart provider (LLM if a key
is set, else spellbook). Offline-first-then-LLM-on-demand is a config
option. This offline+LLM split is deliberate: spellbook kills obvious
typos with zero latency and zero network; the LLM handles genuinely
ambiguous corrections that need context — the cases the Google-search
prototype was really being used for.

## Interaction modes

Actions are a list of bindable commands in config; each can be bound to a
hotkey:

- `fix-last-word` — quick, no UI. Apply the top suggestion in place.
  (The single-key flow from the macOS prototype.)
- `fix-last-sentence` — quick, no UI.
- `review` — open the popup for the last word / N words / sentence.

The **review popup** (egui): shows the text with flagged words marked;
←/→ or Tab moves between flagged words, ↑/↓ cycles suggestions, Enter
accepts the current word, a key applies all, Esc cancels. On Wayland it
is an egui window plus a shipped Hyprland window rule (float/pin/focus)
for MVP; a real `wlr-layer-shell` surface is a later upgrade. On macOS it
is a borderless `NSPanel`.

## Configuration & GUI

- `config.toml` under the platform config directory, resolved by the
  `directories` crate (`~/.config/hyprcorrect/` on Linux); `toml` + `serde`.
- Secrets (LLM API keys) go in the OS keychain via the `keyring` crate
  (macOS Keychain, libsecret/kwallet, Windows Credential Manager) —
  never in `config.toml`.
- egui preferences window (`hyprcorrect-ui`, pattern from
  `vernier-ui/prefs.rs`), panels: Hotkeys, Providers, Behavior
  (inter-key delay, reset sensitivity), Privacy (app blocklist, password
  handling), About.

```toml
# config.toml sketch
[hotkeys]
fix-last-word = "..."          # hyprctl chord / Carbon binding descriptor
review        = "..."

[providers]
default = "spellbook"
smart   = "llm"                # used by fix-last-sentence / review

[providers.llm]
backend = "anthropic"
model   = "claude-haiku-4-5"
# api key lives in the OS keychain, not here

[providers.languagetool]
enabled = false
url     = "http://localhost:8081"

[behavior]
inter_key_delay_ms = 2

[privacy]
app_blocklist = ["1password", "keepassxc"]
```

## Security & privacy

hyprcorrect is, mechanically, a global key listener. It is designed
defensively:

- The buffer is in-memory only, bounded, never written to disk, and
  never logged (text is redacted even at debug level).
- **Password / secure fields:** buffering is suppressed. macOS exposes
  secure-input state (`IsSecureEventInputEnabled`) and the tap stops
  receiving keys there anyway; on Wayland, where field roles are not
  reliably exposed, suppression leans on an app blocklist and a manual
  pause.
- A visible **pause control** (tray menu + a hotkey) and a tray
  indicator showing capture state.
- Typed text leaves the machine only when a network provider (LLM,
  remote LanguageTool) is the active backend, and only the snippet being
  corrected. The spellbook default keeps everything local. The GUI states
  this plainly per provider.

## Licensing

MIT OR Apache-2.0, matching `vernier`. Only permissively-licensed
dependencies; no code derived from lan-mouse/`mousehop` (GPL). The
synthetic-input layer is written fresh against the relevant OS APIs and
Wayland protocols.

## Phased build plan

| Milestone | Deliverable |
|---|---|
| **M0 — Scaffold** | `git init`; 4-crate workspace, edition 2024, shared deps, `rust-toolchain.toml`, dual license, CI + `release-plz` skeleton. Mirrors `vernier`. |
| **M1 — Linux quick-fix slice** | `evdev` capture + xkb mapping → per-window buffers driven by Hyprland focus events; offline spell-check provider (spellbook); `wtype`-based synthetic input; one hyprctl-bound hotkey signaling `SIGUSR1`; ksni tray; `fix-last-word` working end-to-end on Hyprland incl. terminals. No GUI. Proves the riskiest path. |
| **M2 — macOS parity** | `CGEventTap` capture, `CGEvent` emulation, Carbon hotkey, TCC permission flow. `fix-last-word` on macOS. Core now runs on both. |
| **M3 — Config GUI + tray** | egui prefs (hotkeys/providers/behavior/privacy), `config.toml`, `keyring`, `ksni`/`NSStatusItem` tray, pause control. Hotkeys user-configurable. |
| **M4 — Review popup + sentence mode** | egui popup with keyboard nav; `fix-last-sentence`; multi-word review/apply; LLM provider wired in. |
| **M5 — Selection fallback + polish** | Clipboard/selection secondary mode; per-app behavior; inter-key delay tuning; LanguageTool-HTTP provider; IME caveats handled. |
| **M6 — Packaging** | AUR (source/-bin/-git like `vernier`), macOS dmg (ad-hoc signed), GitHub releases via `release-plz`. Windows remains a stub. |

Riskiest-first: M1 proves the capture → correct → replace engine on the
hardest platform before any UI exists.

## Open questions / risks

- `evdev` requires `input`-group membership on Linux — onboarding
  friction; setup must detect and guide.
- `enigo`'s Wayland emulation may be insufficient → fall back to the
  hand-rolled `virtual-keyboard-v1` impl.
- Fast synthetic typing can drop characters in some apps → configurable
  inter-key delay; may need per-app tuning.
- IME / dead keys / non-Latin layouts in the buffer — degraded; needs
  design before non-English support.
- Frontmost-app detection is compositor-specific on Wayland; solid on
  Hyprland via its IPC, best-effort elsewhere.
- Popup focus/placement on Wayland without layer-shell relies on a
  Hyprland window rule for MVP.
