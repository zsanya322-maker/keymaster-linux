//! KDE Wayland: активное окно через KWin DBus (queryWindowInfo).
//!
//! На Wayland обычное приложение не имеет доступа к чужим окнам, поэтому
//! используется официальный DBus-интерфейс KWin — тот же, что у kdotool.
//! Работает на Plasma 5.24+ и Plasma 6.

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

/// Информация об активном окне из KWin.
pub struct ActiveWindow {
    pub process: String,
    pub process_path: String,
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub fullscreen: bool,
    pub virtual_desktop: String,
}

fn query_window_info() -> Option<HashMap<String, OwnedValue>> {
    let conn = Connection::session().ok()?;
    let reply = conn
        .call_method(
            Some("org.kde.KWin"),
            "/KWin",
            Some("org.kde.KWin"),
            "queryWindowInfo",
            &(),
        )
        .ok()?;
    reply.body().deserialize::<HashMap<String, OwnedValue>>().ok()
}

fn value_to_string(value: &OwnedValue) -> Option<String> {
    value.downcast_ref::<String>().ok()
}

fn value_to_i32(value: &OwnedValue) -> Option<i32> {
    value.downcast_ref::<i32>().ok()
}

fn value_to_bool(value: &OwnedValue) -> Option<bool> {
    value.downcast_ref::<bool>().ok()
}

/// Активное окно Plasma/Wayland. None — если DBus/KWin недоступны.
pub fn active_window() -> Option<ActiveWindow> {
    let info = query_window_info()?;

    let pid = info.get("pid").and_then(value_to_i32).unwrap_or(0);
    let process = info
        .get("resourceClass")
        .and_then(value_to_string)
        .or_else(|| info.get("windowResourceClass").and_then(value_to_string))
        .or_else(|| info.get("windowClassName").and_then(value_to_string))
        .unwrap_or_default();
    let title = info
        .get("caption")
        .and_then(value_to_string)
        .unwrap_or_default();

    let (width, height) = info
        .get("geometry")
        .and_then(|value| {
            value
                .downcast_ref::<(i32, i32, i32, i32)>()
                .ok()
                .map(|(_x, _y, w, h)| (w, h))
        })
        .unwrap_or((0, 0));

    let process_path = if pid > 0 {
        std::fs::read_link(format!("/proc/{}/exe", pid))
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let fullscreen = info.get("fullscreen").and_then(value_to_bool).unwrap_or(false);

    Some(ActiveWindow {
        process: crate::shared::clean_process_name(&process),
        process_path,
        title,
        width,
        height,
        fullscreen,
        virtual_desktop: current_virtual_desktop(),
    })
}

/// Текущий виртуальный стол Plasma (идентификатор как строка).
pub fn current_virtual_desktop() -> String {
    let conn = match Connection::session() {
        Ok(conn) => conn,
        Err(_) => return String::new(),
    };
    let reply = conn.call_method(
        Some("org.kde.KWin"),
        "/VirtualDesktopManager",
        Some("org.kde.KWin.VirtualDesktopManager"),
        "current",
        &(),
    );
    match reply {
        Ok(reply) => reply
            .body()
            .deserialize::<u32>()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Доступен ли KWin DBus (для выбора бэкенда в context tracker).
pub fn kwin_available() -> bool {
    Connection::session()
        .ok()
        .and_then(|conn| {
            conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &("org.kde.KWin",),
            )
            .ok()
        })
        .and_then(|reply| reply.body().deserialize::<bool>().ok())
        .unwrap_or(false)
}

/// Погасить экран через org.freedesktop.ScreenSaver (Wayland).
pub fn idle_suspend_screen() -> Result<(), String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    conn.call_method(
        Some("org.freedesktop.ScreenSaver"),
        "/ScreenSaver",
        Some("org.freedesktop.ScreenSaver"),
        "SetActive",
        &(true,),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}
