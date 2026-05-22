# hyprcorrect

Keyboard-driven spelling and typo correction for the whole desktop. Press
a hotkey and the word — or sentence — you just typed is checked and fixed
in place, in whatever application has focus, terminals included.

> **Status: early development.** This repository is currently the M0
> scaffold — the Cargo workspace and CLI surface exist, but the
> correction engine, background daemon, and GUI are not implemented yet.
> See [`DESIGN.md`](DESIGN.md) for the architecture and milestone plan.

## Platforms

- **Linux / Wayland** — Hyprland is the primary target.
- **macOS** — built alongside Linux from the start.
- **Windows** — scaffolded as a stub; not a shipping target yet.

## Build

```sh
cargo build
```

The toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml).

## Workspace

| Crate | Role |
| --- | --- |
| `hyprcorrect-core` | Config, keystroke buffer, correction providers |
| `hyprcorrect-platform` | Per-OS input capture, synthetic input, hotkeys |
| `hyprcorrect-ui` | egui preferences window and suggestion popup |
| `hyprcorrect` | The binary / background daemon |

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.

The bundled `en_US` spelling dictionary is vendored from
[wooorm/dictionaries](https://github.com/wooorm/dictionaries) (derived
from SCOWL) under its own permissive license — see
[`crates/hyprcorrect-core/dictionaries/en_US/LICENSE`](crates/hyprcorrect-core/dictionaries/en_US/LICENSE).
