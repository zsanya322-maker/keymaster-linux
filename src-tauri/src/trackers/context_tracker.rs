//! Context tracker: отслеживание активного окна/процесса/виртуального стола.
//!
//! Замена WinEventHook из Windows-версии. На KDE Wayland используется
//! org.kde.KWin.queryWindowInfo (DBus), на X11 — EWMH-опрос. Опрос 250мс:
//! событий "фокус сменился" на Wayland клиенты не получают.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use tracing::info;

use crate::context::{AppContext, AppContextState};
use crate::platform;

static GLOBAL_CONTEXT: OnceLock<AppContextState> = OnceLock::new();
static TRACKER_STARTED: AtomicBool = AtomicBool::new(false);
static TRACKER_STOP: AtomicBool = AtomicBool::new(false);

pub fn init_context() -> AppContextState {
    if let Some(existing) = GLOBAL_CONTEXT.get() {
        return existing.clone();
    }
    let context = Arc::new(RwLock::new(AppContext::default()));
    let _ = GLOBAL_CONTEXT.set(context.clone());
    GLOBAL_CONTEXT.get().cloned().unwrap_or(context)
}

pub fn get_context() -> Option<AppContextState> {
    GLOBAL_CONTEXT.get().cloned()
}

fn refresh_from_x11(context: &AppContextState) -> bool {
    let Some(active) = platform::x11::active_window() else {
        return false;
    };
    if let Ok(mut state) = context.write() {
        state.revision = state.revision.wrapping_add(1);
        state.active_window_id = active.window_id;
        state.active_process = active.process;
        state.active_process_path = active.process_path;
        state.active_window_title = active.title;
        state.active_window_class = active.class;
        state.window_width = active.width;
        state.window_height = active.height;
        state.fullscreen = active.fullscreen;
        state.monitor_id = active.monitor_id;
        state.virtual_desktop_id = active.virtual_desktop;
    }
    true
}

fn refresh_from_kde(context: &AppContextState) -> bool {
    let Some(active) = platform::kde::active_window() else {
        return false;
    };
    if let Ok(mut state) = context.write() {
        state.revision = state.revision.wrapping_add(1);
        // Идентичности HWND на Wayland нет — используем hash процесса+заголовка
        state.active_window_id = crate::shared::calculate_hash(&(
            active.process.clone(),
            active.title.clone(),
        )) as isize;
        state.active_process = active.process;
        state.active_process_path = active.process_path;
        state.active_window_title = active.title;
        state.active_window_class = String::new();
        state.window_width = active.width;
        state.window_height = active.height;
        state.fullscreen = active.fullscreen;
        state.monitor_id = String::new();
        state.virtual_desktop_id = active.virtual_desktop;
    }
    true
}

fn refresh(context: &AppContextState) {
    // Wayland-сессия: KWin DBus приоритетен (X11 видит только XWayland-окна),
    // но X11 доступен и как fallback, когда KWin-интерфейс недоступен.
    if platform::session_kind() == platform::SessionKind::Wayland
        && platform::kde::kwin_available()
    {
        if refresh_from_kde(context) {
            return;
        }
    }
    if !refresh_from_x11(context) {
        // Нет ни Wayland-KWin, ни X11 (например, tty) — контекст не обновляется
    }
}

pub fn spawn_context_tracker(initial: AppContextState) {
    let _ = GLOBAL_CONTEXT.set(initial);
    if TRACKER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    TRACKER_STOP.store(false, Ordering::SeqCst);
    let context = match GLOBAL_CONTEXT.get() {
        Some(context) => context.clone(),
        None => return,
    };
    thread::Builder::new()
        .name("context-tracker".into())
        .spawn(move || {
            info!("Context tracker started");
            refresh(&context);
            while !TRACKER_STOP.load(Ordering::SeqCst) {
                thread::sleep(std::time::Duration::from_millis(250));
                if TRACKER_STOP.load(Ordering::SeqCst) {
                    break;
                }
                refresh(&context);
            }
            TRACKER_STARTED.store(false, Ordering::SeqCst);
            info!("Context tracker stopped");
        })
        .expect("Failed to start context tracker");
}

pub fn start_context_tracker() {
    spawn_context_tracker(init_context());
}

pub fn stop_context_tracker() {
    TRACKER_STOP.store(true, Ordering::SeqCst);
    TRACKER_STARTED.store(false, Ordering::SeqCst);
}
