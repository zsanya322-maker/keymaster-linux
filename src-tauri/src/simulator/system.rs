//! Системные и оконные действия: снап окон, minimize/maximize/close,
//! запуск приложений, сон, выключение монитора.
//!
//! X11: полный набор через EWMH. KDE Wayland: оконные действия требуют
//! KWin-скриптинга (планируется); сон работает через systemctl на любой сессии.

use tracing::{info, warn};

use crate::platform;

pub fn execute_window_action(action: &str) {
    match platform::session_kind() {
        platform::SessionKind::Headless => {
            warn!("execute_window_action('{}'): нет графической сессии", action);
        }
        _ => {
            let had_x11 = platform::x11::cursor_position().is_some();
            if had_x11 {
                platform::x11::execute_window_action(action);
            } else {
                warn!(
                    "execute_window_action('{}'): на Wayland оконные действия появятся в следующих версиях",
                    action
                );
            }
        }
    }
}

/// Запустить приложение detached (отдельная сессия процесса).
pub fn launch_app(path: &str) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let result = Command::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    match result {
        Ok(child) => info!("launch_app: запущен '{}' (pid {})", path, child.id()),
        Err(e) => warn!("launch_app: не удалось запустить '{}': {}", path, e),
    }
}

/// Поднять окно указанного процесса/заголовка поверх всех окон.
///
/// Поиск по ИЛИ (достаточно одного совпадения):
/// - `process` — точное совпадение clean-имени (lowercase, без .exe)
/// - `title` — заголовок окна содержит строку (case-insensitive)
pub fn focus_process(process: Option<&str>, title: Option<&str>) {
    if platform::x11::cursor_position().is_some() {
        platform::x11::focus_process(process, title);
    } else {
        warn!(
            "focus_process: на Wayland фокусировка окон появится в следующих версиях (process={:?}, title={:?})",
            process, title
        );
    }
}

/// Перевести компьютер в спящий режим (systemctl suspend, работает и на Wayland).
pub fn sleep_pc() {
    use std::process::Command;
    match Command::new("systemctl").arg("suspend").status() {
        Ok(status) if status.success() => {
            info!("sleep_pc: suspend выполнен");
        }
        Ok(status) => warn!("sleep_pc: systemctl suspend завершился с {:?}", status.code()),
        Err(e) => warn!("sleep_pc: не удалось запустить systemctl suspend: {}", e),
    }
}

/// Выключить монитор: DPMS на X11, на Wayland — через org.freedesktop.ScreenSaver.
pub fn monitor_off() {
    if platform::x11::monitor_off() {
        return;
    }
    // Wayland (KDE/GNOME): тупим через ScreenSaver DBus — эмулируем «простой»
    match platform::kde::idle_suspend_screen() {
        Ok(()) => info!("monitor_off: ScreenSaver SetActive выполнен"),
        Err(e) => warn!("monitor_off: не удалось выключить монитор: {}", e),
    }
}
