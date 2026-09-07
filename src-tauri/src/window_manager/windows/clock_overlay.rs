//! 任务栏时钟覆盖层（Phase 0 覆盖式接管）：不透明置顶窗口盖在系统时钟上，
//! 由前端 `ClockOverlayWindow` 渲染「时间 + 农历 + 天气」。
//!
//! 本阶段只接管「显示与悬停」：窗口不再鼠标穿透（`WS_EX_NOACTIVATE`，点击不抢
//! 前台焦点），悬停落在覆盖层上、系统时钟 XAML tooltip 从此不再触发；时钟区的
//! 点击仍由低级钩子整体吞掉（Phase 2 才交接输入），交互行为零变化。
//! 几何贴合复用 UIA 时钟矩形缓存；探测失败时保持隐藏——原生时钟兜底。
use std::sync::Mutex;
use tauri::{AppHandle, Manager, WebviewWindow};
use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetMonitorInfoW, GetPixel, MonitorFromWindow, ReleaseDC, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowLongPtrW, GetWindowRect, IsWindowVisible, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
};

use super::get_window_hwnd;

/// 最近一次应用到覆盖层的时钟矩形（变化检测：矩形没变就不动窗口，避免周期重贴抖动）。
static LAST_APPLIED_RECT: Mutex<Option<RECT>> = Mutex::new(None);

/// 构建任务栏时钟覆盖层窗口（独立、透明画布、置顶、无边框、隐藏待贴合）。
impl super::CalendarWindowManager {
    pub fn build_clock_overlay_window(app_handle: &AppHandle) -> Option<WebviewWindow> {
        let mut builder = tauri::WebviewWindowBuilder::new(
            app_handle,
            "clock_overlay",
            tauri::WebviewUrl::App("index.html?window=clock_overlay".into()),
        );
        // 固定 WebView2 用户数据目录（主窗口数据同目录下子目录），
        // 避免非主窗口默认写入 %TEMP% 导致每次启动堆积 ~68MB EBWebView。
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
            // 先隐藏，探测到时钟矩形并贴合后再无激活显示；探测失败保持隐藏（原生时钟兜底）
            .visible(false)
            .focused(false)
            .skip_taskbar(true)
            .shadow(false)
            .build()
            .ok()?;
        // 覆盖式接管：不再鼠标穿透——悬停落在覆盖层，系统时钟 tooltip 不再触发；
        // 点击仍被钩子整体吞掉（Phase 2 才交接）。NOACTIVATE 保证收到输入也不抢前台焦点，
        // TOOLWINDOW 避免 Alt+Tab 出现覆盖层条目。
        if let Some(hwnd) = get_window_hwnd(&window) {
            unsafe {
                let style = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
                let _ = SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    (style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0 as isize,
                );
            }
        }
        Some(window)
    }
}

/// 供低级钩子等模块获取指定标签窗口的 Win32 句柄。
/// （时钟点击判定使用覆盖窗口矩形：可见的自定义时钟在哪里，点击区就在哪里。）
pub fn window_hwnd_by_label(app_handle: &AppHandle, label: &str) -> Option<HWND> {
    app_handle.get_webview_window(label).and_then(|w| get_window_hwnd(&w))
}

/// Phase 0 启动入口：构建覆盖层（隐藏）并等待 UIA 时钟矩形就绪后贴合显示。
///
/// UIA 初始化在钩子消息线程上，时序可能晚于此处；每 500ms 重试、最多 10 秒。
/// 已存在窗口时（重复调用/心跳续跑）直接重贴，不重复建窗。
pub fn ensure_clock_overlay_attached(app_handle: &AppHandle) {
    // UIA 时钟矩形探测依赖 COM（与钩子消息线程同要求），本线程必须自行初始化
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let window = match app_handle.get_webview_window("clock_overlay") {
        Some(existing) => existing,
        None => {
            let Some(built) = super::CalendarWindowManager::build_clock_overlay_window(app_handle)
            else {
                crate::dbg_log("clock overlay: build_clock_overlay_window failed");
                return;
            };
            built
        }
    };
    for attempt in 0..20 {
        crate::windows_hook::refresh_clock_area_cache();
        let rect = crate::windows_hook::CLOCK_AREA_RECT_CACHE
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().copied());
        if let Some(rect) = rect {
            if apply_overlay_geometry(&window, &rect) {
                if let Some(hwnd) = get_window_hwnd(&window) {
                    show_overlay_above_taskbar(hwnd);
                    crate::dbg_log(&format!(
                        "clock overlay: attached at attempt {attempt} rect=({},{})-({},{})",
                        rect.left, rect.top, rect.right, rect.bottom
                    ));
                    return;
                }
            }
        }
        if attempt == 3 || attempt == 10 {
            crate::dbg_log(&format!(
                "clock overlay: rect not ready at attempt {attempt}, retrying"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!("clock overlay: 10s 内未探测到时钟矩形，覆盖层保持隐藏（原生时钟兜底）");
    crate::dbg_log("clock overlay: gave up after 10s, staying hidden");
}

/// 刷新时钟矩形缓存并重贴（前端命令 / 开关重开路径用）。
pub fn relocate_clock_overlay(app_handle: &AppHandle) {
    crate::windows_hook::refresh_clock_area_cache();
    relocate_clock_overlay_from_cache(app_handle);
}

/// 直接按缓存矩形重贴（不再触发 UIA，供 2 秒周期重探线程复用其刷新结果）。
pub fn relocate_clock_overlay_from_cache(app_handle: &AppHandle) {
    let rect = crate::windows_hook::CLOCK_AREA_RECT_CACHE
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().copied());
    let Some(rect) = rect else { return };
    let Some(window) = app_handle.get_webview_window("clock_overlay") else { return };
    if !apply_overlay_geometry(&window, &rect) {
        return;
    }
    if let Some(hwnd) = get_window_hwnd(&window) {
        // 重贴同时重申 topmost（不激活、不改可见性）：任务栏若重新声明过
        // topmost 会排到覆盖层之上，周期性压一次保证覆盖层始终可见。
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// 任务栏是否处于用户可见状态（自动隐藏滑出屏幕视为不可见）。
fn taskbar_visible() -> bool {
    unsafe {
        let Ok(tray) = FindWindowW(w!("Shell_TrayWnd"), None) else {
            return false;
        };
        if !IsWindowVisible(tray).as_bool() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(tray, &mut rect).is_err() {
            return false;
        }
        // 自动隐藏态：任务栏窗口仍"可见"但整体滑出所在显示器边缘（留 1~2px 唤出热区）
        let hmon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return true;
        }
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return true;
        }
        let m = mi.rcMonitor;
        const TOL: i32 = 4;
        !(rect.top >= m.bottom - TOL
            || rect.bottom <= m.top + TOL
            || rect.left >= m.right - TOL
            || rect.right <= m.left + TOL)
    }
}

/// Phase 1 可见性管理：全屏前台（游戏/视频）或任务栏自动隐藏时隐藏覆盖层，
/// 恢复后自动显示。由 2s 重探线程周期调用；显隐均走无激活路径，绝不抢焦点。
pub fn update_clock_overlay_visibility(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
        return;
    };
    let Some(hwnd) = get_window_hwnd(&window) else {
        return;
    };
    let should_show = !crate::windows_hook::is_foreground_fullscreen() && taskbar_visible();
    unsafe {
        let _ = ShowWindow(hwnd, if should_show { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

/// 按时钟矩形贴合覆盖层（尺寸 + 位置）；矩形与上次一致时跳过重设。
fn apply_overlay_geometry(window: &WebviewWindow, rect: &RECT) -> bool {
    let w = (rect.right - rect.left) as u32;
    let h = (rect.bottom - rect.top) as u32;
    if w == 0 || h == 0 {
        return false;
    }
    if let Ok(mut last) = LAST_APPLIED_RECT.lock() {
        if let Some(prev) = *last {
            if prev.left == rect.left
                && prev.top == rect.top
                && prev.right == rect.right
                && prev.bottom == rect.bottom
            {
                return true;
            }
        }
        *last = Some(*rect);
    }
    let _ = window.set_size(tauri::PhysicalSize { width: w, height: h });
    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
        x: rect.left,
        y: rect.top,
    }));
    true
}

/// 无激活显示并把覆盖层压到任务栏之上（同为 topmost 组内，后声明者在上）。
fn show_overlay_above_taskbar(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// 读取任务栏主题模式（`SystemUsesLightTheme`=1 为浅色——任务栏跟随**系统**模式，
/// 不读 `AppsUseLightTheme`，两者可分别设置）；读取失败按浅色处理。
fn system_uses_light_theme() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return true;
        }
        let mut value: u32 = 1;
        let mut len = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            w!("SystemUsesLightTheme"),
            None,
            None,
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut len),
        )
        .is_ok();
        let _ = RegCloseKey(key);
        ok && value == 1
    }
}

/// 从任务栏实采底色：水平 5 点采样取**通道均值**。任务栏底色沿横向有壁纸
/// 透出的微妙渐变（实测 5 点 5 值），精确众数永不成立，必须取均值。
/// 采样点全失败返回 None（调用方按主题色兜底）。
fn sample_taskbar_pixel() -> Option<(u8, u8, u8)> {
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let mut rect = RECT::default();
        GetWindowRect(tray, &mut rect).ok()?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return None;
        }
        let dc = GetDC(None);
        let mut sum = (0u32, 0u32, 0u32);
        let mut count = 0u32;
        for frac in [0.12f64, 0.3, 0.5, 0.7, 0.88] {
            let x = rect.left + (width as f64 * frac) as i32;
            let y = rect.top + height / 2;
            let color = GetPixel(dc, x, y).0;
            if color == 0xFFFF_FFFF {
                continue;
            }
            sum.0 += (color & 0xFF) as u32;
            sum.1 += ((color >> 8) & 0xFF) as u32;
            sum.2 += ((color >> 16) & 0xFF) as u32;
            count += 1;
        }
        ReleaseDC(None, dc);
        (count > 0).then(|| {
            (
                (sum.0 / count) as u8,
                (sum.1 / count) as u8,
                (sum.2 / count) as u8,
            )
        })
    }
}

/// 覆盖层外观：任务栏底色（实采优先，主题注册表兜底）+ 按亮度选择的对比前景色。
/// 返回 `(背景 hex, 前景 hex)`。绝不返回透明——透底叠字是覆盖式方案的头号风险。
pub fn clock_overlay_appearance_colors() -> (String, String) {
    let (bg, source) = match sample_taskbar_pixel() {
        Some(sampled) => (sampled, "sampled"),
        None => {
            let light = system_uses_light_theme();
            crate::dbg_log(&format!(
                "clock overlay appearance: pixel sampling failed, fallback theme light={light}"
            ));
            (
                if light { (0xF3, 0xF3, 0xF3) } else { (0x20, 0x20, 0x20) },
                "theme",
            )
        }
    };
    let luminance = 0.2126 * bg.0 as f64 + 0.7152 * bg.1 as f64 + 0.0722 * bg.2 as f64;
    let fg = if luminance > 128.0 { "#1a1a1a" } else { "#ffffff" };
    crate::dbg_log(&format!(
        "clock overlay appearance bg=#{:02X}{:02X}{:02X} ({source}) fg={fg}",
        bg.0, bg.1, bg.2
    ));
    (format!("#{:02X}{:02X}{:02X}", bg.0, bg.1, bg.2), fg.to_string())
}
