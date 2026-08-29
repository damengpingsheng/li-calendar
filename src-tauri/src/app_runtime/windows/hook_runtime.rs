//! 任务栏低级鼠标钩子与点击事件分发。
use crate::window_manager::shared::popup_manager::PopupManager;
use crate::window_manager::CalendarWindowManager;
use crate::windows_hook::{
    is_desktop_in_foreground, start_hook_message_thread, ClickEvent, MouseButton,
    WindowsHookManager, IS_MENU_OPEN,
};
use crate::AppState;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenu, MF_STRING, TPM_RETURNCMD,
    TPM_RIGHTBUTTON,
};

// Win32 菜单命令 ID。不使用 tauri 的 `popup_menu`/`MenuEvent`——detached popup 菜单在 Windows 上
// 不会把点击事件派发到 `on_menu_event`（实测菜单点「退出」无反应），故改用原生 TrackPopupMenu。
const MENU_SETTINGS_ID: u32 = 1001;
const MENU_EXIT_ID: u32 = 1002;

/// 启动任务栏 Hook 运行时并接管点击事件流。
pub fn start_taskbar_runtime(app_handle: AppHandle, state: &AppState) {
    // 检查任务栏功能开关，未开启时跳过初始化。
    if !state.taskbar_widget_enabled.load(Ordering::SeqCst) {
        return;
    }
    // 防止重复启动 Hook 运行时。
    if state.taskbar_init_started.swap(true, Ordering::SeqCst) {
        return;
    }
    // 克隆共享窗口管理器句柄供异步任务使用。
    let shared_window_manager = Arc::clone(&state.window_manager);
    // 克隆共享 Hook 管理器句柄用于回填状态。
    let shared_hook_manager = Arc::clone(&state.hook_manager);
    tauri::async_runtime::spawn(async move {
        // 创建当前运行时专用的 Hook 管理器。
        let mut runtime_hook_manager = WindowsHookManager::new();
        // 启动消息循环线程（安装钩子后同线程会做一次 UIA 初始化时钟区域缓存）。
        start_hook_message_thread();
        // 获取事件接收器并开始监听点击事件。
        if let Some(event_receiver) = runtime_hook_manager.take_event_receiver() {
            tauri::async_runtime::spawn(start_hook_listener(
                app_handle.clone(),
                event_receiver,
                Arc::clone(&shared_window_manager),
            ));
        }
        // 将 Hook 管理器写回全局状态。
        if let Ok(mut hook_manager_guard) = shared_hook_manager.lock() {
            *hook_manager_guard = Some(runtime_hook_manager);
        }
        println!("Windows 钩子系统已初始化");
    });
}

/// 监听并处理来自系统时钟区域的点击事件。
pub async fn start_hook_listener(
    app_handle: AppHandle,
    mut event_receiver: mpsc::UnboundedReceiver<ClickEvent>,
    window_manager: Arc<Mutex<Option<CalendarWindowManager>>>,
) {
    while let Some(click_event) = event_receiver.recv().await {
        if is_desktop_in_foreground() {
            if let Ok(window_manager_guard) = window_manager.lock() {
                if let Some(calendar_window_manager) = window_manager_guard.as_ref() {
                    calendar_window_manager.refresh_desktop_window_visibility();
                }
            }
        }

        if !click_event.in_clock_area {
            continue;
        }
        match click_event.button {
            MouseButton::Left => handle_left_click(&window_manager, click_event.x, click_event.y),
            MouseButton::Right => handle_right_click(&app_handle, click_event.x, click_event.y),
        }
    }
}

/// 处理左键点击事件并切换日历弹窗。
fn handle_left_click(
    window_manager: &Arc<Mutex<Option<CalendarWindowManager>>>,
    click_x: i32,
    click_y: i32,
) {
    if let Ok(mut window_manager_guard) = window_manager.lock() {
        if let Some(calendar_window_manager) = window_manager_guard.as_mut() {
            if let Err(error) = calendar_window_manager.toggle_popup_at_position(click_x, click_y) {
                eprintln!("❌ 切换日历窗口失败: {}", error);
                if let Err(fallback_error) = calendar_window_manager.toggle_popup() {
                    eprintln!("❌ 回退到默认切换也失败: {}", fallback_error);
                }
            }
        }
    }
}

/// 处理右键点击事件：用 Win32 原生 `TrackPopupMenu` 弹出「设置/退出」菜单。
///
/// 使用 `TPM_RETURNCMD` 让 `TrackPopupMenu` **同步返回所选命令 ID**，从而完全避开 tauri
/// `popup_menu` 在 Windows 上不派发 `MenuEvent` 的缺陷（修复右键菜单点「退出」无反应）。
fn handle_right_click(app_handle: &AppHandle, mouse_x: i32, mouse_y: i32) {
    if IS_MENU_OPEN.load(Ordering::SeqCst) {
        return;
    }
    IS_MENU_OPEN.store(true, Ordering::SeqCst);
    unsafe {
        if let Ok(hmenu) = CreatePopupMenu() {
            let _ = AppendMenuW(hmenu, MF_STRING, MENU_SETTINGS_ID as usize, w!("设置"));
            let _ = AppendMenuW(hmenu, MF_STRING, MENU_EXIT_ID as usize, w!("退出"));
            // windows crate 将 TrackPopupMenu 返回类型标为 BOOL；在 TPM_RETURNCMD 下该值即选中的命令 ID。
            let ret = TrackPopupMenu(
                hmenu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                mouse_x,
                mouse_y,
                Some(0),
                HWND::default(),
                None,
            );
            let _ = DestroyMenu(hmenu);
            let cmd_id = ret.0 as u32;
            match cmd_id {
                MENU_EXIT_ID => crate::request_app_exit(app_handle),
                MENU_SETTINGS_ID => crate::window_manager::show_or_create_main_window(app_handle),
                _ => {}
            }
        }
    }
    IS_MENU_OPEN.store(false, Ordering::SeqCst);
}