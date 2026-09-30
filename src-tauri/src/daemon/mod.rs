pub mod automation;
pub mod chord_output;
pub mod compiler;
pub mod engine;
pub mod hooks;
pub mod input_state;
pub mod ipc;
pub mod ipc_client;
pub mod ipc_types;
pub mod keymap;
pub mod mouse_triggers;
pub mod profile_runtime;
pub mod router;
/// Daemon-специфичный код (background process)
///
/// Запускается с флагом --daemon. Содержит input capture (evdev), IPC сервер,
/// движки.
pub mod runner;
pub mod state;

#[cfg(test)]
mod router_tests;

pub mod text_expansion;
