/// System Tray setup
use tauri::{
    App, Emitter, Manager,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
};

pub const MENU_SHOW: &str = "show";
pub const MENU_RESTART_ADMIN: &str = "restart_admin";
pub const MENU_PROFILE_PREV: &str = "profile_prev";
pub const MENU_PROFILE_NEXT: &str = "profile_next";
pub const MENU_QUIT: &str = "quit";

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn switch_profile(direction: isize) {
    tauri::async_runtime::spawn(async move {
        let Ok(value) = crate::daemon::ipc_client::call("profile.list", None).await else {
            return;
        };
        let Some(profiles) = value.get("profiles").and_then(|value| value.as_array()) else {
            return;
        };
        if profiles.is_empty() {
            return;
        }
        let active = value
            .get("active")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let current = profiles
            .iter()
            .position(|profile| profile.get("id").and_then(|value| value.as_str()) == Some(active))
            .unwrap_or(0) as isize;
        let target = (current + direction).rem_euclid(profiles.len() as isize) as usize;
        let target_id = profiles[target]
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        if let Some(id) = target_id {
            let _ = crate::daemon::ipc_client::call(
                "profile.activate",
                Some(serde_json::json!({ "id": id })),
            )
            .await;
        }
    });
}

pub fn setup_tray(app: &App) -> Result<(), Box<dyn std::error::Error>> {
    let menu = MenuBuilder::new(app)
        .item(&MenuItemBuilder::with_id(MENU_SHOW, "Показать KeyMaster Linux").build(app)?)
        .separator()
        .item(&MenuItemBuilder::with_id(MENU_PROFILE_PREV, "← Предыдущий профиль").build(app)?)
        .item(&MenuItemBuilder::with_id(MENU_PROFILE_NEXT, "Следующий профиль →").build(app)?)
        .separator()
        .item(&MenuItemBuilder::with_id(MENU_QUIT, "Выйти").build(app)?)
        .build()?;

    let icon = match app.default_window_icon() {
        Some(icon) => icon.clone(),
        None => {
            tracing::warn!("Default window icon not found");
            return Err("Default window icon not found".into());
        }
    };

    let tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("KeyMaster Linux - active")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            MENU_SHOW => show_main_window(app),
            MENU_RESTART_ADMIN => {
                show_main_window(app);
                let _ = app.emit("app-restart-admin-requested", ());
            }
            MENU_PROFILE_PREV => switch_profile(-1),
            MENU_PROFILE_NEXT => switch_profile(1),
            MENU_QUIT => {
                show_main_window(app);
                let _ = app.emit("app-exit-requested", ());
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button == tauri::tray::MouseButton::Left
                    && button_state == tauri::tray::MouseButtonState::Up
                {
                    show_main_window(tray.app_handle());
                }
            }
        })
        .build(app)?;

    app.manage(tray);
    Ok(())
}
