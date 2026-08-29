//! 任务栏时钟覆盖层：透明置顶窗口盖在系统时钟上，显示「时间 + 农历 + 天气」。
//!
//! 系统时钟格式串（`sShortDate` / `sShortTime`）不支持农历，因此这里改用覆盖层方案：
//! 后端把透明窗口贴合到任务栏时钟矩形、并让鼠标穿透（点击落到系统时钟→由低级鼠标钩子
//! 处理弹日历/右键菜单），前端 `ClockOverlayWindow` 负责渲染农历与天气。
use tauri::{AppHandle, Manager, WebviewWindow};

/// 构建任务栏时钟覆盖层窗口（独立、透明、置顶、无边框、忽略鼠标）。
impl super::CalendarWindowManager {
    pub fn build_clock_overlay_window(app_handle: &AppHandle) -> Option<WebviewWindow> {
        let mut builder = tauri::WebviewWindowBuilder::new(
            app_handle,
            "clock_overlay",
            tauri::WebviewUrl::App("index.html?window=clock_overlay".into()),
        );
        // 固定 WebView2 用户数据目录（主窗口数据同目录下子目录），
        // 避免非主窗口默认写入 %TEMP% 导致每次启动堆积 ~68MB EBWebView。
        // WebView2 用户数据固定到 D 盘，不再写 C 盘。
        builder = builder.data_directory(std::path::PathBuf::from(
            r"D:\Program Files\li-calendar\webview-data\clock-overlay",
        ));
        let window = builder
        .title("")
        .inner_size(170.0, 52.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .visible(true)
        .focused(false)
        .skip_taskbar(true)
        .shadow(false)
        .build()
        .ok()?;
        // 鼠标穿透：点击/右键落到系统时钟，交由 WH_MOUSE_LL 钩子处理（弹日历/右键菜单），
        // 同时避免覆盖层自身抢焦点。
        let _ = window.set_always_on_top(true);
        let _ = window.set_ignore_cursor_events(true);
        Some(window)
    }
}

/// 重新将覆盖层窗口贴合到任务栏时钟矩形（UIA 定位），并调整尺寸覆盖原时钟文字。
pub fn relocate_clock_overlay(app_handle: &AppHandle) {
    crate::windows_hook::refresh_clock_area_cache();
    let Ok(cache) = crate::windows_hook::CLOCK_AREA_RECT_CACHE.read() else {
        return;
    };
    let Some(rect) = cache.as_ref() else {
        return;
    };
    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
        return;
    };
    let w = (rect.right - rect.left) as u32;
    let h = (rect.bottom - rect.top) as u32;
    if w == 0 || h == 0 {
        return;
    }
    let _ = window.set_size(tauri::PhysicalSize { width: w, height: h });
    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
        x: rect.left,
        y: rect.top,
    }));
    let _ = window.set_always_on_top(true);
}
