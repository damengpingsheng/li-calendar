//! 任务栏低级鼠标钩子与点击事件分发。
use crate::window_manager::CalendarWindowManager;
use crate::windows_hook::{
    is_desktop_in_foreground, start_hook_message_thread, ClickEvent, MouseButton,
    WindowsHookManager,
};
use crate::AppState;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use tokio::sync::mpsc;

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
        match click_event.button {
            MouseButton::Left => {
                // WIN+D / 显示桌面后桌面日历可能被最小化：任意桌面左键点击时恢复。
                // `ensure_desktop_widget_on_desktop` 仅在窗口最小化时做事，可见时零开销。
                if is_desktop_in_foreground() {
                    if let Ok(mut window_manager_guard) = window_manager.lock() {
                        if let Some(calendar_window_manager) = window_manager_guard.as_mut() {
                            calendar_window_manager.ensure_desktop_widget_on_desktop();
                        }
                    }
                }
                if click_event.in_clock_area {
                    handle_left_click(&window_manager, click_event.x, click_event.y);
                }
            }
            MouseButton::Right => {
                if !click_event.in_clock_area {
                    continue;
                }
                // 隔离实验开关：存在标记文件时不弹菜单（仅记录事件）。
                if std::path::Path::new(r"D:\agents_tmp\ccm_disabled").exists() {
                    crate::dbg_log("right click (menu suppressed by experiment A)");
                    continue;
                }
                crate::dbg_log(&format!(
                    "right click received at ({}, {})",
                    click_event.x, click_event.y
                ));
                // 菜单实现双模式：默认原生 TrackPopupMenu（系统风格）；
                // 存在 `ccm_tauri_menu` 标记文件时改用 Tauri 置顶窗口菜单（备用）。
                let use_tauri_menu =
                    std::path::Path::new(r"D:\agents_tmp\ccm_tauri_menu").exists();
                if use_tauri_menu {
                    if let Err(error) = crate::window_manager::show_clock_context_menu(
                        &app_handle,
                        click_event.x,
                        click_event.y,
                    ) {
                        eprintln!("显示时钟右键菜单失败: {error}");
                    }
                } else if let Some(action) = crate::window_manager::track_native_clock_menu(
                    click_event.x,
                    click_event.y,
                ) {
                    match action {
                        "exit" => crate::request_app_exit(&app_handle),
                        "settings" => {
                            crate::window_manager::show_or_create_main_window(&app_handle)
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// 处理左键点击事件：只保留一个日历，优先切换桌面日历的显示/隐藏，
/// 不再在时钟旁弹出第二个月历；桌面组件未启用时回退为弹窗切换。
fn handle_left_click(
    window_manager: &Arc<Mutex<Option<CalendarWindowManager>>>,
    _click_x: i32,
    _click_y: i32,
) {
    if let Ok(mut window_manager_guard) = window_manager.lock() {
        if let Some(calendar_window_manager) = window_manager_guard.as_mut() {
            if let Err(error) = calendar_window_manager.toggle_clock_calendar() {
                eprintln!("❌ 切换日历窗口失败: {}", error);
            }
        }
    }
}
