//! Windows 系统托盘：左键切换弹窗、失败时回退显示主窗口。
use crate::AppState;
use tauri::tray::{
    MouseButton as TrayMouseButton, MouseButtonState as TrayMouseButtonState, TrayIconBuilder,
    TrayIconEvent,
};
use tauri::{AppHandle, Manager, State};

/// 与托盘构建时使用的 `TrayIconBuilder::with_id` 一致，供按 ID 查找。
pub const WINDOWS_TRAY_ID: &str = "calendar-tray";

/// 创建 Windows 托盘图标并绑定左键点击切换弹窗行为。
pub fn setup_windows_tray(app_handle: &AppHandle, state: &State<'_, AppState>) {
    // 共享窗口管理器句柄，用于托盘点击时切换弹窗。
    let shared_window_manager = state.window_manager.clone();
    // 回退逻辑使用的应用句柄副本。
    let fallback_app_handle = app_handle.clone();
    // 读取默认图标创建托盘实例。
    if let Some(default_icon) = app_handle.default_window_icon() {
        // 构建托盘菜单。
        if let Ok(tray_menu) = crate::menu::build_tray_menu(app_handle) {
            let _ = TrayIconBuilder::with_id(WINDOWS_TRAY_ID)
                .icon(default_icon.clone())
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .tooltip("松鼠日历")
                .on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click { button, button_state, .. } = event {
                        if button == TrayMouseButton::Left
                            && button_state == TrayMouseButtonState::Up
                        {
                            // 与任务栏时钟左键一致：优先切换桌面日历，避免弹出第二个月历。
                            let toggled = shared_window_manager
                                .lock()
                                .ok()
                                .and_then(|mut guard| {
                                    guard.as_mut().map(|wm| wm.toggle_clock_calendar().is_ok())
                                })
                                .unwrap_or(false);
                            if toggled {
                                return;
                            }
                            // 找不到窗口管理器时回退显示主窗口。
                            if let Some(main_window) =
                                fallback_app_handle.get_webview_window("main")
                            {
                                let _ = main_window.show();
                                let _ = main_window.set_focus();
                            }
                        }
                    }
                })
                .build(app_handle);
        }
    }
}
