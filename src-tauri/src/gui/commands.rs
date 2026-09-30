/// Tauri commands (invoke handlers)
///
/// GUI вызывает эти функции через tauri.invoke().
/// Большинство runtime-команд перенаправляются в Daemon через Unix-сокет IPC,
/// а GUI-конфигурация читается/пишется напрямую, чтобы настройки сохранялись
/// даже при остановленном демоне.
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::State;
use tracing::{info, warn};

/// Состояние GUI процесса
pub struct GuiState {
    /// Путь к текущему исполняемому файлу (для spawn daemon)
    exe_path: Option<String>,
    /// Флаг, указывающий, что запуск daemon-процесса уже выполняется
    spawning: AtomicBool,
}

impl Default for GuiState {
    fn default() -> Self {
        let exe_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        Self {
            exe_path,
            spawning: AtomicBool::new(false),
        }
    }
}

/// Запустить daemon-процесс из GUI.
#[tauri::command]
pub fn spawn_daemon(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    let exe_path = state
        .exe_path
        .as_ref()
        .ok_or("Не удалось определить путь к исполняемому файлу")?;

    if crate::daemon::runner::is_daemon_running() {
        state.spawning.store(false, Ordering::SeqCst);
        info!("Daemon уже запущен");
        return Ok(serde_json::json!({
            "success": true,
            "message": "Daemon already running"
        }));
    }

    if state.spawning.swap(true, Ordering::SeqCst) {
        info!("Запуск daemon уже выполняется, игнорируем дублирующий вызов");
        return Ok(serde_json::json!({
            "success": true,
            "message": "Daemon spawn already in progress"
        }));
    }

    let current_pid = std::process::id();
    let socket_path = crate::shared::constants::ipc_socket_path();
    info!(
        "Запуск daemon-процесса: {} --daemon --parent-pid {}",
        exe_path, current_pid
    );
    info!("Ожидаемый IPC сокет: {}", socket_path.display());

    use std::os::unix::process::CommandExt;
    let child = match std::process::Command::new(exe_path)
        .arg("--daemon")
        .arg("--parent-pid")
        .arg(current_pid.to_string())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            state.spawning.store(false, Ordering::SeqCst);
            return Err(format!("Не удалось запустить daemon: {}", e));
        }
    };

    let pid = child.id();
    info!("Daemon запущен с PID: {}", pid);

    Ok(serde_json::json!({
        "success": true,
        "pid": pid,
        "message": "Daemon started"
    }))
}

/// Дождаться появления Named Pipe, если daemon уже был spawn'нут, но ещё
/// находится в коротком startup-окне до запуска IPC server.
async fn wait_for_spawning_daemon(state: &GuiState) {
    if crate::daemon::runner::is_daemon_running() || !state.spawning.load(Ordering::SeqCst) {
        return;
    }

    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if crate::daemon::runner::is_daemon_running() {
            return;
        }
        if !state.spawning.load(Ordering::SeqCst) {
            return;
        }
    }
}

/// Реальная остановка daemon. Учитывает гонку spawn -> pipe и возвращает успех
/// только после исчезновения Named Pipe.
async fn stop_daemon_impl(state: &GuiState) -> Result<serde_json::Value, String> {
    wait_for_spawning_daemon(state).await;

    if !crate::daemon::runner::is_daemon_running() {
        state.spawning.store(false, Ordering::SeqCst);
        return Ok(serde_json::json!({
            "success": true,
            "message": "Daemon already stopped"
        }));
    }

    // После появления pipe daemon уже вышел из spawn-фазы. Новый spawn разрешим
    // только когда текущий процесс действительно исчезнет.
    state.spawning.store(false, Ordering::SeqCst);
    info!("stop_daemon: отправка shutdown через IPC");

    let shutdown_error = crate::daemon::ipc_client::call("shutdown", None)
        .await
        .err();
    if let Some(ref error) = shutdown_error {
        // Даже при ошибке чтения ответа daemon мог успеть принять shutdown.
        // Проверяем фактическое состояние pipe, прежде чем объявлять failure.
        warn!("stop_daemon: IPC shutdown вернул ошибку: {}", error);
    }

    for _ in 0..30 {
        if !crate::daemon::runner::is_daemon_running() {
            info!("stop_daemon: daemon полностью остановлен");
            return Ok(serde_json::json!({
                "success": true,
                "message": "Daemon stopped"
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let message = shutdown_error
        .map(|error| format!("Daemon shutdown failed after IPC error: {}", error))
        .unwrap_or_else(|| "Daemon shutdown timeout".to_string());
    warn!("stop_daemon: {}", message);
    Ok(serde_json::json!({
        "success": false,
        "message": message
    }))
}

/// Остановить daemon-процесс через IPC.
#[tauri::command]
pub async fn stop_daemon(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    stop_daemon_impl(&state).await
}

async fn stop_daemon_before_process_transition(
    reason: &str,
    state: &GuiState,
) -> Result<(), String> {
    let result = stop_daemon_impl(state).await?;
    if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let message = result
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("daemon did not stop");
        warn!("{}: daemon не подтвердил остановку: {}", reason, message);
        return Err(format!(
            "Не удалось остановить daemon перед {}: {}",
            reason, message
        ));
    }
    Ok(())
}

/// Получить статус Daemon через IPC.
/// Любой завершившийся status-check снимает spawn-guard: если daemon уже
/// отвечает — запуск завершён успешно; если не отвечает — следующий retry
/// имеет право попробовать запустить процесс ещё раз.
#[tauri::command]
pub async fn daemon_status(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    match crate::daemon::ipc_client::call("get_status", None).await {
        Ok(status) => {
            state.spawning.store(false, Ordering::SeqCst);
            Ok(serde_json::json!({
                "connected": true,
                "status": "running",
                "details": status
            }))
        }
        Err(_) => {
            state.spawning.store(false, Ordering::SeqCst);
            Ok(serde_json::json!({
                "connected": false,
                "status": "stopped"
            }))
        }
    }
}

/// IPC-прокси: отправить произвольный JSON-RPC запрос в Daemon.
#[tauri::command]
pub async fn ipc_call(
    method: String,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    crate::daemon::ipc_client::call(&method, params).await
}

/// Тестовая команда.
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Привет, {}! KeyMaster Pro работает 🎉", name)
}

/// Полностью завершить GUI. Quit остаётся best-effort: если daemon завис,
/// пользователь всё равно должен иметь возможность закрыть приложение, а
/// parent-PID watchdog останется аварийной страховкой.
#[tauri::command]
pub async fn quit_app(
    app_handle: tauri::AppHandle,
    state: State<'_, GuiState>,
) -> Result<(), String> {
    info!("quit_app: explicit application exit");
    if let Err(error) = stop_daemon_before_process_transition("quit_app", &state).await {
        warn!("quit_app: продолжаем выход после ошибки daemon: {}", error);
    }
    app_handle.exit(0);
    Ok(())
}

/// Перезапустить приложение (используется после обновления).
/// В отличие от обычного Quit, restart запрещён, если старый daemon не удалось
/// полностью остановить: новый GUI не должен подключаться к daemon старого PID.
#[tauri::command]
pub async fn restart_app(
    app_handle: tauri::AppHandle,
    state: State<'_, GuiState>,
) -> Result<(), String> {
    info!("restart_app: перезапуск приложения");
    stop_daemon_before_process_transition("restart_app", &state).await?;
    app_handle.restart();
}

/// Перезапуск от имени администратора на Linux не требуется (нет UAC):
/// команда сохранена для совместимости с frontend и сообщает об отказе.
#[tauri::command]
pub async fn restart_as_admin(_state: State<'_, GuiState>) -> Result<(), String> {
    Err("Unsupported on this OS".to_string())
}

/// Проверить, запущено ли приложение с повышенными привилегиями (root).
#[tauri::command]
pub fn is_elevated() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Uid:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|uid| uid.parse::<u32>().ok())
            })
        })
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

/// Загрузить конфигурацию приложения напрямую из config.json.
#[tauri::command]
pub fn get_gui_config() -> Result<serde_json::Value, String> {
    let config = crate::shared::config::load_config()?;
    serde_json::to_value(config).map_err(|e| format!("Ошибка сериализации config: {}", e))
}

/// Частично обновить GUI-конфигурацию напрямую в config.json.
///
/// Patch сначала сливается с текущей конфигурацией, затем весь результат
/// десериализуется обратно в AppConfig. Поэтому неизвестные типы/некорректные
/// значения не могут тихо записать повреждённый config.json.
#[tauri::command]
pub fn update_gui_config(patch: serde_json::Value) -> Result<serde_json::Value, String> {
    let current = crate::shared::config::load_config()?;
    let mut merged = serde_json::to_value(current)
        .map_err(|e| format!("Ошибка сериализации текущего config: {}", e))?;

    let patch_object = patch
        .as_object()
        .ok_or_else(|| "Patch конфигурации должен быть JSON-объектом".to_string())?;
    let merged_object = merged
        .as_object_mut()
        .ok_or_else(|| "Текущая конфигурация не является JSON-объектом".to_string())?;

    for (key, value) in patch_object {
        if !merged_object.contains_key(key) {
            return Err(format!("Неизвестное поле конфигурации: {}", key));
        }
        merged_object.insert(key.clone(), value.clone());
    }

    let validated: crate::shared::types::AppConfig = serde_json::from_value(merged)
        .map_err(|e| format!("Некорректное значение конфигурации: {}", e))?;
    crate::shared::config::save_config(&validated)?;

    serde_json::to_value(validated)
        .map_err(|e| format!("Ошибка сериализации сохранённого config: {}", e))
}

/// Хранилище AI-ключей: JSON-файл 0600 в конфигурационном каталоге
/// ($XDG_DATA_HOME/keymaster-linux/ai-secrets.json).
fn ai_secrets_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::shared::persistence::app_data_dir()?.join("ai-secrets.json"))
}

fn read_ai_secrets() -> serde_json::Map<String, serde_json::Value> {
    let Ok(path) = ai_secrets_path() else {
        return serde_json::Map::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_ai_secrets(secrets: serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = ai_secrets_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Не удалось создать {}: {}", parent.display(), e))?;
    }
    let data = serde_json::to_string_pretty(&secrets)
        .map_err(|e| format!("Ошибка сериализации секретов: {}", e))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("Не удалось открыть {}: {}", path.display(), e))?;
    file.write_all(data.as_bytes())
        .map_err(|e| format!("Не удалось записать секреты: {}", e))?;
    let mut permissions = std::fs::metadata(&path)
        .map_err(|e| e.to_string())?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&path, permissions).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn ai_secret_set(provider_id: String, api_key: String) -> Result<(), String> {
    if provider_id.trim().is_empty() {
        return Err("AI provider id is empty".to_string());
    }
    if api_key.len() > 5120 {
        return Err("AI API key is too large".to_string());
    }

    let mut secrets = read_ai_secrets();
    secrets.insert(provider_id, serde_json::Value::String(api_key));
    write_ai_secrets(secrets)
}

#[tauri::command]
pub fn ai_secret_get(provider_id: String) -> Result<Option<String>, String> {
    let secrets = read_ai_secrets();
    Ok(secrets
        .get(&provider_id)
        .and_then(|value| value.as_str())
        .map(|s| s.to_string()))
}

#[tauri::command]
pub fn ai_secret_delete(provider_id: String) -> Result<(), String> {
    let mut secrets = read_ai_secrets();
    secrets.remove(&provider_id);
    write_ai_secrets(secrets)
}
