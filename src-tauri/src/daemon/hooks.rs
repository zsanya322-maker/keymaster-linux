//! Keyboard & Mouse input capture через evdev (замена SetWindowsHookEx).
//!
//! Daemon захватывает физические устройства ввода (EVIOCGRAB) и читает их
//! напрямую. Каждое событие проходит тот же конвейер, что и LL-хук Windows:
//! capture-режим KeyPicker → emergency stop → запись макросов → движок правил.
//! Событие, которое движок решил пропустить, воспроизводится через виртуальные
//! uinput-устройства (см. platform::uinput). Зацикливание невозможно: мы не
//! читаем собственные виртуальные ноды.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use evdev::{Device, EventType, KeyCode, RelativeAxisCode};
use tracing::{error, info, warn};

use crate::daemon::engine;
use crate::daemon::keymap;
use crate::daemon::state::DaemonStateRef;
use crate::platform;

/// Флаг остановки reader-потоков
static RUNNING: AtomicBool = AtomicBool::new(true);
static KB_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static MOUSE_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Глобальная ссылка на состояние Daemon
static GLOBAL_STATE: OnceLock<DaemonStateRef> = OnceLock::new();

pub static LAST_RECORDED_MOUSE_POS: Mutex<Option<(i32, i32)>> = Mutex::new(None);
pub static LAST_RECORDED_MOUSE_TIME: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Приблизительная позиция курсора. На X11 синхронизируется запросами,
/// на Wayland накапливается из относительных движений.
static CURSOR_POS: Mutex<(i32, i32)> = Mutex::new((0, 0));

/// Результат установки хуков
#[derive(Debug)]
pub struct HookHandles {
    pub kb_thread_id: u32,
    pub mouse_thread_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    Keyboard,
    Mouse,
}

/// Установить input-захват на устройствах evdev
pub fn install_hooks(state: DaemonStateRef) -> Result<HookHandles, String> {
    let _ = GLOBAL_STATE.set(state.clone());
    RUNNING.store(true, Ordering::SeqCst);

    platform::uinput::init()?;
    sync_cursor_from_display();

    let devices = collect_input_devices()?;
    if devices.is_empty() {
        return Err(
            "Не найдено устройств ввода в /dev/input. Проверьте права: пользователь должен \
             состоять в группе 'input' (sudo usermod -aG input $USER) и перелогиниться."
                .to_string(),
        );
    }

    let mut kb_count = 0usize;
    let mut mouse_count = 0usize;
    for (path, kind) in devices {
        match kind {
            DeviceKind::Keyboard => kb_count += 1,
            DeviceKind::Mouse => mouse_count += 1,
        }
        let state_reader = state.clone();
        std::thread::Builder::new()
            .name(format!("km-evdev-{:?}-{}", kind, path.display()))
            .spawn(move || run_reader(path, kind, state_reader))
            .map_err(|e| format!("Не удалось создать поток reader: {}", e))?;
    }
    info!(
        "evdev: захват {} клавиатурных и {} мышиных устройств",
        kb_count, mouse_count
    );

    // Ждём пока reader'ы захватят устройства (с таймаутом)
    let timeout = std::time::Instant::now();
    loop {
        let kb_ok = KB_HOOK_INSTALLED.load(Ordering::SeqCst) || kb_count == 0;
        let mouse_ok = MOUSE_HOOK_INSTALLED.load(Ordering::SeqCst) || mouse_count == 0;
        if kb_ok && mouse_ok {
            break;
        }
        if timeout.elapsed() > std::time::Duration::from_secs(5) {
            return Err("Таймаут захвата устройств ввода (5с)".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    info!("Хуки keyboard + mouse установлены (evdev)");

    Ok(HookHandles {
        kb_thread_id: 0,
        mouse_thread_id: 0,
    })
}

/// Деинсталлировать захват (установить флаги остановки)
pub fn uninstall_hooks() {
    RUNNING.store(false, Ordering::SeqCst);
    KB_HOOK_INSTALLED.store(false, Ordering::SeqCst);
    MOUSE_HOOK_INSTALLED.store(false, Ordering::SeqCst);
    engine::reset_modifier_state();
    info!("Хуки деинсталлированы");
}

/// Перечислить и классифицировать устройства /dev/input/event*.
/// Touchpad'ы (ABS-мыши) намеренно не захватываются: их события требуют
/// пересылки ABS-осей, а ремап кнопок тачпада — не MVP-функция.
fn collect_input_devices() -> Result<Vec<(PathBuf, DeviceKind)>, String> {
    let mut result = Vec::new();
    let entries = std::fs::read_dir("/dev/input")
        .map_err(|e| format!("Не удалось прочитать /dev/input: {}. Есть ли права (группа input)?", e))?;

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("event") {
            continue;
        }
        let path = entry.path();
        let Ok(device) = Device::open(&path) else {
            warn!("evdev: не удалось открыть {} (нет прав?)", path.display());
            continue;
        };
        let dev_name = device.name().unwrap_or("").to_string();
        if dev_name.contains("KeyMaster Virtual") {
            continue;
        }

        let keys = device.supported_keys();
        let rel = device.supported_relative_axes();
        let has_rel = rel
            .map(|r| r.contains(RelativeAxisCode::REL_X) || r.contains(RelativeAxisCode::REL_Y))
            .unwrap_or(false);
        let has_mouse_buttons = keys
            .map(|k| k.contains(KeyCode::BTN_LEFT) || k.contains(KeyCode::BTN_RIGHT))
            .unwrap_or(false);
        let has_typing_keys = keys
            .map(|k| {
                k.contains(KeyCode::KEY_Q)
                    || k.contains(KeyCode::KEY_P)
                    || k.contains(KeyCode::KEY_SPACE)
                    || k.contains(KeyCode::KEY_KP1)
                    || k.contains(KeyCode::KEY_VOLUMEUP)
                    || k.contains(KeyCode::KEY_PLAYPAUSE)
            })
            .unwrap_or(false);

        if has_rel && has_mouse_buttons {
            result.push((path, DeviceKind::Mouse));
        } else if has_typing_keys {
            result.push((path, DeviceKind::Keyboard));
        }
    }
    Ok(result)
}

fn run_reader(path: PathBuf, kind: DeviceKind, state: DaemonStateRef) {
    let mut device = match Device::open(&path) {
        Ok(device) => device,
        Err(e) => {
            error!("evdev: не удалось открыть {}: {}", path.display(), e);
            return;
        }
    };
    if let Err(e) = device.grab() {
        error!("evdev: не удалось захватить {}: {}. Другой демон (kanata/ydotool) уже держит устройство?", path.display(), e);
        return;
    }

    match kind {
        DeviceKind::Keyboard => {
            KB_HOOK_INSTALLED.store(true, Ordering::SeqCst);
            info!("evdev: клавиатура захвачена: {}", path.display());
        }
        DeviceKind::Mouse => {
            MOUSE_HOOK_INSTALLED.store(true, Ordering::SeqCst);
            info!("evdev: мышь захвачена: {}", path.display());
        }
    }

    while RUNNING.load(Ordering::SeqCst) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    match kind {
                        DeviceKind::Keyboard => handle_event_keyboard(event, &state),
                        DeviceKind::Mouse => handle_event_mouse(event, &state),
                    }
                }
            }
            Err(e) => {
                if RUNNING.load(Ordering::SeqCst) {
                    error!("evdev: ошибка чтения {}: {}", path.display(), e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }

    let _ = device.ungrab();
    info!("evdev: устройство отпущено: {}", path.display());
}

fn forward_keyboard(event: evdev::InputEvent) {
    let _ = platform::uinput::emit_key(event.code(), event.value());
}


fn sync_cursor_from_display() {
    if let Some((x, y)) = platform::x11::cursor_position() {
        if let Ok(mut pos) = CURSOR_POS.lock() {
            *pos = (x, y);
        }
    }
}

fn cursor() -> (i32, i32) {
    match CURSOR_POS.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn set_cursor(x: i32, y: i32) {
    if let Ok(mut pos) = CURSOR_POS.lock() {
        *pos = (x, y);
    }
}

/// Обработка события клавиатурного устройства (аналог keyboard_hook_callback).
fn handle_event_keyboard(event: evdev::InputEvent, state_ref: &DaemonStateRef) {
    if event.event_type() != EventType::KEY {
        return; // LED/MSC от физических устройств не пересылаем
    }

    let code = event.code();
    let value = event.value();

    // Мышинные кнопки на совмещённом устройстве идут по мышиный конвейер
    if (0x110..=0x117).contains(&code) {
        handle_mouse_button(code, value, state_ref);
        return;
    }

    let Some(vk_code) = keymap::evdev_to_vk(code) else {
        // Неизвестная клавиша — просто пропускаем дальше
        forward_keyboard(event);
        return;
    };

    // Windows LL-хук получает автоповторы как WM_KEYDOWN — value=2 трактуем так же
    let is_key_down = value != 0;
    let scan_code = code;
    let event_modifiers = engine::update_modifier_state(vk_code, is_key_down);

    tracing::debug!(
        "evdev kb: code={}, vk={:#x}, value={}, mods={:#x}",
        code,
        vk_code,
        value,
        event_modifiers
    );

    let s = match state_ref.read() {
        Ok(s) => s,
        Err(_) => {
            forward_keyboard(event);
            return;
        }
    };

    // Режим захвата клавиши для KeyPicker: собираем chord сами и блокируем
    // событие, чтобы Win/Alt-комбинации не вызывали системных действий.
    if s.key_capture_active.load(Ordering::Relaxed) {
        if is_key_down && !engine::is_modifier_vk(vk_code) {
            let captured = if vk_code == 0x1B {
                crate::schemas::frontend::KeyChord::single(0)
            } else {
                crate::schemas::frontend::KeyChord {
                    code: vk_code,
                    modifiers: event_modifiers,
                }
            };
            if let Ok(mut slot) = s.last_captured_key.lock() {
                *slot = Some(captured);
            }
        }
        return; // Блокируем (не пересылаем)
    }

    // Emergency stop макросов — до записи/диспетчеризации правил
    if is_key_down && s.macro_emergency_stop_vk != 0 && vk_code == s.macro_emergency_stop_vk {
        if let Some(simulator) = &s.simulator {
            simulator.cancel_all_macros();
        }
        tracing::warn!("Macro emergency stop triggered (VK={})", vk_code);
        return;
    }

    // F12 — запуск/остановка записи макроса
    if vk_code == 0x7B {
        if is_key_down {
            if s.is_recording.load(Ordering::Relaxed) {
                s.is_recording.store(false, Ordering::Relaxed);
                crate::gui::events::broadcast_event(
                    crate::gui::events::DaemonEvent::MacroRecordingStopped {
                        macro_id: "".to_string(),
                    },
                );
                tracing::info!("Запись макроса остановлена по нажатию F12");
            } else if s.record_ready.load(Ordering::Relaxed) {
                s.is_recording.store(true, Ordering::Relaxed);
                if let Ok(mut last_time) = s.last_record_time.lock() {
                    *last_time = None;
                }
                if let Ok(mut last_pos) = LAST_RECORDED_MOUSE_POS.lock() {
                    *last_pos = None;
                }
                if let Ok(mut last_mouse_time) = LAST_RECORDED_MOUSE_TIME.lock() {
                    *last_mouse_time = None;
                }
                tracing::info!("Запись макроса запущена по нажатию F12");
            }
        }
        return; // F12 не уходит в систему
    }

    // Запись макроса: клавиатурные шаги
    if s.is_recording.load(Ordering::Relaxed) {
        if let Ok(mut last_time_lock) = s.last_record_time.lock() {
            let now = std::time::Instant::now();
            let delay_ms = match *last_time_lock {
                Some(last) => now.duration_since(last).as_millis() as u32,
                None => 0,
            };
            *last_time_lock = Some(now);

            let action = if is_key_down {
                crate::schemas::frontend::MacroAction::KeyDown { code: vk_code }
            } else {
                crate::schemas::frontend::MacroAction::KeyUp { code: vk_code }
            };

            let step = crate::schemas::frontend::MacroStep { action, delay_ms };
            if let Ok(mut steps) = s.recorded_steps.lock() {
                steps.push(step.clone());
            }
            if let Ok(step_json) = serde_json::to_value(&step) {
                crate::gui::events::broadcast_event(
                    crate::gui::events::DaemonEvent::MacroRecordingStep { step: step_json },
                );
            }
        }
    }

    drop(s);

    let start = std::time::Instant::now();
    let action = engine::process_keyboard_event(
        vk_code,
        scan_code,
        is_key_down,
        0,
        event_modifiers,
        Some(state_ref),
    );

    let elapsed = start.elapsed().as_micros() as u64;
    if let Ok(s) = state_ref.read() {
        s.last_latency_us.store(elapsed, Ordering::Relaxed);
        if is_key_down {
            s.keystrokes_processed.fetch_add(1, Ordering::Relaxed);
        }
    }

    if matches!(action, engine::EventAction::PassThrough) {
        forward_keyboard(event);
    }
}

/// Кнопки мыши (BTN_*) — общий путь для мышиных и совмещённых устройств.
fn handle_mouse_button(code: u16, value: i32, state_ref: &DaemonStateRef) {
    let Some(button) = keymap::evdev_button_to_button(code) else {
        // Неизвестная кнопка — пропускаем
        let _ = platform::uinput::emit_mouse_button(code, value);
        return;
    };
    let is_mouse_down = value == 1;

    // Трекинг нажатых кнопок (для drag-детекции при записи макросов)
    update_button_state(button, is_mouse_down);

    // На реальном клике синхронизируем позицию с X11 (на Wayland остаётся накопленной)
    sync_cursor_from_display();
    let (x, y) = cursor();

    let record_kind = {
        let s = match state_ref.read() {
            Ok(s) => s,
            Err(_) => {
                let _ = platform::uinput::emit_mouse_button(code, value);
                return;
            }
        };

        // Захват кнопки для KeyPicker: запоминаем код кнопки 1-5 при mouse down
        if s.key_capture_active.load(Ordering::Relaxed) {
            if is_mouse_down {
                if let Ok(mut captured) = s.last_captured_mouse.lock() {
                    *captured = Some(button);
                    tracing::debug!("Захвачена кнопка мыши для KeyPicker: {}", button);
                }
            }
            None
        } else if s.is_recording.load(Ordering::Relaxed) {
            // Запись макроса: клики
            Some(RecordKind::Button {
                button,
                down: is_mouse_down,
            })
        } else {
            None
        }
        // guard здесь освобождается перед вызовом движка
    };

    if let Some(kind) = record_kind {
        record_mouse_step(state_ref, kind, x, y);
    }

    let start = std::time::Instant::now();
    let action = engine::process_mouse_event(
        button, x, y, 0, false, false, 0, is_mouse_down, Some(state_ref),
    );
    let elapsed = start.elapsed().as_micros() as u64;
    if let Ok(s) = state_ref.read() {
        s.last_latency_us.store(elapsed, Ordering::Relaxed);
    }

    if matches!(action, engine::EventAction::PassThrough) {
        let _ = platform::uinput::emit_mouse_button(code, value);
    }
}

/// Битовая маска нажатых физических кнопок (1..=5 → биты 0..=4).
static BUTTONS_DOWN: AtomicU32 = AtomicU32::new(0);

fn update_button_state(button: u8, down: bool) {
    if !(1..=5).contains(&button) {
        return;
    }
    let mask = 1u32 << (button - 1);
    if down {
        BUTTONS_DOWN.fetch_or(mask, Ordering::Relaxed);
    } else {
        BUTTONS_DOWN.fetch_and(!mask, Ordering::Relaxed);
    }
}

fn any_button_down() -> bool {
    BUTTONS_DOWN.load(Ordering::Relaxed) != 0
}

/// Относительные движения и скролл мыши (REL_*).
fn handle_event_mouse(event: evdev::InputEvent, state_ref: &DaemonStateRef) {
    if event.event_type() != EventType::RELATIVE {
        // ABS/прочее от мышиных устройств не пересылаем (не заявлено в виртуальном устройстве)
        return;
    }

    let code = event.code();
    let value = event.value();

    let is_wheel = code == RelativeAxisCode::REL_WHEEL.0;
    let is_hwheel = code == RelativeAxisCode::REL_HWHEEL.0;
    let is_move = code == RelativeAxisCode::REL_X.0 || code == RelativeAxisCode::REL_Y.0;

    let (x, y, delta, record_kind) = if is_wheel || is_hwheel {
        sync_cursor_from_display();
        let (x, y) = cursor();
        let delta = value * 120; // Windows-совместимый wheel delta (120 на щелчок)
        let record_kind = match state_ref.read() {
            Ok(s) => {
                if s.is_recording.load(Ordering::Relaxed) {
                    Some(RecordKind::Scroll { delta, horizontal: is_hwheel })
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        (x, y, delta, record_kind)
    } else if is_move {
        let (old_x, old_y) = cursor();
        let (nx, ny) = if code == RelativeAxisCode::REL_X.0 {
            (old_x + value, old_y)
        } else {
            (old_x, old_y + value)
        };
        set_cursor(nx, ny);

        // Запись движений мыши при записи макроса (порог 15px, троттлинг 100мс)
        let record_kind = match state_ref.read() {
            Ok(s) => {
                if s.is_recording.load(Ordering::Relaxed)
                    && s.record_mouse_moves.load(Ordering::Relaxed)
                {
                    let is_drag = if s.record_mouse_drag_drop_only.load(Ordering::Relaxed) {
                        any_button_down()
                    } else {
                        true
                    };
                    if is_drag && moved_far_enough(nx, ny) {
                        Some(RecordKind::Move { x: nx, y: ny })
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        (nx, ny, 0, record_kind)
    } else {
        // REL_MISC и прочее — пропускаем дальше без обработки движком
        let _ = platform::uinput::emit_rel(code, value);
        return;
    };

    if let Some(kind) = record_kind {
        record_mouse_step(state_ref, kind, x, y);
    }

    let action = engine::process_mouse_event(
        255, x, y, delta, is_hwheel, is_move, 0, false, Some(state_ref),
    );

    if matches!(action, engine::EventAction::PassThrough) {
        let _ = platform::uinput::emit_rel(code, value);
    }
}

/// Порог 15 пикселей для сглаживания мелких движений при записи.
fn moved_far_enough(x: i32, y: i32) -> bool {
    let mut should = false;
    if let Ok(last_pos_guard) = LAST_RECORDED_MOUSE_POS.lock() {
        match *last_pos_guard {
            Some((lx, ly)) => {
                if (x - lx).abs() > 15 || (y - ly).abs() > 15 {
                    should = true;
                }
            }
            None => should = true,
        }
    }
    should
}

enum RecordKind {
    Button { button: u8, down: bool },
    Scroll { delta: i32, horizontal: bool },
    Move { x: i32, y: i32 },
}

/// Записать шаг макроса для мыши. Портировано из mouse_hook_callback
/// Windows-версии: перед кликом/скроллом вставляется MouseToAbsolute.
fn record_mouse_step(state_ref: &DaemonStateRef, kind: RecordKind, x: i32, y: i32) {
    let Ok(s) = state_ref.read() else {
        return;
    };
    let now = std::time::Instant::now();
    let mut steps_to_record = Vec::new();

    let mut current_delay = 0u32;
    if let Ok(last_time) = s.last_record_time.lock() {
        current_delay = match *last_time {
            Some(last) => now.duration_since(last).as_millis() as u32,
            None => 0,
        };
    }

    // Вставляем координаты перед кликом/скроллом, если позиция изменилась
    if !matches!(kind, RecordKind::Move { .. }) {
        let mut need_force_mouse_pos = false;
        if let Ok(mut last_pos_guard) = LAST_RECORDED_MOUSE_POS.lock() {
            match *last_pos_guard {
                Some((lx, ly)) => {
                    if lx != x || ly != y {
                        *last_pos_guard = Some((x, y));
                        need_force_mouse_pos = true;
                    }
                }
                None => {
                    *last_pos_guard = Some((x, y));
                    need_force_mouse_pos = true;
                }
            }
        }
        if need_force_mouse_pos {
            if let Ok(mut last_mouse_time) = LAST_RECORDED_MOUSE_TIME.lock() {
                *last_mouse_time = Some(now);
            }
            steps_to_record.push(crate::schemas::frontend::MacroStep {
                action: crate::schemas::frontend::MacroAction::MouseToAbsolute { x, y },
                delay_ms: current_delay,
            });
            current_delay = 0;
        }
    }

    let action_to_record = match kind {
        RecordKind::Button { button, down } => {
            if down {
                Some(crate::schemas::frontend::MacroAction::MouseDown { code: button })
            } else {
                Some(crate::schemas::frontend::MacroAction::MouseUp { code: button })
            }
        }
        RecordKind::Scroll { delta, horizontal } => Some(if horizontal {
            crate::schemas::frontend::MacroAction::MouseHScroll { delta }
        } else {
            crate::schemas::frontend::MacroAction::MouseScroll { delta }
        }),
        RecordKind::Move { x, y } => {
            // Троттлинг 100 мс для предотвращения сотен шагов
            let mut should_record = false;
            if let Ok(mut last_mouse_time) = LAST_RECORDED_MOUSE_TIME.lock() {
                match *last_mouse_time {
                    Some(last_t) => {
                        if now.duration_since(last_t).as_millis() >= 100 {
                            should_record = true;
                        }
                    }
                    None => should_record = true,
                }
                if should_record {
                    *last_mouse_time = Some(now);
                }
            }
            if should_record {
                if let Ok(mut last_pos_guard) = LAST_RECORDED_MOUSE_POS.lock() {
                    *last_pos_guard = Some((x, y));
                }
                Some(crate::schemas::frontend::MacroAction::MouseToAbsolute { x, y })
            } else {
                None
            }
        }
    };

    if let Some(action) = action_to_record {
        steps_to_record.push(crate::schemas::frontend::MacroStep {
            action,
            delay_ms: current_delay,
        });
    }

    if !steps_to_record.is_empty() {
        if let Ok(mut last_time_lock) = s.last_record_time.lock() {
            *last_time_lock = Some(now);
        }
        if let Ok(mut steps) = s.recorded_steps.lock() {
            for step in &steps_to_record {
                steps.push(step.clone());
            }
        }
        for step in steps_to_record {
            if let Ok(step_json) = serde_json::to_value(&step) {
                crate::gui::events::broadcast_event(
                    crate::gui::events::DaemonEvent::MacroRecordingStep { step: step_json },
                );
            }
        }
    }
}
