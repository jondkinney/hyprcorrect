# HyprCorrect

Keyboard-driven spelling and typo correction for the whole desktop.
Press a chord and the word — or sentence — you just typed is checked
and fixed in place, in whatever app has focus, terminals included.

Hyprland-first on Linux, with a sibling macOS backend (CGEventTap
capture, `CGEvent` synthetic input, a Carbon global hotkey, and an
`NSStatusItem` menu-bar item — see [`DESIGN.md`](DESIGN.md) for the
platform interface). Windows is a stub.

Site: <https://hyprcorrect.com>

## Highlights

- **Per-window keystroke buffer** driven by Hyprland focus events —
  switching windows doesn't poison the buffer of another, and
  returning to a window restores its prior state.
- **Configurable chord(s)**: `fix-last-word`, `fix-last-sentence`,
  and a `review` popup are each bindable to any
  Ctrl / Shift / Alt / Super combination plus a key, recorded
  directly in Preferences (the daemon mediates the recording over
  IPC so Super-containing chords work too).
- **Three correction providers**:
  - `spellbook` — bundled, offline, instant (Hunspell-compatible
    en_US dictionary).
  - `languagetool` — POST to a self-hosted LanguageTool server's
    `/v2/check`. Preferences ships a one-click "Install with Docker"
    convenience that pulls `erikvl87/languagetool` and runs it
    locally on the port in your URL.
  - `llm` — Anthropic Messages API (Haiku by default); the key
    lives in the OS keychain.

  Any provider failure (no key, network error, LanguageTool down)
  falls back to spellbook and fires a desktop toast so the chord
  never silently no-ops.
- **Privacy blocklist** — pick running apps from a dropdown
  (populated from `hyprctl clients`, with `.desktop` icons and
  display names) whose keys should never be buffered.
- **Pause control + clean shutdown** from the tray; the daemon
  uninstalls its Hyprland keybind on exit and removes its PID
  file so dev rebuilds don't accumulate stale binds.

## Install

### From the AUR (recommended on Arch)

```sh
yay -S hyprcorrect          # builds from the latest tagged release
# or
yay -S hyprcorrect-bin      # prebuilt binary from the GitHub release
# or
yay -S hyprcorrect-git      # tracks main
```

PKGBUILDs live in [`packaging/aur/`](packaging/aur),
[`packaging/aur-bin/`](packaging/aur-bin), and
[`packaging/aur-git/`](packaging/aur-git).

### From crates.io

```sh
cargo install hyprcorrect
hyprcorrect install-desktop   # register the icon + launcher entry
```

`cargo install` drops only the binary in `~/.cargo/bin` — no icon, no
`.desktop` entry — so on its own hyprcorrect won't appear in launchers
or file managers. `install-desktop` writes the app icon and `.desktop`
entry into your XDG data dir. The daemon also does this automatically on
its **first launch** (once — not on every start), so running the command
is optional: use it when you want the launcher entry before that first
run, or to refresh the icon and entry after a rebuild.

### From source

```sh
git clone https://github.com/jondkinney/hyprcorrect
cd hyprcorrect
cargo build --release
cargo install --path crates/hyprcorrect
```

On **Linux** you also need the runtime dependencies the AUR packages
declare: `wtype`, `hyprland`, `libxkbcommon`, `libsecret`, `wayland`,
`libglvnd`, `fontconfig`, `freetype2`. `wl-clipboard` is optional
(enables the empty-buffer clipboard fallback). On **macOS** there are
no extra runtime dependencies — capture/emit/hotkey/tray are built
against the system frameworks (Core Graphics, AppKit, Carbon) — just
the two TCC permissions below. Rust 1.85+ is required.

## Setup

1. **Add yourself to the `input` group** so the daemon can read
   `/dev/input/event*`:

   ```sh
   sudo usermod -aG input "$USER"
   ```

   …and log back in. (`getent group input` should now show your
   user.)

2. **Start the daemon.** Run `hyprcorrect` in a terminal, or enable
   *Behavior → Start at login* in Preferences — that drops a
   `~/.config/autostart/hyprcorrect.desktop` entry so the daemon
   launches with your session.

3. **Press Ctrl+Shift+Alt+Super+F** in any window to fix the last
   word. The default chord is configurable in *Preferences →
   Hotkeys* (right-click the tray icon → *Open Preferences…*).

### macOS

Build and run the daemon the same way (`cargo build --release` then
run `hyprcorrect`); it appears as a menu-bar item, not a Dock app.

On **macOS 13+ the only permission you grant is Accessibility**
(*System Settings → Privacy & Security → Accessibility*). That single
grant covers both halves of what hyprcorrect does — watching keystrokes
(the capture tap) and typing the correction (synthetic events) — so
hyprcorrect usually never even appears in the separate Input Monitoring
list. On launch it fires the Accessibility prompt; just enable
hyprcorrect there. (On older macOS 11–12 the capture tap needs the
separate **Input Monitoring** grant as well, since the two aren't
unified yet.)

Both capabilities are latched at process start, so a grant that lands
while hyprcorrect is already running can't activate them in place.
**You don't need to restart it manually, though** — the daemon watches
for the grant and relaunches itself automatically the moment you enable
Accessibility (from the `.app` it reopens the bundle; a `cargo run` dev
build re-spawns the binary). Toggle it on and corrections start working
within a second or two.

The trigger chord's *Super* is ⌘; the default
`Ctrl+Shift+Alt+Super+F` is `⌃⇧⌥⌘F`. Because the chord is a Carbon
`RegisterEventHotKey`, the OS intercepts it, so it never leaks into the
focused app. Focus is tracked at the **app** level (per-bundle-id
buffers) on macOS, so the privacy blocklist takes bundle identifiers
(`com.apple.Terminal`).

## Usage

| Action | Default chord | What it does |
|---|---|---|
| **Fix last word** | `Ctrl+Shift+Alt+Super+F` | Backspaces the last word in the focused window's buffer, types the top suggestion from the *Default* provider. With LLM as default, the surrounding sentence is sent for context so homophones (`their` / `there`) can be disambiguated. Falls back to the clipboard / selection path when the buffer is empty. |
| **Fix last sentence** | `Ctrl+Shift+Alt+Super+S` | Sends the last sentence through the *Smart* provider (LLM, LanguageTool, or spellbook). |
| **Review correction** | `Ctrl+Shift+Alt+Super+R` | Same as above, but shows a popup with the proposed correction first. Press Enter to apply or Esc to cancel. |

Set `HYPRCORRECT_CHORD=CTRL+J` (for example) to override
*fix-last-word* for one-off dev runs without editing the config.

## Preferences

Run `hyprcorrect prefs` or click *Open Preferences…* in the tray.

- **Hotkeys** — record a chord for each action by clicking the chip
  and pressing the combo (Esc cancels). The daemon temporarily
  releases its bind during capture so the chord you're recording
  isn't intercepted.
- **Providers** — pick the *Default* and *Smart* providers,
  configure the LLM (backend, model, API key — stored in the OS
  keychain) and LanguageTool (URL plus the one-click Docker
  installer).
- **Behavior** — *Start at login* toggle; *Pause per backspace*
  slider (0–30 ms, default 8 ms; raise it if a slow app leaves a
  prefix of the original on screen after a fix); *Buffer reset
  keys* — checkboxes for Enter, Tab, Esc, the arrow keys, Page
  Up/Down, forward Delete, and Insert, which let you choose which
  keys clear the per-window typing buffer.
- **Privacy** — app blocklist. The dropdown shows window classes
  reported by `hyprctl clients`, with icons and display names
  resolved from your `.desktop` registry; there's also a manual
  text-entry field for apps that aren't running yet.
- **About** — version, source link, license.

Save writes the config to disk, persists the API key to the OS
keychain, and sends `SIGHUP` to the running daemon so it picks up
the change without a restart.

## Troubleshooting

**The chord doesn't do anything.** Check the daemon is running:
`pgrep -ax hyprcorrect`. Check the bind is installed:
`hyprctl binds | grep hyprcorrect`. If the chord echoes its raw
escape sequence (e.g. `^[[102;8u`) into a terminal, Hyprland isn't
intercepting it — make sure the daemon has been signaled to install
the bind (re-save from Preferences, or restart the daemon).

**Capture sees no keys.** Verify you're in the `input` group:
`groups | grep input`. The session group list is set at login —
`sudo usermod -aG input "$USER"` requires logging out and back in.
The daemon prints a clear error if it can't read `/dev/input/event*`.

**Tray icon disappears when paused.** The daemon stays SNI-active
and swaps the icon glyph instead of using the "this isn't
important" hint that Waybar hides by default — so Pause keeps the
tray entry visible.

**Provider failed silently.** Provider errors (LLM network failure,
missing API key, LanguageTool unreachable) raise a desktop
notification via `notify-send` and the daemon falls back to
spellbook. Install `libnotify` if no toast appears.

**LanguageTool URL maps to the wrong port.** The Docker installer
parses the host port out of *Providers → LanguageTool → URL* — so
set the URL first (e.g. `http://localhost:8081`), then click
*Install with Docker*. The button is disabled until the URL has
an explicit port.

## Architecture

The full design — per-OS interface contract, signal protocol, IPC
shape — is in [`DESIGN.md`](DESIGN.md). Briefly:

- A **4-crate workspace**: `hyprcorrect-core` (pure logic),
  `hyprcorrect-platform` (per-OS capture / emit / hotkey / tray /
  focus), `hyprcorrect-ui` (platform-independent egui),
  `hyprcorrect` (the binary).
- Linux: `evdev` capture, `xkbcommon` translation, per-window
  buffers via Hyprland IPC, `wtype` emit, `hyprctl keyword bind`
  for the chord (whose `exec` raises `SIGUSR1` on the daemon),
  `ksni` tray, `keyring` for secrets, egui prefs.
- macOS mirrors that interface under
  `crates/hyprcorrect-platform/src/macos/`: a listen-only `CGEventTap`
  for capture (Input Monitoring), `CGEvent` +
  `CGEventKeyboardSetUnicodeString` for emit (Accessibility), Carbon
  `RegisterEventHotKey` for the trigger (which raises `SIGUSR1`, so the
  signal protocol is shared), `NSWorkspace.frontmostApplication` for
  app-level focus, and an `NSStatusItem` menu bar. AppKit runs on the
  main thread (`bootstrap_main`) with the daemon loop on a worker. The
  shared daemon (`main.rs`) is identical across both platforms via a
  `backend` alias.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.

The bundled `en_US` spelling dictionary is vendored from
[wooorm/dictionaries](https://github.com/wooorm/dictionaries) (derived
from SCOWL) under its own permissive license — see
[`crates/hyprcorrect-core/dictionaries/en_US/LICENSE`](crates/hyprcorrect-core/dictionaries/en_US/LICENSE).
