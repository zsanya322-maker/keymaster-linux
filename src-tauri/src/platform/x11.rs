//! X11: активное окно, позиция курсора, оконные действия (EWMH), DPMS.
//!
//! Работает и в чистом X11, и через XWayland — но на Wayland-сессии видит
//! только XWayland-окна; для нативных окон используется kde.rs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use x11rb::connection::Connection;
use x11rb::protocol::dpms;
use x11rb::protocol::randr;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask,
};
use x11rb::protocol::xproto::{Atom, Window};
use x11rb::rust_connection::RustConnection;

/// Информация об активном окне (заполняет AppContext).
pub struct ActiveWindow {
    pub window_id: isize,
    pub process: String,
    pub process_path: String,
    pub title: String,
    pub class: String,
    pub width: i32,
    pub height: i32,
    pub fullscreen: bool,
    pub monitor_id: String,
    pub virtual_desktop: String,
}

static ATOMS: OnceLock<Mutex<HashMap<&'static str, Atom>>> = OnceLock::new();

// RustConnection не реализует Clone, поэтому держим одно соединение под
// глобальным мьютексом: операции редкие (опрос 250мс + действия по требованию),
// стоимость блокировки несущественна.
static CONN_SINGLE: OnceLock<Mutex<Option<Conn>>> = OnceLock::new();

struct Conn {
    conn: RustConnection,
    screen: usize,
}

fn with_conn<T>(f: impl FnOnce(&RustConnection, usize) -> Option<T>) -> Option<T> {
    let slot = CONN_SINGLE.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().ok()?;
    if guard.is_none() {
        match RustConnection::connect(None) {
            Ok((conn, screen)) => {
                *guard = Some(Conn { conn, screen });
            }
            Err(_) => return None,
        }
    }
    let held = guard.as_mut()?;
    match f(&held.conn, held.screen) {
        Some(value) => Some(value),
        None => {
            // Мёртвое соединение — сбрасываем, следующая попытка переподключится.
            *guard = None;
            None
        }
    }
}

fn intern(conn: &RustConnection, name: &'static str) -> Option<Atom> {
    let cache = ATOMS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(atom) = map.get(name) {
            return Some(*atom);
        }
    }
    let atom = conn
        .intern_atom(false, name.as_bytes())
        .ok()?
        .reply()
        .ok()?
        .atom;
    if let Ok(mut map) = cache.lock() {
        map.insert(name, atom);
    }
    Some(atom)
}

const A_ACTIVE: &str = "_NET_ACTIVE_WINDOW";
const A_CLIENT_LIST: &str = "_NET_CLIENT_LIST";
const A_CLOSE: &str = "_NET_CLOSE_WINDOW";
const A_WM_STATE: &str = "_NET_WM_STATE";
const A_MAX_HORZ: &str = "_NET_WM_STATE_MAXIMIZED_HORZ";
const A_MAX_VERT: &str = "_NET_WM_STATE_MAXIMIZED_VERT";
const A_WM_PID: &str = "_NET_WM_PID";
const A_WM_NAME: &str = "_NET_WM_NAME";
const A_UTF8: &str = "UTF8_STRING";
const A_CURRENT_DESKTOP: &str = "_NET_CURRENT_DESKTOP";
const A_CHANGE_STATE: &str = "WM_CHANGE_STATE";

fn root(conn: &RustConnection, screen: usize) -> Window {
    conn.setup().roots[screen].root
}

fn cardinals(conn: &RustConnection, win: Window, atom: Atom) -> Option<Vec<u32>> {
    let reply = conn
        .get_property(false, win, atom, AtomEnum::CARDINAL, 0, 16)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(
        reply
            .value32()?
            .collect::<Vec<u32>>(),
    )
}

fn wm_name_string(conn: &RustConnection, win: Window) -> Option<String> {
    let reply = conn
        .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 4096)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = reply.value;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).to_string())
}

fn utf8_property(conn: &RustConnection, win: Window, name: &'static str) -> Option<String> {
    let type_atom = intern(conn, A_UTF8)?;
    let value_atom = intern(conn, name)?;
    let reply = conn
        .get_property(false, win, value_atom, type_atom, 0, 8192)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&reply.value).to_string())
}

fn wm_class(conn: &RustConnection, win: Window) -> Option<(String, String)> {
    let reply = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 2048)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    let bytes = reply.value;
    let mut parts = bytes.split(|&b| b == 0).filter(|p| !p.is_empty());
    let instance = parts.next().map(|p| String::from_utf8_lossy(p).to_string());
    let class = parts.next().map(|p| String::from_utf8_lossy(p).to_string());
    Some((
        instance.unwrap_or_default(),
        class.unwrap_or_default(),
    ))
}

fn pid_of(conn: &RustConnection, win: Window) -> Option<u32> {
    let atom = intern(conn, A_WM_PID)?;
    cardinals(conn, win, atom)?.first().copied()
}

fn process_path_by_pid(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{}/exe", pid))
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn monitor_id_for(conn: &RustConnection, screen: usize, wx: i32, wy: i32) -> String {
    let Ok(cookie) = randr::get_monitors(conn, root(conn, screen) as u32, true) else {
        return String::new();
    };
    let Ok(monitors) = cookie.reply() else {
        return String::new();
    };
    for m in &monitors.monitors {
        if wx >= m.x as i32
            && wy >= m.y as i32
            && wx < (m.x as i32 + m.width as i32)
            && wy < (m.y as i32 + m.height as i32)
        {
            if let Ok(cookie) = conn.get_atom_name(m.name) {
                if let Ok(reply) = cookie.reply() {
                    return String::from_utf8_lossy(&reply.name).to_string();
                }
            }
        }
    }
    String::new()
}

/// Активное окно (EWMH). None — если X11 недоступен или активного окна нет.
pub fn active_window() -> Option<ActiveWindow> {
    with_conn(|conn, screen| {
        let root = root(conn, screen);
        let active_atom = intern(conn, A_ACTIVE)?;
        let win = *cardinals(conn, root, active_atom)?.first()?;
        if win == 0 {
            return None;
        }

        let class_pair = wm_class(conn, win).unwrap_or_default();
        let pid = pid_of(conn, win);
        let class = class_pair.1.clone();
        let process = if class.is_empty() {
            let comm = pid
                .and_then(|p| std::fs::read_to_string(format!("/proc/{}/comm", p)).ok())
                .unwrap_or_default();
            crate::shared::clean_process_name(comm.trim())
        } else {
            crate::shared::clean_process_name(&class)
        };

        let geom = conn.get_geometry(win).ok()?.reply().ok()?;
        let coords = conn
            .translate_coordinates(win, root, 0, 0)
            .ok()?
            .reply()
            .ok()?;

        let screen_w = conn.setup().roots[screen].width_in_pixels as i32;
        let screen_h = conn.setup().roots[screen].height_in_pixels as i32;
        let fullscreen = coords.dst_x <= 1
            && coords.dst_y <= 1
            && geom.width as i32 >= screen_w - 1
            && geom.height as i32 >= screen_h - 1;

        let desktop = intern(conn, A_CURRENT_DESKTOP)
            .and_then(|atom| cardinals(conn, root, atom))
            .and_then(|values| values.first().map(|v| v.to_string()))
            .unwrap_or_default();

        Some(ActiveWindow {
            window_id: win as isize,
            process,
            process_path: pid.map(process_path_by_pid).unwrap_or_default(),
            title: utf8_property(conn, win, A_WM_NAME).unwrap_or_default(),
            class: class_pair.1,
            width: geom.width as i32,
            height: geom.height as i32,
            fullscreen,
            monitor_id: monitor_id_for(conn, screen, coords.dst_x as i32, coords.dst_y as i32),
            virtual_desktop: desktop,
        })
    })
}

/// Позиция курсора в координатах root-окна.
pub fn cursor_position() -> Option<(i32, i32)> {
    with_conn(|conn, screen| {
        let reply = conn.query_pointer(root(conn, screen)).ok()?.reply().ok()?;
        Some((reply.root_x as i32, reply.root_y as i32))
    })
}

/// Телепортировать курсор в абсолютные координаты.
pub fn warp_pointer(x: i32, y: i32) -> bool {
    with_conn(|conn, screen| {
        Some(
            conn.warp_pointer(root(conn, screen), root(conn, screen), 0, 0, 0, 0, x as i16, y as i16)
                .is_ok(),
        )
    })
    .unwrap_or(false)
}

fn send_client_message(
    conn: &RustConnection,
    screen: usize,
    event_window: Window,
    message_type: Atom,
    data: [u32; 5],
) -> bool {
    let event = ClientMessageEvent::new(
        32,
        event_window,
        message_type,
        ClientMessageData::from(data),
    );
    conn.send_event(
        false,
        root(conn, screen),
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )
    .is_ok()
}

fn active_window_id(conn: &RustConnection, screen: usize) -> Option<Window> {
    let atom = intern(conn, A_ACTIVE)?;
    let win = *cardinals(conn, root(conn, screen), atom)?.first()?;
    (win != 0).then_some(win)
}

/// Оконные действия над активным окном: snap_left/right/center, minimize,
/// maximize, close. Полный аналог execute_window_action из Windows-версии.
pub fn execute_window_action(action: &str) {
    with_conn(|conn, screen| {
        let root_win = root(conn, screen);
        let win = active_window_id(conn, screen)?;
        let geom = conn.get_geometry(win).ok()?.reply().ok()?;
        let _coords = conn
            .translate_coordinates(win, root_win, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        let screen_w = conn.setup().roots[screen].width_in_pixels as i32;
        let screen_h = conn.setup().roots[screen].height_in_pixels as i32;

        match action {
            "snap_left" => {
                let _ = conn.configure_window(
                    win,
                    &ConfigureWindowAux::new()
                        .x(0)
                        .y(0)
                        .width((screen_w / 2) as u32)
                        .height(screen_h as u32),
                );
            }
            "snap_right" => {
                let _ = conn.configure_window(
                    win,
                    &ConfigureWindowAux::new()
                        .x((screen_w / 2) as i32)
                        .y(0)
                        .width((screen_w / 2) as u32)
                        .height(screen_h as u32),
                );
            }
            "snap_center" => {
                let x = (screen_w - geom.width as i32) / 2;
                let y = (screen_h - geom.height as i32) / 2;
                let _ = conn.configure_window(
                    win,
                    &ConfigureWindowAux::new()
                        .x(x)
                        .y(y)
                        .width(geom.width as u32)
                        .height(geom.height as u32),
                );
            }
            "minimize" => {
                let atom = intern(conn, A_CHANGE_STATE)?;
                send_client_message(conn, screen, win, atom, [3, 0, 0, 0, 0]);
            }
            "maximize" => {
                let state = intern(conn, A_WM_STATE)?;
                let horz = intern(conn, A_MAX_HORZ)?;
                let vert = intern(conn, A_MAX_VERT)?;
                send_client_message(conn, screen, win, state, [1, horz, vert, 0, 0]);
            }
            "close" => {
                let atom = intern(conn, A_CLOSE)?;
                send_client_message(conn, screen, win, atom, [0, 0, 0, 0, 0]);
            }
            _ => {}
        }
        Some(())
    });
}

/// Поднять окно указанного процесса/заголовка (аналог focus_process).
pub fn focus_process(process: Option<&str>, title: Option<&str>) {
    with_conn(|conn, screen| {
        let target_process = process
            .map(crate::shared::clean_process_name)
            .filter(|p| !p.is_empty());
        let target_title = title
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty());
        if target_process.is_none() && target_title.is_none() {
            return None;
        }

        let list_atom = intern(conn, A_CLIENT_LIST)?;
        let windows = cardinals(conn, root(conn, screen), list_atom)?;

        for win in windows {
            let matched_process = target_process.as_ref().is_some_and(|target| {
                let class = wm_class(conn, win).map(|(_, class)| class).unwrap_or_default();
                if crate::shared::clean_process_name(&class) == *target {
                    return true;
                }
                pid_of(conn, win).is_some_and(|pid| {
                    let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
                        .unwrap_or_default();
                    crate::shared::clean_process_name(comm.trim()) == *target
                })
            });
            let matched_title = target_title.as_ref().is_some_and(|target| {
                utf8_property(conn, win, A_WM_NAME)
                    .or_else(|| wm_name_string(conn, win))
                    .is_some_and(|t| t.to_lowercase().contains(target))
            });

            if matched_process || matched_title {
                let active_atom = intern(conn, A_ACTIVE)?;
                let current = active_window_id(conn, screen).unwrap_or(0);
                let _ = conn.configure_window(
                    win,
                    &ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
                );
                send_client_message(conn, screen, win, active_atom, [2, 0, current, 0, 0]);
                return Some(());
            }
        }
        None
    });
}

/// Выключить монитор через DPMS (только X11-сессия).
pub fn monitor_off() -> bool {
    with_conn(|conn, _screen| Some(dpms::force_level(conn, dpms::DPMSMode::OFF).is_ok()))
        .unwrap_or(false)
}

/// Список X11-окон для отладки.
#[allow(dead_code)]
pub fn list_windows() -> Vec<(u32, String)> {
    with_conn(|conn, screen| {
        let list_atom = intern(conn, A_CLIENT_LIST)?;
        Some(
            cardinals(conn, root(conn, screen), list_atom)?
                .into_iter()
                .map(|win| {
                    let title =
                        utf8_property(conn, win, A_WM_NAME).unwrap_or_else(|| "?".to_string());
                    (win, title)
                })
                .collect(),
        )
    })
    .unwrap_or_default()
}
