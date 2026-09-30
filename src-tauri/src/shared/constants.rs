/// Константы приложения
///
/// Путь к IPC-сокету, директории, версии и т.д.

/// Имя Unix-сокета для IPC GUI ↔ Daemon (в XDG_RUNTIME_DIR)
pub const IPC_SOCKET_NAME: &str = "keymaster-daemon.sock";

/// Имя приложения
pub const APP_NAME: &str = "KeyMaster Linux";

/// Версия схемы данных (для миграций)
pub const DATA_VERSION: u32 = 1;

/// Интервал проверки активного окна (Layer Watcher)
pub const WINDOW_POLL_INTERVAL_MS: u64 = 500;

/// Максимальное количество бэкапов профиля
pub const MAX_BACKUPS: usize = 5;

/// Максимальное время обработки события ввода (мс) — информационный лимит
pub const HOOK_CALLBACK_TIMEOUT_MS: u32 = 300;

/// Полный путь к Unix-сокету IPC.
/// XDG_RUNTIME_DIR (обычно /run/user/<uid>), fallback /tmp c uid.
pub fn ipc_socket_path() -> std::path::PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime_dir.is_empty() {
            return std::path::PathBuf::from(runtime_dir).join(IPC_SOCKET_NAME);
        }
    }
    let uid = current_uid();
    std::path::PathBuf::from(format!("/tmp/keymaster-daemon-{}.sock", uid))
}

/// UID текущего пользователя (без внешних зависимостей).
fn current_uid() -> u32 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    if let Ok(uid) = first.parse::<u32>() {
                        return uid;
                    }
                }
            }
        }
    }
    1000
}
