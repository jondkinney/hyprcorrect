# hyprcorrect

Keyboard-driven spelling and typo correction for the whole desktop.
Press a hotkey and the word — or sentence — you just typed is checked
and fixed in place, in whatever app has focus, terminals included.

Hyprland-first on Linux; macOS is a sibling target ([`DESIGN.md`](DESIGN.md)
covers the platform interface). Windows is a stub.

## Highlights

- **Per-window keystroke buffer** driven by Hyprland focus events —
  switching windows doesn't poison the buffer of another, and
  returning to a window restores its prior state.
- **Configurable chord(s)**: `fix-last-word`, `fix-last-sentence`,
  and a `review` popup are each bindable to any Super/Ctrl/Shift/Alt
  combination + a key.
- **Three correction providers**:
  - `spellbook` — bundled, offline, instant (Hunspell-compatible).
  - `llm` — Anthropic Messages API; key lives in the OS keychain.
  - `languagetool` — POST to your own self-hosted server's
    `/v2/check`.
  Failures fall back to spellbook so the chord never silently no-ops.
- **Privacy blocklist** — pick running apps from a dropdown
  (populated from `hyprctl clients`) whose keys should never be
  buffered.
- **Pause control + clean shutdown** from the tray; the daemon
  uninstalls its Hyprland keybind on exit and removes its PID file
  so dev rebuilds don't accumulate stale binds.

## Install

### From the AUR (recommended on Arch)

```sh
yay -S hyprcorrect          # builds from the latest tagged release
# or
yay -S hyprcorrect-bin      # prebuilt binary from the GitHub release
# or
yay -S hyprcorrect-git      # tracks main
```

PKGBUILDs live in [`packaging/aur/`](packaging/aur), `packaging/aur-bin/`,
and `packaging/aur-git/`.

### From source

```sh
git clone https://github.com/jondkinney/hyprcorrect
cd hyprcorrect
cargo build --release
cargo install --path crates/hyprcorrect
```

You also need the runtime dependencies the AUR packages declare:
`wtype`, `hyprland`, `libxkbcommon`, `libsecret`, `wayland`,
`libglvnd`, `fontconfig`, `freetype2`. `wl-clipboard` is optional
(enables the empty-buffer clipboard fallback).

## Setup

1. **Add yourself to the `input` group** so the daemon can read
   `/dev/input/event*`:

   ```sh
   sudo usermod -aG input "$USER"
   ```

   …and log back in. (`getent group input` should now show your
   user.)

2. **Start the daemon.** Run `hyprcorrect` in a terminal, or have
   your session manager autostart it
   (`hyprcorrect.desktop` ships with `X-GNOME-Autostart-enabled=true`).

3. **Press Super+Ctrl+Shift+Alt+F** in any window to fix the last
   word. The default chord is configurable in
   *Preferences → Hotkeys* (Right-click the tray icon → Open
   Preferences…).

## Usage

| Action | Default chord | What it does |
|---|---|---|
| **Fix last word** | `Super+Ctrl+Shift+Alt+F` | Backspaces the last word in the focused window's buffer, types the top spellbook suggestion. Falls back to the clipboard / selection path when the buffer is empty. |
| **Fix last sentence** | *(unbound)* | Sends the last sentence through the *Smart provider* (LLM, LanguageTool, or spellbook). Bind it in Preferences. |
| **Review correction** | *(unbound)* | Same as above, but shows a popup with the proposed correction first. Press Enter to apply or Esc to cancel. |

The trigger letter override `$HYPRCORRECT_CHORD="SUPER+CTRL+J"` is
honored for one-off dev runs without editing the config.

## Preferences

Run `hyprcorrect prefs` or click "Open Preferences…" in the tray.

- **Hotkeys** — record chord(s) for each action.
- **Providers** — pick the default and smart providers, configure
  the LLM (model + API key) and LanguageTool (URL).
- **Behavior** — `inter_key_delay_ms` slider (raise it if some app
  drops characters under wtype's default fast speed).
- **Privacy** — app blocklist; the dropdown lists running window
  classes from `hyprctl clients`.
- **About** — version + source + license links.

Save sends `SIGHUP` to the running daemon, which reloads everything
without a restart.

## Troubleshooting

**The chord doesn't do anything.** Check the daemon is running:
`pgrep -ax hyprcorrect`. Check the bind is installed:
`hyprctl binds | grep hyprcorrect`. If the chord echoes its raw
escape sequence (e.g. `^[[102;8u`) into a terminal, Hyprland isn't
intercepting it — make sure the daemon has been signaled to install
the bind (re-save from prefs, or restart).

**Capture sees no keys.** Verify you're in the `input` group:
`groups | grep input`. The session group list is set at login —
`sudo usermod -aG input "$USER"` requires logging out and back in.
The daemon prints a clear error if it can't read `/dev/input/event*`.

**Tray icon disappears when paused.** Fixed — the daemon stays
`Status::Active` and swaps the icon glyph instead of using the SNI
"this isn't important" hint that Waybar hides by default.

**The chord chip in Preferences can't capture Super combos.**
egui-winit on Linux/Wayland doesn't surface the Super key in its
`Modifiers` struct, so the in-app chord-capture chip will only
record `Ctrl` / `Shift` / `Alt`. The daemon temporarily releases
its bind during capture so you *can* re-record the existing
Super+Ctrl+Shift+Alt+F default — just press the same combo. For
fresh Super combos that aren't already bound, edit
`~/.config/hyprcorrect/config.toml` directly:

```toml
[hotkeys]
fix_word = "SUPER+R"
```

The daemon will re-bind on the next Save in Preferences (or a
SIGHUP).

## Architecture

The full design — per-OS interface contract, signal protocol, IPC
shape — is in [`DESIGN.md`](DESIGN.md). Briefly:

- A **4-crate workspace**: `hyprcorrect-core` (pure logic),
  `hyprcorrect-platform` (per-OS capture/emit/hotkey/tray/focus),
  `hyprcorrect-ui` (platform-independent egui),
  `hyprcorrect` (the binary).
- Linux M3+: `evdev` capture, `xkbcommon` translation, per-window
  buffers via Hyprland IPC, `wtype` emit, `hyprctl keyword bind`
  for the chord (signals `SIGUSR1` with a discriminator action
  file), `ksni` tray, `keyring` for secrets, egui prefs.
- macOS M2 is the contract on the Linux side: the platform
  modules mirror the same interface using `CGEventTap`,
  `RegisterEventHotKey`, and `NSStatusItem`.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.

The bundled `en_US` spelling dictionary is vendored from
[wooorm/dictionaries](https://github.com/wooorm/dictionaries) (derived
from SCOWL) under its own permissive license — see
[`crates/hyprcorrect-core/dictionaries/en_US/LICENSE`](crates/hyprcorrect-core/dictionaries/en_US/LICENSE).
