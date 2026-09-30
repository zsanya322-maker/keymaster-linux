//! Виртуальные устройства uinput (клавиатура + мышь).
//!
//! Через них идёт и passthrough захваченных событий (когда движок решает
//! пропустить нажатие), и инъекция remap/макросов. Захваченные evdev-устройства
//! не отдают события системе, поэтому единственный путь обратно — виртуальные
//! устройства. Зацикливание невозможно: мы не читаем собственные виртуальные
//! ноды.

use std::io;
use std::sync::{Mutex, OnceLock};

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};

use crate::daemon::keymap;

const KEYBOARD_NAME: &str = "KeyMaster Virtual Keyboard";
const MOUSE_NAME: &str = "KeyMaster Virtual Mouse";

static KEYBOARD: OnceLock<Mutex<VirtualDevice>> = OnceLock::new();
static MOUSE: OnceLock<Mutex<VirtualDevice>> = OnceLock::new();

fn key_event(code: u16, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code, value)
}

fn rel_event(code: u16, value: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE.0, code, value)
}

/// Создать виртуальные устройства. Требует прав на /dev/uinput
/// (группа `input` или uaccess-правило).
pub fn init() -> Result<(), String> {
    if KEYBOARD.get().is_some() {
        return Ok(());
    }

    let key_codes = keymap::all_evdev_keyboard_keys();
    let mut key_set: AttributeSet<KeyCode> = AttributeSet::default();
    for code in key_codes {
        key_set.insert(KeyCode::new(code));
    }

    let keyboard = VirtualDevice::builder()
        .and_then(|builder| builder.name(KEYBOARD_NAME).with_keys(&key_set))
        .and_then(|builder| builder.build())
        .map_err(|e| format!("Не удалось создать виртуальную клавиатуру uinput: {}", e))?;
    let _ = KEYBOARD.set(Mutex::new(keyboard));

    let mut mouse_buttons: AttributeSet<KeyCode> = AttributeSet::default();
    for code in [0x110u16, 0x111, 0x112, 0x113, 0x114] {
        mouse_buttons.insert(KeyCode::new(code));
    }
    let mut mouse_axes: AttributeSet<RelativeAxisCode> = AttributeSet::default();
    for axis in [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
    ] {
        mouse_axes.insert(axis);
    }

    let mouse = VirtualDevice::builder()
        .and_then(|builder| builder.name(MOUSE_NAME).with_keys(&mouse_buttons))
        .and_then(|builder| builder.with_relative_axes(&mouse_axes))
        .and_then(|builder| builder.build())
        .map_err(|e| format!("Не удалось создать виртуальную мышь uinput: {}", e))?;
    let _ = MOUSE.set(Mutex::new(mouse));

    Ok(())
}

pub fn keyboard_ready() -> bool {
    KEYBOARD.get().is_some()
}

pub fn mouse_ready() -> bool {
    MOUSE.get().is_some()
}

/// Отправить событие в виртуальную клавиатуру (value: 1=down, 0=up, 2=repeat).
/// emit() сам завершает батч SYN_REPORT-ом.
pub fn emit_key(evdev_code: u16, value: i32) -> io::Result<()> {
    let Some(kb) = KEYBOARD.get() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "uinput keyboard not initialized",
        ));
    };
    let mut device = kb.lock().map_err(|e| io::Error::other(e.to_string()))?;
    device.emit(&[key_event(evdev_code, value)])
}

/// Отправить нажатие/отпускание кнопки мыши.
pub fn emit_mouse_button(evdev_code: u16, value: i32) -> io::Result<()> {
    let Some(mouse) = MOUSE.get() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "uinput mouse not initialized",
        ));
    };
    let mut device = mouse.lock().map_err(|e| io::Error::other(e.to_string()))?;
    device.emit(&[key_event(evdev_code, value)])
}

/// Относительное движение/скролл (REL_X/REL_Y/REL_WHEEL/REL_HWHEEL).
pub fn emit_rel(code: u16, value: i32) -> io::Result<()> {
    let Some(mouse) = MOUSE.get() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "uinput mouse not initialized",
        ));
    };
    let mut device = mouse.lock().map_err(|e| io::Error::other(e.to_string()))?;
    device.emit(&[rel_event(code, value)])
}

/// Нажать и отпустить клавишу по VK-коду.
pub fn tap_key(vk: u8) {
    if let Some(code) = keymap::vk_to_evdev(vk) {
        let _ = emit_key(code, 1);
        let _ = emit_key(code, 0);
    }
}
