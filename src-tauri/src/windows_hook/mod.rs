//! Windows 任务栏时钟区域：低级鼠标钩子、注册表时间格式、高亮覆盖层等。
//!
//! - `types`：钩子投递的事件类型
//! - `state`：跨模块共享的全局静态
//! - `window_utils`：窗口类名与桌面前台检测
//! - `clock_window`：时钟 HWND 查找与 UIA 矩形
//! - `registry_clock`：国际化注册表与自定义时钟文案
//! - `mouse_hook`：低级鼠标钩子与 `WindowsHookManager`

mod clock_window;
mod mouse_hook;
mod registry_clock;
mod state;
mod types;
mod window_utils;

// ---- 对外 API（与拆分前 `windows_hook` 根模块保持一致）----

/// 主任务栏几何信息与是否横向；供弹窗定位使用。
pub use clock_window::{
    find_clock_window, get_taskbar_info, is_mouse_in_clock_area, refresh_clock_rect_if_in_taskbar,
};
pub(crate) use clock_window::refresh_clock_area_cache;
/// 任务栏组件开关、钩子消息泵线程、钩子管理器、退出时显式卸钩。
pub use mouse_hook::{
    set_taskbar_widget_enabled, start_hook_message_thread, uninstall_global_mouse_hook,
    WindowsHookManager,
};
/// 注册表中的时间/日期格式与自定义任务栏文案。
pub use registry_clock::{
    disable_custom_clock, get_current_system_time_format, set_custom_clock_text,
};
/// 任务栏时钟区域矩形缓存（UIA 定位），供覆盖层贴合时钟使用。
pub use state::CLOCK_AREA_RECT_CACHE;
/// 任务栏右键菜单是否正在显示（防重复弹出）。
pub use state::IS_MENU_OPEN;
/// 原生 TrackPopupMenu 跟踪中标记。
pub use state::NATIVE_MENU_TRACKING;
/// 钩子向 Tokio 侧发送的点击事件与按键枚举。
pub use types::{ClickEvent, MouseButton};
/// 桌面前台检测（用于检测 WIN+D 后桌面显示状态）。
pub use window_utils::is_desktop_in_foreground;

/// 保存应用句柄（setup 时调用一次），供钩子回调内操作窗口使用。
pub fn set_app_handle(handle: tauri::AppHandle) {
    let _ = state::APP_HANDLE.set(handle);
}

/// 获取已保存的应用句柄副本。
pub fn app_handle() -> Option<tauri::AppHandle> {
    state::APP_HANDLE.get().cloned()
}
