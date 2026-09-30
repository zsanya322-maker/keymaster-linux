<!-- 🇷🇺 Русский | 🇬🇧 [English](README.md) -->

<div align="center">

# ⌨️ KeyMaster Linux

**Автоматизация клавиатуры и мыши для Linux — Linux-версия KeyMaster Pro**

[![License: FCL](https://img.shields.io/badge/License-FCL-blue.svg)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/Platform-Linux%20%28Wayland%2FX11%29-1793D1.svg)](https://github.com/zsanya322-maker/keymaster-linux)
[![Built with Tauri](https://img.shields.io/badge/Built%20with-Tauri%20v2-FFC131.svg)](https://v2.tauri.app)
[![Rust](https://img.shields.io/badge/Rust-edition%202024-CE422B.svg)](https://www.rust-lang.org)
[![React](https://img.shields.io/badge/React-19-61DAFB.svg)](https://react.dev)

</div>

---

## 📝 Описание

**KeyMaster Linux** — утилита автоматизации, построенная вокруг одной идеи: **всё — это правило**.

```
[ ТРИГГЕР ]  +  [ УСЛОВИЯ ]  →  [ ДЕЙСТВИЯ ]
```

Нажатие клавиши, кнопка мыши, текстовая аббревиатура или tap-hold запускают правило. Необязательные условия (активное окно, активный слой) решают, сработает ли оно. Затем выполняется одно или несколько действий — ремап клавиши, макрос, снап окна, управление медиа и другое.

Собрано на **Rust + Tauri v2 + React 19**. На Linux daemon захватывает ввод напрямую через **evdev** и воспроизводит через **uinput** (как kanata/ydotool) — работает и на **Wayland**, и на **X11**, с **нулём логирования нажатий**.

> Форк [KeyMaster Pro](https://github.com/zsanya322-maker/keymaster-pro) (Windows) с версии v0.5.1, переработан под Linux.

---

## ✨ Возможности

Единый **Rule Builder** (триггер → условие → действие):

- 🎯 **Триггеры**: нажатие/отпускание клавиш (любые комбинации), кнопки мыши, tap-hold, набранный текст
- ⚡ **Действия**: ремап клавиш/мыши, макросы (запись + воспроизведение), слои (toggle/hold), текст-экспандер, медиа и громкость, управление окнами, запуск приложений, системные действия
- 🧩 **Условия**: активный слой, совпадение окна (процесс/заголовок), виртуальный стол
- 🔄 **Профили по приложениям**, 🔥 слои в стиле QMK, 🚀 двухпроцессная архитектура (daemon + GUI)
- 🌐 интерфейс EN/RU, тёмная/светлая тема, системный трей, автозапуск, логи по сессиям

### Linux-бэкенд (статус порта v0.5.1)

| Область | Статус |
|---|---|
| Захват ввода (evdev grab) | ✅ Wayland и X11 |
| Ремап клавиш/мыши, слои, tap-hold (инъекция uinput) | ✅ |
| IPC (Unix-сокет, JSON-RPC) | ✅ |
| KDE Wayland: активное окно через KWin DBus | ✅ |
| X11: совпадение окон, снап/min/max/close, фокус, виртуальные столы (EWMH) | ✅ |
| Оконные действия и фокус на Wayland кроме KDE | 🔄 нужен скриптинг композитора (в планах) |
| Набор текста (TypeString) | ✅ US-раскладка; не-ASCII в планах |
| Абсолютная позиция мыши на Wayland | 🔄 частично (только X11/XWayland) |
| Дистрибуция: PKGBUILD, AppImage/deb (CI) | ✅ |

---

## 📥 Установка

### Arch Linux / EndeavourOS (PKGBUILD)

```bash
git clone https://github.com/zsanya322-maker/keymaster-linux.git
cd keymaster-linux
makepkg -si
```

Пакет ставит udev-правило для `/dev/uinput`. Затем добавьте себя в группу `input` и перелогиньтесь:

```bash
sudo usermod -aG input $USER
```

> Без группы `input` daemon не сможет захватить клавиатуру.

### Из исходников

Нужно: Node.js 18+, pnpm, Rust 1.85+, а также `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator`, `librsvg` (Arch: `sudo pacman -S webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg`).

```bash
pnpm install
pnpm tauri dev      # режим разработки
pnpm tauri build    # production-сборка
```

---

## 🛠️ Как это работает на Linux

```
┌──────────────┐  evdev grab   ┌───────────────────┐   uinput    ┌───────────────┐
│  Клавиатура  │ ────────────▶ │  KeyMaster daemon  │ ──────────▶ │  Композитор   │
│  и мышь (HW) │               │  (движок правил)   │  (виртуальные│  (KDE/GNOME/  │
└──────────────┘               └───────────────────┘   устройства)│  X11/Wayland) │
                                 └────────▲─────────┘             └───────────────┘
                                          │ JSON-RPC через Unix-сокет
                                 ┌────────┴─────────┐
                                 │   GUI (React)    │
                                 └──────────────────┘
```

- Daemon **эксклюзивно захватывает** физические устройства ввода (`/dev/input/event*`), прогоняет события через движок правил и пересылает необработанное через виртуальные uinput-устройства. Зацикливания нет — daemon не читает свои же виртуальные ноды.
- Контекст активного окна: **KWin DBus** на KDE Wayland и **EWMH** на X11.
- GUI и daemon общаются через пользовательский **Unix-сокет** (`$XDG_RUNTIME_DIR/keymaster-daemon.sock`).
- Данные хранятся в `$XDG_DATA_HOME/keymaster-linux/` (по умолчанию `~/.local/share/keymaster-linux/`).

---

## 🗺️ Дорожная карта

- [x] Форк v0.5.1, сборка на Linux, evdev/uinput-бэкенд, UDS IPC, X11/KWin-контекст, PKGBUILD + CI
- [ ] Оконные действия и фокус на GNOME/wlroots Wayland (зависит от композитора)
- [ ] Набор не-ASCII текста (zwp_virtual_keyboard / keymap-aware инъекция)
- [ ] Hotplug устройств ввода, профили по устройствам
- [ ] Публикация пакета в AUR

Историческая дорожная карта оригинала — в [ROADMAP.md](ROADMAP.md).

---

## 📜 Лицензия

**Fair Core License (FCL)** — открытый исходный код, автоматический переход в MIT 1 января 2030. См. [LICENSE](LICENSE).

---

<div align="center">

<sub>Форк KeyMaster Pro. Rust, Tauri и React с ❤️ — теперь и на Linux.</sub>

</div>
