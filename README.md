<!-- 🇬🇧 English | 🇷🇺 [Русский](README.ru.md) -->

<div align="center">

# ⌨️ KeyMaster Linux

**A keyboard & mouse automation utility for Linux — the Linux-only edition of KeyMaster Pro**

[![License: FCL](https://img.shields.io/badge/License-FCL-blue.svg)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/Platform-Linux%20%28Wayland%2FX11%29-1793D1.svg)](https://github.com/zsanya322-maker/keymaster-linux)
[![Built with Tauri](https://img.shields.io/badge/Built%20with-Tauri%20v2-FFC131.svg)](https://v2.tauri.app)
[![Rust](https://img.shields.io/badge/Rust-edition%202024-CE422B.svg)](https://www.rust-lang.org)
[![React](https://img.shields.io/badge/React-19-61DAFB.svg)](https://react.dev)

</div>

---

## 📝 Description

**KeyMaster Linux** is a desktop automation utility built around one idea: **everything is a rule**.

```
[ TRIGGER ]  +  [ CONDITIONS ]  →  [ ACTIONS ]
```

A key press, mouse click, typed abbreviation, or tap-hold fires a rule. Optional conditions (active window, active layer) decide whether it runs. Then one or more actions execute — remap a key, run a macro, snap a window, control media, and more.

Built with **Rust + Tauri v2 + React 19**. On Linux the daemon captures input directly through **evdev** and re-injects it through **uinput** (the same approach as kanata/ydotool) — this works on **both Wayland and X11**, with **zero input logging**.

> Forked from [KeyMaster Pro](https://github.com/zsanya322-maker/keymaster-pro) (Windows) at v0.5.1 and reworked for Linux.

---

## ✨ Features

The unified **Rule Builder** (trigger → condition → action) covers:

- 🎯 **Triggers**: key down/up (any key or combo), mouse buttons, tap-hold, typed text
- ⚡ **Actions**: remap keys/mouse, macros (recording + playback), layers (toggle/hold), text expansion, media & volume, window management, launch apps, system actions
- 🧩 **Conditions**: active layer, window match (process/title), virtual desktop
- 🔄 **Per-app profiles**, 🔥 QMK-style layers, 🚀 daemon + GUI two-process architecture
- 🌐 EN/RU UI, dark/light theme, system tray, autostart, per-session logs

### Linux backend (v0.5.1 port status)

| Area | Status |
|---|---|
| Input capture (evdev grab) | ✅ works on Wayland & X11 |
| Key/mouse remap, layers, tap-hold (uinput injection) | ✅ |
| IPC (Unix socket, JSON-RPC) | ✅ |
| KDE Wayland: active window via KWin DBus | ✅ |
| X11: window match, snap/min/max/close, focus, virtual desktops (EWMH) | ✅ |
| Window actions & focus on non-KDE Wayland | 🔄 needs compositor scripting (planned) |
| Typed text (TypeString) | ✅ US layout; non-ASCII planned |
| Absolute mouse position on Wayland | 🔄 partial (X11/XWayland only) |
| Distribution: PKGBUILD, AppImage/deb (CI) | ✅ |

---

## 📥 Install

### Arch Linux / EndeavourOS (PKGBUILD)

```bash
git clone https://github.com/zsanya322-maker/keymaster-linux.git
cd keymaster-linux
makepkg -si
```

The package installs the udev rule for `/dev/uinput`. Then add yourself to the `input` group and re-login:

```bash
sudo usermod -aG input $USER
```

> Without the `input` group the daemon cannot capture your keyboard.

### From source

Prerequisites: Node.js 18+, pnpm, Rust 1.85+, plus `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator`, `librsvg` (Arch: `sudo pacman -S webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg`).

```bash
pnpm install
pnpm tauri dev      # dev mode
pnpm tauri build    # production build
```

---

## 🛠️ How it works on Linux

```
┌──────────────┐  evdev grab   ┌───────────────────┐   uinput    ┌───────────────┐
│  Keyboard &  │ ────────────▶ │  KeyMaster daemon  │ ──────────▶ │  Compositor   │
│  Mouse (HW)  │               │  (rules engine)    │  (virtual   │  (KDE/GNOME/  │
└──────────────┘               └───────────────────┘   devices)  │  X11/Wayland) │
                                 └────────▲─────────┘             └───────────────┘
                                          │ JSON-RPC over Unix socket
                                 ┌────────┴─────────┐
                                 │   GUI (React)    │
                                 └──────────────────┘
```

- The daemon **exclusively grabs** physical input devices (`/dev/input/event*`), processes events through the rules engine, and forwards everything it doesn't handle via virtual uinput devices. No event loops back into the daemon.
- Active-window context comes from **KWin DBus** on KDE Wayland and **EWMH** on X11.
- GUI and daemon communicate over a per-user **Unix socket** (`$XDG_RUNTIME_DIR/keymaster-daemon.sock`).
- Data is stored under `$XDG_DATA_HOME/keymaster-linux/` (default `~/.local/share/keymaster-linux/`).

---

## 🗺️ Roadmap

- [x] Fork v0.5.1, Linux compile, evdev/uinput backend, UDS IPC, X11/KWin context, PKGBUILD + CI
- [ ] Window actions & focus on GNOME/wlroots Wayland (compositor-specific)
- [ ] Non-ASCII text typing (zwp_virtual_keyboard / keymap-aware injection)
- [ ] Hotplug of input devices, per-device remap profiles
- [ ] AUR package publication

See [ROADMAP.md](ROADMAP.md) for the original project roadmap.

---

## 📜 License

**Fair Core License (FCL)** — source-available, converts to MIT on January 1, 2030. See [LICENSE](LICENSE).

---

<div align="center">

<sub>Fork of KeyMaster Pro. Built with ❤️ using Rust, Tauri, and React — now for Linux.</sub>

</div>
