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
    SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
};

use super::get_window_hwnd;

/// 最近一次应用到覆盖层的时钟矩形（变化检测：矩形没变就不动窗口，避免周期重贴抖动）。
static LAST_APPLIED_RECT: Mutex<Option<RECT>> = Mutex::new(None);
/// 任务栏「静止位」矩形（贴底/贴边时的位置）；跟随位移以此为基准计算偏移。
static TRAY_HOME_RECT: Mutex<Option<RECT>> = Mutex::new(None);
/// 最近一次跟随移动到的 y（相同则跳过 SetWindowPos，静止期零窗口操作）。
static LAST_FOLLOW_Y: Mutex<Option<i32>> = Mutex::new(None);

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
    // 可见性统一走跟随管理：覆盖层曾因矩形未就绪保持隐藏（如启动时任务栏正
    // 收起、UIA 探测失败），矩形就绪后由这里恢复显示或按全屏/滑出态维持隐藏。
    update_clock_overlay_visibility(app_handle);
}

/// 任务栏当前状态：是否完全滑出屏幕 + 相对静止位的位移（滑入/滑出动画的实时偏移）。
struct TrayState {
    /// 整体滑出所在显示器边缘（自动隐藏收起完成态）
    fully_hidden: bool,
    /// 相对静止位的双轴位移（静止时 (0,0)；滑出动画中 y>0——底部任务栏）
    dx: i32,
    dy: i32,
    /// 静止位基准是否已学习（未学习时不可跟随，覆盖层应保持隐藏）
    ready: bool,
}

/// 读取任务栏状态并顺带维护「静止位」基准。
fn read_tray_state() -> Option<TrayState> {
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if !IsWindowVisible(tray).as_bool() {
            return Some(TrayState { fully_hidden: true, dx: 0, dy: 0, ready: true });
        }
        let mut rect = RECT::default();
        if GetWindowRect(tray, &mut rect).is_err() {
            return None;
        }
        let hmon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        // 完全滑出判定：窗口整体在某边缘之外（留 ≤4px 唤出热区）
        const TOL: i32 = 4;
        let fully_hidden = rect.top >= m.bottom - TOL
            || rect.bottom <= m.top + TOL
            || rect.left >= m.right - TOL
            || rect.right <= m.left + TOL;
        // 静止位基准维护。两条铁律（皆有实测教训）：
        // 1) 不能用"贴住任一显示器边缘"判定静止——任务栏左右恒贴满屏幕宽，
        //    滑动全程都会误判静止、每帧刷新基准，dy 恒为 0，覆盖层不跟随。
        // 2) 首次记录不能用"无条件接受"——自动隐藏用户启动应用时任务栏多为
        //    收起态，收起位被记成静止位后，唤出时 dy=-82 覆盖层顶飞到屏幕中央。
        // 正确判定：静止位 ⇔ 任务栏矩形**完全在显示器内**（收起态与滑动中间态
        // 都必然跨出边缘）。基准记录后只在回到基准 ±TOL 时刷新；位移超过屏幕
        // 尺寸视为基准过期（分辨率变更），丢弃重学。
        let fully_inside = rect.left >= m.left
            && rect.right <= m.right
            && rect.top >= m.top
            && rect.bottom <= m.bottom;
        let screen_span = (m.right - m.left).abs().max((m.bottom - m.top).abs());
        let (dx, dy, ready) = if let Ok(mut home) = TRAY_HOME_RECT.lock() {
            match *home {
                Some(h) => {
                    let ddx = rect.left - h.left;
                    let ddy = rect.top - h.top;
                    if ddx.abs() > screen_span || ddy.abs() > screen_span {
                        // 基准过期（分辨率/显示器变更）：丢弃，等完全入屏后重学
                        *home = None;
                        (0, 0, false)
                    } else if ddx.abs() <= TOL && ddy.abs() <= TOL {
                        // 回到静止位：刷新基准（吸收静止位的微小漂移）
                        *home = Some(rect);
                        (0, 0, true)
                    } else {
                        // 滑动中：保持基准，返回实时位移
                        (ddx, ddy, true)
                    }
                }
                None => {
                    if fully_inside {
                        *home = Some(rect);
                        (0, 0, true)
                    } else {
                        (0, 0, false)
                    }
                }
            }
        } else {
            (0, 0, false)
        };
        Some(TrayState { fully_hidden, dx, dy, ready })
    }
}

/// Phase 1 可见性与跟随管理：全屏前台（游戏/视频）或任务栏完全滑出时隐藏；
/// 任务栏滑入/滑出动画期间**跟随其位移移动**（不是瞬间显隐——任务栏滑到哪
/// 覆盖层就在哪，视觉上像任务栏的一部分）。由 WinEvent 与 2s 兜底线程调用。
pub fn update_clock_overlay_visibility(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
        return;
    };
    let Some(hwnd) = get_window_hwnd(&window) else {
        return;
    };
    // 全屏前台：彻底隐藏（位置留给恢复时重算）
    if crate::windows_hook::is_foreground_fullscreen() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        if let Ok(mut last) = LAST_FOLLOW_Y.lock() {
            *last = None;
        }
        return;
    }
    let Some(state) = read_tray_state() else {
        return;
    };
    // 基准未学习（如启动时任务栏收起，尚未见过静止位）：保持隐藏，
    // 待任务栏完全入屏学到基准后再现身（避免无基准的"提前现身"）。
    if state.fully_hidden || !state.ready {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        if let Ok(mut last) = LAST_FOLLOW_Y.lock() {
            *last = None;
        }
        return;
    }
    // 跟随：覆盖层位置 = 缓存时钟矩形 + 任务栏当前位移（滑出动画中 dy 逐渐增大，
    // 覆盖层同步滑出屏外；滑入时同步滑回。SWP_NOZORDER 保持 topmost 不被重排）
    let Some(clock) = crate::windows_hook::CLOCK_AREA_RECT_CACHE
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().copied())
    else {
        return;
    };
    let x = clock.left + state.dx;
    let y = clock.top + state.dy;
    if let Ok(mut last) = LAST_FOLLOW_Y.lock() {
        if *last == Some(y) {
            return;
        }
        *last = Some(y);
    }
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
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

/// 从任务栏实采底色：采样**时钟矩形右侧、屏幕右缘之前的细条**（「显示桌面」区）。
///
/// 该区域没有任何图标/按钮像素，且紧邻时钟——底色与时钟局部最贴近。
/// 旧版采任务栏横向全宽中带，点位大量落在开始按钮/图标上，均值被污染产生色差。
/// 时钟矩形本身被覆盖层盖住（采到的是自己），也必须避开。
fn sample_taskbar_pixel() -> Option<(u8, u8, u8)> {
    unsafe {
        // 覆盖层贴合的时钟矩形：右缘之右即「显示桌面」细条
        let clock = crate::windows_hook::CLOCK_AREA_RECT_CACHE
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().copied())?;
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let mut tray_rect = RECT::default();
        GetWindowRect(tray, &mut tray_rect).ok()?;
        // 时钟所在显示器右缘（「显示桌面」条右边界）
        let hmon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return None;
        }
        let screen_right = mi.rcMonitor.right;
        // 条内取 3 个纵向位置、每列 3 点（上/中/下错开，抗单点噪点）
        let y_mid = (tray_rect.top + tray_rect.bottom) / 2;
        let y_top = tray_rect.top + (tray_rect.bottom - tray_rect.top) / 4;
        let y_bot = tray_rect.bottom - (tray_rect.bottom - tray_rect.top) / 4;
        let dc = GetDC(None);
        let mut sum = (0u32, 0u32, 0u32);
        let mut count = 0u32;
        for dx in [3i32, 8, 13] {
            let x = clock.right + dx;
            if x >= screen_right - 1 {
                break;
            }
            for y in [y_top, y_mid, y_bot] {
                let color = GetPixel(dc, x, y).0;
                if color == 0xFFFF_FFFF {
                    continue;
                }
                sum.0 += (color & 0xFF) as u32;
                sum.1 += ((color >> 8) & 0xFF) as u32;
                sum.2 += ((color >> 16) & 0xFF) as u32;
                count += 1;
            }
        }
        ReleaseDC(None, dc);
        if count < 3 {
            return None;
        }
        Some((
            (sum.0 / count) as u8,
            (sum.1 / count) as u8,
            (sum.2 / count) as u8,
        ))
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
