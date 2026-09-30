pub mod context;
/// KeyMaster Linux Library
///
/// Handles Tauri GUI initialization, plugin registration, and commands.
pub mod daemon;
pub mod gui;
pub mod logging;
pub mod mcp;
pub mod platform;
pub mod schemas;
pub mod shared;
pub mod simulator;
pub mod trackers;

use gui::commands::GuiState;
use tauri::{Emitter, Manager};
use tracing::{error, info};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    info!("Starting GUI process");

    tauri::Builder::default()
        // Tauri рекомендует регистрировать single-instance первым. Повторный
        // запуск не создаёт вторую оболочку/второй GUI lifecycle — вместо этого
        // поднимаем уже существующее окно из tray.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(GuiState::default())
        .invoke_handler(tauri::generate_handler![
            gui::commands::greet,
            gui::commands::ipc_call,
            gui::commands::daemon_status,
            gui::commands::spawn_daemon,
            gui::commands::stop_daemon,
            gui::commands::quit_app,
            gui::commands::restart_app,
            gui::commands::restart_as_admin,
            gui::commands::is_elevated,
            gui::commands::get_gui_config,
            gui::commands::update_gui_config,
            gui::ai::ai_chat_completion,
            gui::commands::ai_secret_set,
            gui::commands::ai_secret_get,
            gui::commands::ai_secret_delete,
        ])
        .setup(|app| {
            // Background event subscription is a long-lived connection distinct
            // from request/response IPC. Handshake is bounded and validated so a
            // half-alive daemon cannot park this task forever before reconnect.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use tauri::Emitter;
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                use tokio::net::UnixStream;

                const SUBSCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
                const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

                loop {
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    let socket_path = crate::shared::constants::ipc_socket_path();

                    let mut pipe = match UnixStream::connect(&socket_path).await {
                        Ok(pipe) => pipe,
                        Err(_) => continue,
                    };

                    let request_id = serde_json::json!(999);
                    let sub_request = crate::daemon::ipc_types::JsonRpcRequest {
                        jsonrpc: "2.0".to_string(),
                        method: "subscribe_events".to_string(),
                        params: None,
                        id: Some(request_id.clone()),
                    };

                    let mut req_bytes = match serde_json::to_string(&sub_request) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!("Event subscription serialization failed: {}", error);
                            continue;
                        }
                    };
                    req_bytes.push('\n');

                    let send_result = tokio::time::timeout(SUBSCRIBE_TIMEOUT, async {
                        pipe.write_all(req_bytes.as_bytes()).await?;
                        pipe.flush().await
                    })
                    .await;
                    match send_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::debug!("Event subscription write failed: {}", error);
                            continue;
                        }
                        Err(_) => {
                            tracing::warn!("Event subscription write timeout");
                            continue;
                        }
                    }

                    let (reader, _writer) = tokio::io::split(pipe);
                    let mut lines = BufReader::new(reader).lines();
                    let ack_line = match tokio::time::timeout(SUBSCRIBE_TIMEOUT, lines.next_line()).await {
                        Ok(Ok(Some(line))) => line,
                        Ok(Ok(None)) => continue,
                        Ok(Err(error)) => {
                            tracing::debug!("Event subscription ACK read failed: {}", error);
                            continue;
                        }
                        Err(_) => {
                            tracing::warn!("Event subscription ACK timeout");
                            continue;
                        }
                    };

                    let ack = match serde_json::from_str::<crate::daemon::ipc_types::JsonRpcResponse>(&ack_line) {
                        Ok(response) => response,
                        Err(error) => {
                            tracing::warn!("Event subscription invalid ACK: {}", error);
                            continue;
                        }
                    };
                    let subscribed = ack.jsonrpc == "2.0"
                        && ack.id == request_id
                        && ack.error.is_none()
                        && ack
                            .result
                            .as_ref()
                            .and_then(|value| value.get("subscribed"))
                            .and_then(|value| value.as_bool())
                            == Some(true);
                    if !subscribed {
                        tracing::warn!("Event subscription rejected or malformed: {:?}", ack);
                        continue;
                    }

                    tracing::info!("GUI event listener subscribed to IPC socket: {}", socket_path.display());
                    while let Ok(Some(line)) = lines.next_line().await {
                        match serde_json::from_str::<crate::gui::events::DaemonEvent>(&line) {
                            Ok(event) => {
                                let _ = app_handle.emit("daemon-event", event);
                            }
                            Err(error) => {
                                tracing::warn!("Ignoring malformed daemon event: {}", error);
                            }
                        }
                    }
                    tracing::debug!("GUI event listener disconnected; reconnecting");
                }
            });

            let has_icon = app.default_window_icon().is_some();
            info!("Default window icon loaded: {}", has_icon);

            if let Err(e) = gui::tray::setup_tray(app) {
                error!("Failed to set up system tray: {:?}", e);
                return Err(e);
            }
            info!("System tray configured successfully");

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
                info!("Main window focused");
            } else {
                error!("Main webview window not found");
            }

            info!("GUI initialization complete");
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let minimize_to_tray = crate::shared::config::load_config()
                    .map(|config| config.minimize_to_tray)
                    .unwrap_or(true);

                if minimize_to_tray {
                    let _ = window.hide();
                } else {
                    // Само окно не закрываем сразу: React проверит, есть ли
                    // несохранённый черновик, и при необходимости покажет
                    // внутренний ConfirmDialog. Явный quit_app() завершит процесс.
                    let _ = window.emit("app-exit-requested", ());
                }
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("Tauri execution failed");
}
