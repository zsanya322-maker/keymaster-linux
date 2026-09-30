use std::sync::OnceLock;
/// Daemon main loop
///
/// Точка входа daemon-процесса. Запускает IPC сервер, input capture (evdev),
/// layer watcher, persistence thread.
///
/// Архитектура потоков:
/// - Reader-потоки evdev: захват клавиатуры/мыши
/// - Tokio Runtime: IPC Server, Persistence, Layer Watcher
/// - State: Arc<RwLock<DaemonState>>
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{error, info, warn};

use crate::daemon::state::{DaemonState, DaemonStateRef};
use crate::logging;
use crate::shared::config;
use crate::shared::constants;

/// Флаг для graceful shutdown
static DAEMON_RUNNING: AtomicBool = AtomicBool::new(true);

/// Глобальный хэндл Tokio для запуска задач из потоков reader'ов
pub static TOKIO_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Запустить асинхронную задачу на глобальном рантайме Tokio
pub fn spawn_on_runtime<F>(future: F)
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    if let Some(handle) = TOKIO_HANDLE.get() {
        handle.spawn(future);
    } else {
        error!("Tokio handle not initialized!");
    }
}

/// Разрешить профиль, с которым daemon должен стартовать.
///
/// Recovery-профиль от повреждённого/несовместимого файла никогда не становится
/// активным runtime-профилем, если можно выбрать здоровый fallback. Исходный
/// повреждённый файл при этом остаётся на диске и показывается GUI отдельно.
fn resolve_startup_profile(
    app_config: &mut crate::shared::types::AppConfig,
) -> Result<crate::shared::types::Profile, String> {
    let configured_id = app_config.active_profile_id.clone();
    if let Ok(profile) = crate::shared::persistence::load_profile_checked(&configured_id) {
        info!("Profile '{}' successfully loaded from disk", configured_id);
        return Ok(profile);
    }

    warn!(
        "Configured active profile '{}' is unavailable/corrupt; selecting safe fallback",
        configured_id
    );

    let ids = crate::shared::persistence::list_profiles()?;
    let had_profile_files = !ids.is_empty();
    let mut first_readable = None;
    let mut selected_default = None;

    for id in ids {
        if id == configured_id {
            continue;
        }
        match crate::shared::persistence::load_profile_checked(&id) {
            Ok(profile) => {
                if profile.is_default {
                    selected_default = Some(profile);
                    break;
                }
                if first_readable.is_none() {
                    first_readable = Some(profile);
                }
            }
            Err(error) => {
                warn!("Skipping unhealthy fallback profile '{}': {}", id, error);
            }
        }
    }

    let profile = if let Some(profile) = selected_default.or(first_readable) {
        info!("Startup fallback profile selected: '{}'", profile.id);
        profile
    } else {
        // Empty installation keeps the historical ID `1`. If files exist but
        // none are healthy, create a fresh UUID instead of risking overwrite
        // of an incompatible/corrupt `1.json`.
        let id = if had_profile_files {
            uuid::Uuid::new_v4().to_string()
        } else {
            "1".to_string()
        };
        let default_profile = crate::shared::types::Profile {
            id: id.clone(),
            name: "Default".to_string(),
            is_default: true,
            linked_apps: vec![],
            bindings: vec![],
            order: 0,
            rules: vec![],
            macros: vec![],
            layers: vec![],
            folders: vec![],
        };
        crate::shared::persistence::save_profile(&default_profile)?;
        info!("Created startup default profile '{}'", id);
        default_profile
    };

    if app_config.active_profile_id != profile.id {
        app_config.active_profile_id = profile.id.clone();
        config::save_config(app_config)?;
        info!(
            "Recovered activeProfileId in config -> '{}'",
            app_config.active_profile_id
        );
    }

    Ok(profile)
}

/// Запустить daemon-процесс
///
/// Вызывается из main.rs когда передан флаг `--daemon`.
/// Блокирует текущий поток до получения сигнала завершения.
pub fn run_daemon(parent_pid: Option<u32>) -> Result<(), String> {
    DAEMON_RUNNING.store(true, Ordering::SeqCst);

    logging::init_logging()?;
    info!(
        "KeyMaster Linux Daemon v{} starting...",
        env!("CARGO_PKG_VERSION")
    );

    // Создаём runtime и резервируем первый сокет ПЕРЕД чтением/миграцией
    // persistence и перед любыми background engines. Это настоящий startup gate:
    // второй daemon завершается до simulator/context/input capture.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("km-daemon")
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    let first_ipc_server =
        tokio_rt.block_on(async { crate::daemon::ipc::reserve_first_socket_instance() })?;

    // load_config сам безопасно восстанавливает повреждённый legacy config, но
    // future schema / I/O failure считаются fatal: старый daemon не имеет права
    // запускаться с дефолтами и затем случайно перезаписать более новый config.
    let mut app_config = config::load_config()
        .map_err(|e| format!("Не удалось безопасно загрузить config.json: {}", e))?;
    info!("Configuration loaded. Language: {}", app_config.language);
    let loaded_profile = resolve_startup_profile(&mut app_config)?;

    let state = DaemonState::from_config(&app_config).into_ref();

    // С этого места единственный daemon ownership уже подтверждён.
    let simulator_tx = crate::simulator::spawn_simulator_thread();
    if let Ok(mut s) = state.write() {
        s.simulator = Some(simulator_tx);
    }

    crate::trackers::context_tracker::spawn_context_tracker(
        crate::context::AppContextState::default(),
    );

    let _ = TOKIO_HANDLE.set(tokio_rt.handle().clone());

    let ipc_state = state.clone();
    let shutdown_state = state.clone();

    tokio_rt.spawn(async move {
        if let Err(e) = crate::daemon::ipc::start_ipc_server(ipc_state, first_ipc_server).await {
            error!("IPC server stopped with error: {}", e);
            request_shutdown();
        }
    });

    if let Some(pid) = parent_pid {
        tokio_rt.spawn(async move {
            info!("Запущен мониторинг родительского процесса PID: {}", pid);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if !is_process_alive(pid) {
                    warn!(
                        "Родительский процесс PID {} завершился. Самоликвидация daemon'а...",
                        pid
                    );
                    request_shutdown();
                    break;
                }
            }
        });
    }

    // Синхронизировать runtime-настройки, которые hook/engine читают из DaemonState.
    // GUI сохраняет config.json напрямую; daemon следит только за mtime файла и
    // перечитывает его при реальном изменении, поэтому здесь нет постоянного JSON-I/O.
    let config_sync_state = state.clone();
    tokio_rt.spawn(async move {
        let config_path = match crate::shared::persistence::app_data_dir() {
            Ok(dir) => dir.join("config.json"),
            Err(e) => {
                warn!("Config sync disabled: {}", e);
                return;
            }
        };
        let mut last_modified = std::fs::metadata(&config_path)
            .and_then(|meta| meta.modified())
            .ok();

        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let running = config_sync_state
                .read()
                .map(|s| s.running)
                .unwrap_or(false);
            if !running {
                break;
            }

            let modified = match std::fs::metadata(&config_path).and_then(|meta| meta.modified()) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if last_modified
                .as_ref()
                .is_some_and(|previous| *previous == modified)
            {
                continue;
            }
            last_modified = Some(modified);

            let updated = match crate::shared::config::load_config() {
                Ok(config) => config,
                Err(e) => {
                    warn!("Не удалось перечитать config для runtime sync: {}", e);
                    continue;
                }
            };

            if let Ok(mut s) = config_sync_state.write() {
                let changed = s.kb_hook_enabled != updated.kb_hook_enabled
                    || s.mouse_hook_enabled != updated.mouse_hook_enabled
                    || s.restore_mouse_after_macro != updated.restore_mouse_after_macro
                    || s.macro_emergency_stop_vk != updated.macro_emergency_stop_vk
                    || s.auto_switch_profiles != updated.auto_switch_profiles
                    || s.manual_profile_lock != updated.manual_profile_lock;

                s.kb_hook_enabled = updated.kb_hook_enabled;
                s.mouse_hook_enabled = updated.mouse_hook_enabled;
                s.restore_mouse_after_macro = updated.restore_mouse_after_macro;
                s.macro_emergency_stop_vk = updated.macro_emergency_stop_vk;
                s.auto_switch_profiles = updated.auto_switch_profiles;
                s.manual_profile_lock = updated.manual_profile_lock;
                s.auto_switch_profiles=updated.auto_switch_profiles;
                s.manual_profile_lock=updated.manual_profile_lock;

                if changed {
                    info!(
                        "Runtime config applied: keyboard={}, mouse={}, restore_mouse_after_macro={}, macro_emergency_stop_vk={}",
                        s.kb_hook_enabled,
                        s.mouse_hook_enabled,
                        s.restore_mouse_after_macro,
                        s.macro_emergency_stop_vk
                    );
                }
            }
        }
    });

    // Profile auto-switch is runtime-only. The persisted/preferred id changes only
    // through profile.activate. Disk/profile evaluation is gated by foreground revision.
    let profile_switch_state = state.clone();
    tokio_rt.spawn(async move {
        let mut last_revision = u64::MAX;
        let mut last_signature: Option<(bool, bool, String, String)> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            let (running, auto_switch, manual_lock, preferred, current) =
                match profile_switch_state.read() {
                    Ok(daemon) => (
                        daemon.running,
                        daemon.auto_switch_profiles,
                        daemon.manual_profile_lock,
                        daemon.preferred_profile_id.clone(),
                        daemon.active_profile_id.clone(),
                    ),
                    Err(_) => continue,
                };
            if !running {
                break;
            }

            let context = crate::trackers::context_tracker::get_context()
                .and_then(|context| context.read().ok().map(|value| value.clone()))
                .unwrap_or_default();
            let signature = (auto_switch, manual_lock, preferred.clone(), current.clone());
            if last_revision == context.revision && last_signature.as_ref() == Some(&signature) {
                continue;
            }
            last_revision = context.revision;
            last_signature = Some(signature);

            let target = if auto_switch && !manual_lock {
                let mut profiles = crate::shared::persistence::list_profiles()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|id| crate::shared::persistence::load_profile_checked(&id).ok())
                    .collect::<Vec<_>>();
                profiles.sort_by(|a, b| {
                    a.order
                        .cmp(&b.order)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                profiles
                    .into_iter()
                    .find(|profile| {
                        crate::daemon::profile_runtime::profile_matches(profile, &context)
                    })
                    .or_else(|| crate::shared::persistence::load_profile_checked(&preferred).ok())
            } else {
                crate::shared::persistence::load_profile_checked(&preferred).ok()
            };

            if let Some(profile) = target {
                if profile.id != current {
                    if let Err(error) = crate::daemon::profile_runtime::activate_runtime(
                        &profile_switch_state,
                        profile,
                    ) {
                        warn!("Profile auto-switch failed: {}", error);
                    }
                }
            }
        }
    });

    let taphold_state = state.clone();
    tokio_rt.spawn(async move {
        info!("Запущен фоновый ticker для Tap-Hold маппингов");
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;

            if let Ok(s) = taphold_state.read() {
                if !s.running {
                    break;
                }
            }

            crate::daemon::engine::tick_tap_holds(Some(&taphold_state));
        }
    });

    {
        if let Ok(mut s) = state.write() {
            let frontend_config = crate::schemas::frontend::FrontendConfig {
                rules: loaded_profile.rules.clone(),
                macros: vec![],
                layers: loaded_profile.layers.clone(),
                tap_hold_timeout_ms: app_config.tap_hold_timeout_ms,
            };
            s.engine_schema = crate::daemon::compiler::compile_schema(&frontend_config);
            s.active_profile_id = loaded_profile.id.clone();
            s.active_profile = Some(loaded_profile);
        }
    }

    crate::daemon::hooks::install_hooks(state.clone())?;

    {
        let mut s = state
            .write()
            .map_err(|e| format!("Failed to lock state: {}", e))?;
        s.hooks_installed = true;
    }

    info!("KeyMaster Linux Daemon started and ready");
    info!("IPC Socket: {}", constants::ipc_socket_path().display());

    run_message_loop(&state);

    info!("Daemon shutting down...");
    DAEMON_RUNNING.store(false, Ordering::SeqCst);

    {
        let mut s = shutdown_state
            .write()
            .map_err(|e| format!("Ошибка блокировки state: {}", e))?;
        s.running = false;
    }

    crate::daemon::hooks::uninstall_hooks();
    tokio_rt.shutdown_background();
    crate::trackers::context_tracker::stop_context_tracker();

    // Убираем сокет, чтобы не оставлять остаток после выхода
    let _ = std::fs::remove_file(constants::ipc_socket_path());

    info!("KeyMaster Linux Daemon остановлен");
    Ok(())
}

/// Главный цикл daemon-потока: ждёт сигнала остановки.
fn run_message_loop(_state: &DaemonStateRef) {
    while DAEMON_RUNNING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Инициировать graceful shutdown daemon-процесса
pub fn request_shutdown() {
    DAEMON_RUNNING.store(false, Ordering::SeqCst);
}

/// Проверить, запущен ли процесс по его PID
fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Проверить наличие живого daemon-сокета (подключением, без побочных эффектов).
/// Unix-сокет отвечает мгновенно (успех/отказ), таймаут не нужен.
pub fn is_daemon_running() -> bool {
    use std::os::unix::net::UnixStream;

    let socket_path = constants::ipc_socket_path();
    if !socket_path.exists() {
        return false;
    }
    UnixStream::connect(&socket_path).is_ok()
}
