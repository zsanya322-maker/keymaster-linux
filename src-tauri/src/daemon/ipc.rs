/// IPC Unix Socket Server + JSON-RPC 2.0 Router
///
/// Listens to `$XDG_RUNTIME_DIR/keymaster-daemon.sock`, accepts JSON-RPC 2.0
/// requests from GUI and routes them to handlers.
///
/// Protocol: Newline-delimited JSON (one JSON line + \n per message).
use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

use crate::daemon::state::DaemonStateRef;
use crate::shared::constants;

use super::ipc_types::*;

// Profile mutations arrive over independent socket connections and may be
// processed concurrently. Serialize only mutation requests so read/status IPC
// remains concurrent while read-modify-write operations are atomic relative to
// every existing GUI/profile mutation.
static PROFILE_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn serializes_profile_mutation(method: &str) -> bool {
    method.starts_with("automation.")
        || (method.starts_with("profile.")
            && !matches!(
                method,
                "profile.list" | "profile.runtime_status" | "profile.backups"
            ))
        || matches!(method, "set_active_profile" | "apply_onboarding_example")
}

/// Захватить первый (эксклюзивный) сокет IPC.
///
/// Вызывается runner'ом синхронно внутри Tokio runtime ДО запуска simulator,
/// context tracker и input capture. Поэтому второй daemon не успевает даже
/// временно запустить второй hook engine. Остаток сокета от упавшего daemon
/// удаляется, если за ним не живёт активный процесс.
pub fn reserve_first_socket_instance() -> Result<UnixListener, String> {
    let socket_path = constants::ipc_socket_path();

    if socket_path.exists() {
        match std::os::unix::net::UnixStream::connect(&socket_path) {
            Ok(_) => {
                return Err(format!(
                    "Сокет '{}' уже обслуживается другим daemon.",
                    socket_path.display()
                ));
            }
            Err(_) => {
                // Остаток после падения — удаляем и биндимся заново
                let _ = std::fs::remove_file(&socket_path);
            }
        }
    }

    let parent_dir = socket_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let _ = std::fs::create_dir_all(&parent_dir);

    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| format!("Не удалось занять сокет '{}': {}. Вероятно, другой daemon уже запущен.", socket_path.display(), e))?;

    // Сокет только для текущего пользователя: команды демона не для чужих процессов
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(&socket_path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(&socket_path, permissions);
    }

    info!("IPC: first socket instance acquired exclusively ({})", socket_path.display());
    Ok(listener)
}

/// Start the IPC server using an already-reserved socket listener.
pub async fn start_ipc_server(state: DaemonStateRef, listener: UnixListener) -> Result<(), String> {
    let socket_path = constants::ipc_socket_path();
    info!("IPC сервер запускается на {}", socket_path.display());

    loop {
        info!("IPC: waiting for client connection...");
        match listener.accept().await {
            Ok((stream, _addr)) => {
                info!("IPC: client connected");
                let state_for_client = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state_for_client).await {
                        warn!("IPC: client handling error: {}", e);
                    }
                });
            }
            Err(e) => {
                warn!("Connection error: {}. Retrying in 50ms...", e);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

/// Handle client connection.
///
/// Reads JSON-RPC requests line by line and sends responses.
async fn handle_client(stream: UnixStream, state: DaemonStateRef) -> Result<(), String> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("Read error: {}", e))?
    {
        if line.is_empty() {
            continue;
        }

        let parsed = serde_json::from_str::<JsonRpcRequest>(&line);
        match parsed {
            Ok(req) => {
                if req.method == "subscribe_events" {
                    let id = req.id.unwrap_or(serde_json::Value::Null);
                    let response =
                        JsonRpcResponse::success(serde_json::json!({ "subscribed": true }), id);
                    let mut response_bytes = serde_json::to_string(&response)
                        .map_err(|e| format!("Serialization error: {}", e))?;
                    response_bytes.push('\n');
                    writer
                        .write_all(response_bytes.as_bytes())
                        .await
                        .map_err(|e| format!("Send error: {}", e))?;
                    writer
                        .flush()
                        .await
                        .map_err(|e| format!("Flush error: {}", e))?;

                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    {
                        let mut listeners = crate::gui::events::EVENT_LISTENERS
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        listeners.push(tx);
                    }

                    while let Some(event_msg) = rx.recv().await {
                        if let Err(e) = writer.write_all(event_msg.as_bytes()).await {
                            warn!("IPC Event write error: {}", e);
                            break;
                        }
                        if let Err(e) = writer.flush().await {
                            warn!("IPC Event flush error: {}", e);
                            break;
                        }
                    }
                    return Ok(());
                } else {
                    // Shutdown acknowledges and flushes the JSON-RPC response
                    // before the daemon stops, so shutdown never looks like a
                    // broken pipe to the GUI.
                    let shutdown_after_response = req.method == "shutdown";
                    let mutation_guard = if serializes_profile_mutation(&req.method) {
                        Some(PROFILE_MUTATION_LOCK.lock().await)
                    } else {
                        None
                    };
                    let response = route_request(req, &state).await;
                    drop(mutation_guard);
                    let mut response_bytes = serde_json::to_string(&response)
                        .map_err(|e| format!("Serialization error: {}", e))?;
                    response_bytes.push('\n');

                    let send_result: Result<(), String> = async {
                        writer
                            .write_all(response_bytes.as_bytes())
                            .await
                            .map_err(|e| format!("Send error: {}", e))?;
                        writer
                            .flush()
                            .await
                            .map_err(|e| format!("Flush error: {}", e))?;
                        Ok(())
                    }
                    .await;

                    if shutdown_after_response {
                        crate::daemon::runner::request_shutdown();
                    }

                    send_result?;
                    if shutdown_after_response {
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                warn!("IPC: invalid JSON-RPC: {}", e);
                let response = JsonRpcResponse::error(
                    PARSE_ERROR,
                    format!("Parse error: {}", e),
                    serde_json::Value::Null,
                );
                let mut response_bytes = serde_json::to_string(&response)
                    .map_err(|e| format!("Serialization error: {}", e))?;
                response_bytes.push('\n');
                writer
                    .write_all(response_bytes.as_bytes())
                    .await
                    .map_err(|e| format!("Send error: {}", e))?;
                writer
                    .flush()
                    .await
                    .map_err(|e| format!("Flush error: {}", e))?;
            }
        }
    }

    Ok(())
}

/// Route JSON-RPC request to handler
async fn route_request(req: JsonRpcRequest, state: &DaemonStateRef) -> JsonRpcResponse {
    let id = req.id.unwrap_or(serde_json::Value::Null);

    info!("IPC → {} (id={})", req.method, id);

    let result = match req.method.as_str() {
        // === System ===
        "ping" => Ok(serde_json::json!({ "pong": true })),
        "get_status" => handle_get_status(state).await,
        "shutdown" => handle_shutdown(state).await,

        // === Legacy profile aliases ===
        "get_active_profile" => handle_get_active_profile(state).await,
        "set_active_profile" => handle_set_active_profile(state, req.params).await,
        "list_profiles" => handle_list_profiles().await,

        // === Legacy config aliases ===
        "get_config" => handle_get_config(state).await,
        "update_config" => handle_update_config(state, req.params).await,

        // === Automation write/validation boundary ===
        method if method.starts_with("automation.") => {
            match crate::daemon::automation::dispatch(method, req.params, state).await {
                Ok(value) => Ok(value),
                Err(message) => {
                    warn!("Automation IPC error for {}: {}", method, message);
                    Err(JsonRpcError {
                        code: INTERNAL_ERROR,
                        message,
                        data: None,
                    })
                }
            }
        }

        // === Canonical router ===
        _ => match crate::daemon::router::dispatch(req.method.as_str(), req.params, state).await {
            Ok(val) => Ok(val),
            Err(err) => {
                warn!("IPC Router error for {}: {}", req.method, err);
                Err(JsonRpcError {
                    code: INTERNAL_ERROR,
                    message: err,
                    data: None,
                })
            }
        },
    };

    match result {
        Ok(value) => JsonRpcResponse::success(value, id),
        Err(err) => JsonRpcResponse::error(err.code, err.message, id),
    }
}

/// CPU daemon-процесса из /proc/self/stat (дельта к общему времени ядра).
fn get_current_cpu_usage_percent(state: &DaemonStateRef) -> f64 {
    // (utime+stime) в тиках + общее время системы в тиках
    fn proc_ticks() -> Option<(u64, u64)> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after_paren = stat.rsplit(')').next()?;
        let fields: Vec<&str> = after_paren.split_whitespace().collect();
        // поля после comm: state(1) ... utime=12, stime=13 (индексы с 0: 11, 12)
        let utime = fields.get(11)?.parse::<u64>().ok()?;
        let stime = fields.get(12)?.parse::<u64>().ok()?;
        Some((utime, stime))
    }

    fn sys_ticks() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let cpu_line = stat.lines().next()?;
        let fields: Vec<&str> = cpu_line.split_whitespace().collect();
        let sum: u64 = fields
            .iter()
            .skip(1)
            .filter_map(|f| f.parse::<u64>().ok())
            .sum();
        Some(sum)
    }

    let Some((utime, stime)) = proc_ticks() else {
        return 0.0;
    };
    let Some(sys) = sys_ticks() else {
        return 0.0;
    };
    let proc_t = utime + stime;

    let now = std::time::Instant::now();
    if let Ok(s) = state.read() {
        if let Ok(mut last_lock) = s.cpu_tracking.lock() {
            if let Some((last_proc, last_sys, last_time)) = *last_lock {
                let proc_diff = proc_t.saturating_sub(last_proc);
                let sys_diff = sys.saturating_sub(last_sys);
                let time_diff = now.duration_since(last_time).as_secs_f64();

                *last_lock = Some((proc_t, sys, now));

                if sys_diff > 0 && time_diff > 0.0 {
                    let usage = (proc_diff as f64 / sys_diff as f64) * 100.0;
                    return (usage * 100.0).round() / 100.0;
                }
            } else {
                *last_lock = Some((proc_t, sys, now));
            }
        }
    }
    0.0
}

/// RAM daemon-процесса из /proc/self/status (VmRSS).
fn get_current_ram_usage_mb() -> f64 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: f64 = rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0.0);
                return ((kb / 1024.0) * 10.0).round() / 10.0;
            }
        }
    }
    0.0
}

// === Command Handlers ===

/// Get current daemon status
async fn handle_get_status(state: &DaemonStateRef) -> Result<serde_json::Value, JsonRpcError> {
    // CPU tracking acquires state internally. Compute it before taking the
    // snapshot lock below so get_status never recursively acquires the same
    // RwLock on one request path.
    let cpu_usage = get_current_cpu_usage_percent(state);
    let ram_usage = get_current_ram_usage_mb();

    let s = state.read().map_err(|_| JsonRpcError {
        code: INTERNAL_ERROR,
        message: "Failed to read state".into(),
        data: None,
    })?;

    Ok(serde_json::json!({
        "running": s.running,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "hooks_installed": s.hooks_installed,
        "kb_hook_enabled": s.kb_hook_enabled,
        "mouse_hook_enabled": s.mouse_hook_enabled,
        "active_profile_id": s.active_profile_id,
        "active_layers": s.active_layers,
        "cpu_usage": cpu_usage,
        "memory_usage_mb": ram_usage,
        "keystrokes_processed": s.keystrokes_processed.load(std::sync::atomic::Ordering::Relaxed),
        "last_latency_us": s.last_latency_us.load(std::sync::atomic::Ordering::Relaxed),
    }))
}

/// Mark daemon shutdown as requested. The actual shutdown is performed by
/// handle_client only after the JSON-RPC response has been flushed.
async fn handle_shutdown(state: &DaemonStateRef) -> Result<serde_json::Value, JsonRpcError> {
    info!("IPC: shutdown command received");

    let mut s = state.write().map_err(|_| JsonRpcError {
        code: INTERNAL_ERROR,
        message: "Failed to write state".into(),
        data: None,
    })?;
    s.running = false;

    Ok(serde_json::json!({
        "success": true,
        "message": "Daemon shutting down"
    }))
}

/// Get active profile ID (legacy alias).
async fn handle_get_active_profile(
    state: &DaemonStateRef,
) -> Result<serde_json::Value, JsonRpcError> {
    let s = state.read().map_err(|_| JsonRpcError {
        code: INTERNAL_ERROR,
        message: "Failed to read state".into(),
        data: None,
    })?;

    Ok(serde_json::json!({
        "profile_id": s.active_profile_id
    }))
}

/// Set active profile ID (legacy alias).
///
/// Do not mutate only `active_profile_id`: that used to leave the compiled
/// engine schema and persisted config pointing at different profiles. Translate
/// the old request into the canonical profile.activate command instead.
async fn handle_set_active_profile(
    state: &DaemonStateRef,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsonRpcError> {
    let params = params.ok_or(JsonRpcError {
        code: INVALID_PARAMS,
        message: "Missing params".into(),
        data: None,
    })?;

    let profile_id = params
        .get("profile_id")
        .and_then(|v| v.as_str())
        .ok_or(JsonRpcError {
            code: INVALID_PARAMS,
            message: "Missing 'profile_id'".into(),
            data: None,
        })?;

    crate::daemon::router::dispatch(
        "profile.activate",
        Some(serde_json::json!({ "id": profile_id })),
        state,
    )
    .await
    .map_err(|message| JsonRpcError {
        code: INTERNAL_ERROR,
        message,
        data: None,
    })?;

    info!("IPC legacy set_active_profile -> {}", profile_id);
    Ok(serde_json::json!({
        "success": true,
        "profile_id": profile_id
    }))
}

/// List profile IDs (legacy alias).
async fn handle_list_profiles() -> Result<serde_json::Value, JsonRpcError> {
    match crate::shared::persistence::list_profiles() {
        Ok(profiles) => Ok(serde_json::json!({
            "profiles": profiles
        })),
        Err(e) => Err(JsonRpcError {
            code: INTERNAL_ERROR,
            message: format!("Failed to list profiles: {}", e),
            data: None,
        }),
    }
}

/// Get daemon configuration (legacy alias).
async fn handle_get_config(_state: &DaemonStateRef) -> Result<serde_json::Value, JsonRpcError> {
    match crate::shared::config::load_config() {
        Ok(config) => Ok(serde_json::to_value(config).unwrap_or(serde_json::Value::Null)),
        Err(e) => Err(JsonRpcError {
            code: INTERNAL_ERROR,
            message: format!("Failed to load config: {}", e),
            data: None,
        }),
    }
}

/// Update daemon configuration (legacy alias).
async fn handle_update_config(
    state: &DaemonStateRef,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsonRpcError> {
    let params = params.ok_or_else(|| JsonRpcError {
        code: INVALID_PARAMS,
        message: "Missing parameters".to_string(),
        data: None,
    })?;
    let obj = params.as_object().ok_or_else(|| JsonRpcError {
        code: INVALID_PARAMS,
        message: "Configuration update must be a JSON object".to_string(),
        data: None,
    })?;

    let mut config = crate::shared::config::load_config().map_err(|e| JsonRpcError {
        code: INTERNAL_ERROR,
        message: format!("Failed to load config: {}", e),
        data: None,
    })?;

    let old_timeout = config.tap_hold_timeout_ms;

    if let Some(lang) = obj.get("language").and_then(|v| v.as_str()) {
        config.language = lang.to_string();
    }
    if let Some(theme) = obj.get("theme").and_then(|v| v.as_str()) {
        config.theme = theme.to_string();
    }
    if let Some(autostart) = obj.get("autostart").and_then(|v| v.as_bool()) {
        config.autostart = autostart;
    }
    if let Some(minimize) = obj.get("minimizeToTray").and_then(|v| v.as_bool()) {
        config.minimize_to_tray = minimize;
    }
    if let Some(kb) = obj.get("kbHookEnabled").and_then(|v| v.as_bool()) {
        config.kb_hook_enabled = kb;
    }
    if let Some(mouse) = obj.get("mouseHookEnabled").and_then(|v| v.as_bool()) {
        config.mouse_hook_enabled = mouse;
    }
    if let Some(debug) = obj.get("debugMode").and_then(|v| v.as_bool()) {
        config.debug_mode = debug;
    }
    if let Some(active_id) = obj.get("activeProfileId").and_then(|v| v.as_str()) {
        let exists = crate::shared::persistence::list_profiles()
            .map_err(|e| JsonRpcError {
                code: INTERNAL_ERROR,
                message: format!("Failed to list profiles: {}", e),
                data: None,
            })?
            .iter()
            .any(|id| id == active_id);
        if !exists {
            return Err(JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Profile '{}' does not exist", active_id),
                data: None,
            });
        }
        config.active_profile_id = active_id.to_string();
    }
    if let Some(scale) = obj.get("scale").and_then(|v| v.as_f64()) {
        config.scale = scale;
    }
    if let Some(restore) = obj.get("restoreMouseAfterMacro").and_then(|v| v.as_bool()) {
        config.restore_mouse_after_macro = restore;
    }
    if let Some(onboarding) = obj.get("onboardingComplete").and_then(|v| v.as_bool()) {
        config.onboarding_complete = onboarding;
    }
    if let Some(timeout) = obj.get("tapHoldTimeoutMs").and_then(|v| v.as_u64()) {
        config.tap_hold_timeout_ms = timeout;
    }
    if let Some(font_size) = obj.get("fontSize").and_then(|v| v.as_u64()) {
        config.font_size = font_size as u32;
    }
    if let Some(row_padding) = obj.get("rowPadding").and_then(|v| v.as_u64()) {
        config.row_padding = row_padding as u32;
    }

    crate::shared::config::save_config(&config).map_err(|e| JsonRpcError {
        code: INTERNAL_ERROR,
        message: format!("Failed to save config: {}", e),
        data: None,
    })?;

    if let Ok(mut s) = state.write() {
        s.kb_hook_enabled = config.kb_hook_enabled;
        s.mouse_hook_enabled = config.mouse_hook_enabled;
        s.restore_mouse_after_macro = config.restore_mouse_after_macro;

        if config.tap_hold_timeout_ms != old_timeout {
            if let Some(ref prof) = s.active_profile {
                let frontend_config = crate::schemas::frontend::FrontendConfig {
                    rules: prof.rules.clone(),
                    macros: vec![],
                    layers: prof.layers.clone(),
                    tap_hold_timeout_ms: config.tap_hold_timeout_ms,
                };
                s.engine_schema = crate::daemon::compiler::compile_schema(&frontend_config);
            }
        }
    }

    Ok(serde_json::json!({ "success": true }))
}
