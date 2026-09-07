//! 日历相关窗口的创建、定位与生命周期：按平台选择 `windows` 或 `macos` 实现。
#[cfg(target_os = "macos")]
#[path = "window_manager/macos/popup.rs"]
mod macos_popup;
#[path = "window_manager/shared.rs"]
pub(crate) mod shared;
#[cfg(windows)]
#[path = "window_manager/windows.rs"]
mod windows;

#[cfg(windows)]
pub use windows::main_window::show_or_create_main_window;
#[cfg(windows)]
pub use windows::relocate_clock_overlay;
#[cfg(windows)]
pub use windows::relocate_clock_overlay_from_cache;
#[cfg(windows)]
pub use windows::clock_overlay_appearance_colors;
#[cfg(windows)]
pub use windows::ensure_clock_overlay_attached;
#[cfg(windows)]
pub use windows::update_clock_overlay_visibility;
#[cfg(windows)]
pub use windows::{
    dismiss_native_menu_from_hook, hide_clock_context_menu, hide_from_hook, menu_rect_contains,
    native_menu_rect, native_menu_rect_contains, precreate_clock_context_menu,
    show_clock_context_menu, track_native_clock_menu, window_hwnd_by_label,
};
#[cfg(windows)]
pub use windows::CalendarWindowManager;

#[cfg(target_os = "macos")]
pub use macos_popup::{show_or_create_main_window, CalendarWindowManager};
