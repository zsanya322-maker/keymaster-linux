use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

pub mod macro_player;
pub mod system;

use self::macro_player::{MacroExecutor, MacroPlayer};
use crate::daemon::keymap;
use crate::platform;
use crate::schemas::engine::{MacroPlaybackConfig, SimulatorCommand};

/// Два независимых канала симуляции.
///
/// `immediate_tx` обслуживает короткие реакции на хук (обычный remap, media,
/// text expansion) и не должен ждать `Delay` из длинного макроса.
/// `macro_tx` последовательно воспроизводит целые макросы на отдельном worker.
#[derive(Clone)]
pub struct SimulatorSender {
    immediate_tx: Sender<SimulatorCommand>,
    macro_player: MacroPlayer,
}

impl std::fmt::Debug for SimulatorSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatorSender").finish_non_exhaustive()
    }
}

impl SimulatorSender {
    /// Отправить мгновенную команду. Сигнатура намеренно похожа на mpsc::Sender,
    /// чтобы существующие места вызова не усложнять.
    pub fn send(&self, command: SimulatorCommand) -> Result<(), SendError<SimulatorCommand>> {
        self.immediate_tx.send(command)
    }

    /// Поставить в очередь macro-job. Delay/repeat/cancellation живут только в
    /// macro-player и никогда не блокируют immediate remap queue.
    pub fn send_macro(
        &self,
        commands: Vec<SimulatorCommand>,
        playback: MacroPlaybackConfig,
        macro_key: u64,
    ) -> Result<u64, String> {
        self.macro_player.enqueue(commands, playback, macro_key)
    }

    pub fn cancel_macro_key(&self, macro_key: u64) {
        self.macro_player.cancel_macro_key(macro_key);
    }

    pub fn cancel_current_macro(&self) {
        self.macro_player.cancel_current();
    }

    pub fn cancel_all_macros(&self) {
        self.macro_player.cancel_all();
    }
}

/// Внутренний конструктор worker'ов с внедряемым executor.
///
/// В production executor вызывает Win32 SendInput. В тестах можно подставить
/// детерминированный наблюдатель и проверить именно архитектуру очередей, не
/// генерируя реальные нажатия клавиш на CI-машине.
fn spawn_simulator_with_executor(executor: MacroExecutor) -> SimulatorSender {
    let (immediate_tx, immediate_rx): (Sender<SimulatorCommand>, Receiver<SimulatorCommand>) =
        mpsc::channel();
    let macro_player = MacroPlayer::spawn(Arc::clone(&executor));

    let immediate_executor = Arc::clone(&executor);
    thread::Builder::new()
        .name("km-simulator".to_string())
        .spawn(move || {
            info!("Immediate simulator thread started.");
            while let Ok(command) = immediate_rx.recv() {
                immediate_executor(command);
            }
            info!("Immediate simulator thread channel closed, exiting.");
        })
        .expect("Failed to spawn simulator thread");

    SimulatorSender {
        immediate_tx,
        macro_player,
    }
}

pub fn spawn_simulator_thread() -> SimulatorSender {
    spawn_simulator_with_executor(Arc::new(execute_command))
}

fn execute_command(cmd: SimulatorCommand) {
    match cmd {
        SimulatorCommand::PressKey(code) => send_key(code, false),
        SimulatorCommand::ReleaseKey(code) => send_key(code, true),
        SimulatorCommand::MousePress(code) => send_mouse(code, false),
        SimulatorCommand::MouseRelease(code) => send_mouse(code, true),
        SimulatorCommand::TypeString(text) => type_string(&text),
        SimulatorCommand::Delay(ms) => thread::sleep(Duration::from_millis(ms as u64)),
        SimulatorCommand::MouseMove { dx, dy } => move_mouse(dx, dy),
        SimulatorCommand::MouseScroll { delta } => scroll_mouse(delta, false),
        SimulatorCommand::MouseHScroll { delta } => scroll_mouse(delta, true),
        SimulatorCommand::MouseAbsolute { x, y } => move_mouse_absolute(x, y),
        SimulatorCommand::RestorePhysicalModifiers { mask } => {
            for vk in crate::daemon::engine::currently_held_modifier_vks(mask) {
                send_key(vk, false);
            }
        }
    }
}

fn send_key(vk: u8, is_keyup: bool) {
    let Some(code) = keymap::vk_to_evdev(vk) else {
        return;
    };
    let _ = platform::uinput::emit_key(code, if is_keyup { 0 } else { 1 });
}

fn send_mouse(button: u8, is_keyup: bool) {
    let Some(code) = keymap::button_to_evdev(button) else {
        return;
    };
    let _ = platform::uinput::emit_mouse_button(code, if is_keyup { 0 } else { 1 });
}

/// Набор текста через uinput (US-раскладка). Не-ASCII символы в MVP
/// пропускаются с предупреждением: произвольный Unicode требует
/// zwp_virtual_keyboard (Wayland) или XTEST+keymap (X11).
fn type_string(text: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    for ch in text.chars() {
        let Some((vk, shift)) = keymap::char_to_vk_us(ch) else {
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!("type_string: символ '{}' (не-ASCII/US) пропущен — расширенный ввод текста будет в следующих версиях", ch);
            }
            continue;
        };
        let Some(code) = keymap::vk_to_evdev(vk) else {
            continue;
        };
        if shift {
            if let Some(shift_code) = keymap::vk_to_evdev(0xA0) {
                let _ = platform::uinput::emit_key(shift_code, 1);
            }
        }
        let _ = platform::uinput::emit_key(code, 1);
        let _ = platform::uinput::emit_key(code, 0);
        if shift {
            if let Some(shift_code) = keymap::vk_to_evdev(0xA0) {
                let _ = platform::uinput::emit_key(shift_code, 0);
            }
        }
    }
}

fn move_mouse(dx: i32, dy: i32) {
    if dx != 0 {
        let _ = platform::uinput::emit_rel(
            evdev::RelativeAxisCode::REL_X.0,
            dx,
        );
    }
    if dy != 0 {
        let _ = platform::uinput::emit_rel(
            evdev::RelativeAxisCode::REL_Y.0,
            dy,
        );
    }
}

fn scroll_mouse(delta: i32, horizontal: bool) {
    let notches = delta / 120;
    if notches == 0 {
        return;
    }
    let code = if horizontal {
        evdev::RelativeAxisCode::REL_HWHEEL.0
    } else {
        evdev::RelativeAxisCode::REL_WHEEL.0
    };
    let _ = platform::uinput::emit_rel(code, notches);
}

/// Абсолютное перемещение курсора: на X11 через warp, иначе недоступно
/// (Wayland не даёт клиенту управлять глобальной позицией напрямую).
fn move_mouse_absolute(x: i32, y: i32) {
    if platform::x11::warp_pointer(x, y) {
        return;
    }
    tracing::warn!("MouseAbsolute: абсолютное позиционирование недоступно (нет X11) — пропущено");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn macro_delay_does_not_block_immediate_queue() {
        let (observed_tx, observed_rx) = mpsc::channel::<String>();
        let executor: MacroExecutor = Arc::new(move |command| {
            if let SimulatorCommand::TypeString(text) = command {
                let _ = observed_tx.send(text);
            }
        });

        let sender = spawn_simulator_with_executor(executor);
        sender
            .send_macro(
                vec![
                    SimulatorCommand::Delay(250),
                    SimulatorCommand::TypeString("macro-finished".to_string()),
                ],
                MacroPlaybackConfig::default(),
                1,
            )
            .expect("macro queue should be available");

        std::thread::sleep(Duration::from_millis(25));
        sender
            .send(SimulatorCommand::TypeString("immediate".to_string()))
            .expect("immediate queue should be available");

        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "immediate",
            "immediate command must execute while macro worker is delayed",
        );
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "macro-finished",
        );
    }

    #[test]
    fn macro_jobs_remain_serial_and_ordered() {
        let (observed_tx, observed_rx) = mpsc::channel::<String>();
        let executor: MacroExecutor = Arc::new(move |command| {
            if let SimulatorCommand::TypeString(text) = command {
                let _ = observed_tx.send(text);
            }
        });

        let sender = spawn_simulator_with_executor(executor);
        sender
            .send_macro(
                vec![
                    SimulatorCommand::TypeString("a".to_string()),
                    SimulatorCommand::TypeString("b".to_string()),
                ],
                MacroPlaybackConfig::default(),
                1,
            )
            .unwrap();
        sender
            .send_macro(
                vec![SimulatorCommand::TypeString("c".to_string())],
                MacroPlaybackConfig::default(),
                2,
            )
            .unwrap();

        let observed = (0..3)
            .map(|_| observed_rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observed, vec!["a", "b", "c"]);
    }
}
