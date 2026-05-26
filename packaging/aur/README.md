# hyprcorrect AUR packaging

Three Arch User Repository (AUR) PKGBUILDs live in this directory:

| Directory | Package | Built from | Use it when… |
|---|---|---|---|
| `aur/` | `hyprcorrect` | latest tagged source tarball (`v$pkgver`) | you want the released version + every commit since the last tag → no; pin via tags. |
| `aur-bin/` | `hyprcorrect-bin` | prebuilt binary from the GitHub Release | you don't want to wait for a full Rust build. |
| `aur-git/` | `hyprcorrect-git` | the current `main` HEAD | you want to track development. |

All three install the same payload — `/usr/bin/hyprcorrect`, the
`.desktop` entry, and the dual MIT / Apache-2.0 licenses — and
declare `provides`/`conflicts` so installing two of them at once
fails cleanly.

## Bumping the version

The `pkgver` and `sha256sums` lines in `aur/` and `aur-bin/` are
updated automatically by the GitHub release workflow when
`release-plz` cuts a new tag — don't edit them by hand. `aur-git/`'s
`pkgver()` function is dynamic and needs no maintenance.

## Hard vs optional deps

- **Hard**: `wtype` (the only emit path), `hyprland` (the daemon
  registers its chord via `hyprctl bind`), `libxkbcommon` (capture
  keysym translation), `libsecret` (LLM API key storage), plus the
  graphics stack (`wayland`, `libglvnd`, `fontconfig`, `freetype2`)
  the egui prefs window needs.
- **Optional**: `wl-clipboard` — only the clipboard / selection
  fallback path uses it, and the daemon degrades gracefully when
  it's missing.
