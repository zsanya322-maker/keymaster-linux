//! Платформенный слой Linux: uinput, X11, KDE Wayland (DBus).
//!
//! Здесь собрано всё, что в Windows-версии было на Win32: инъекция ввода,
//! запрос позиции курсора, информация об активном окне, оконные действия.

pub mod kde;
pub mod uinput;
pub mod x11;

/// Тип графической сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    X11,
    Wayland,
    Headless,
}

pub fn session_kind() -> SessionKind {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        SessionKind::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        SessionKind::X11
    } else {
        SessionKind::Headless
    }
}
