//! 任务栏时钟窗口查找、矩形探测与点击区域判定。

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::state::CLOCK_AREA_RECT_CACHE;
use super::window_utils::get_window_class_name;

/// 通过 UI Automation 在 Shell 托盘树上查找时钟控件屏幕矩形。
pub fn get_clock_rect_via_uia() -> Option<RECT> {
    unsafe {
        // 优先：按时钟窗口 HWND + GetWindowRect 获取【物理像素】矩形。
        // 低级鼠标钩子 lParam 传的是【物理屏幕坐标】，而 UIA BoundingRectangle 在 DPI 缩放下
        // 常返回【逻辑坐标】（本机 4K/175% 下实测为 (2085,1186)，正等于 3840/1.75、2160/1.75）。
        // 两套坐标不一致会导致 is_mouse_in_clock_area 永不命中（右键/左键任务栏时钟均无反应）。
        if let Some(hwnd) = find_clock_window() {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok()
                && rect.right > rect.left
                && rect.bottom > rect.top
            {
                return Some(rect);
            }
        }

        // 原 UIA 兜底（逻辑坐标，仅在高 DPI 下会错位——已被上面物理优先覆盖）。
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let hwnd_tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let tray_element = automation.ElementFromHandle(hwnd_tray).ok()?;

        // 优先按 AutomationId「ClockButton」（Win10/11 常见）
        let automation_id_condition = automation
            .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("ClockButton"))
            .ok();
        if let Some(condition) = automation_id_condition {
            if let Ok(clock_element) = tray_element.FindFirst(TreeScope_Descendants, &condition) {
                if let Ok(rect) = clock_element.CurrentBoundingRectangle() {
                    return Some(rect);
                }
            }
        }

        // 再按类名枚举若干候选（含 Win11 OmniButton 等）
        let class_names = ["ClockButton", "TrayClockWClass", "SystemTray.OmniButton"];
        for class_name in class_names {
            let Ok(condition) = automation
                .CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(class_name))
            else {
                continue;
            };
            if let Ok(clock_element) = tray_element.FindFirst(TreeScope_Descendants, &condition) {
                if let Ok(rect) = clock_element.CurrentBoundingRectangle() {
                    return Some(rect);
                }
            }
        }

        None
    }
}

/// 将 UIA 得到的时钟矩形写入 [`super::state::CLOCK_AREA_RECT_CACHE`]。
pub fn update_clock_area_cache() {
    if let Some(rect) = get_clock_rect_via_uia() {
        if let Ok(mut w) = CLOCK_AREA_RECT_CACHE.write() {
            *w = Some(rect);
        }
    }
}

/// 在非钩子线程上刷新时钟区域缓存（先 `CoInitializeEx`，再 UIA）。
///
/// 供 Tauri 命令等在任务栏时钟外观变更后重新探测矩形；钩子消息泵线程可直接调 [`update_clock_area_cache`]。
pub fn refresh_clock_area_cache() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    update_clock_area_cache();
}

/// 供 [`super::mouse_hook`] 使用：判断 `(x, y)` 是否落在缓存的时钟矩形外扩 2 像素内。
///
/// * `x` / `y` - 屏幕物理像素坐标。
///
/// 仅读取 [`super::state::CLOCK_AREA_RECT_CACHE`]（安装钩子或应用/恢复自定义时钟后写入），不做 UIA（钩子回调必须极快返回）。
pub fn is_mouse_in_clock_area(x: i32, y: i32) -> bool {
    let Ok(cache) = CLOCK_AREA_RECT_CACHE.read() else {
        return false;
    };
    let Some(rect) = cache.as_ref() else {
        return false;
    };
    x >= rect.left - 2 && x <= rect.right + 2 && y >= rect.top - 2 && y <= rect.bottom + 2
}

/// 返回主任务栏的屏幕矩形以及是否为横向任务栏。
///
/// 返回值：`(任务栏矩形, 是否横向)`；横向指长边在水平方向。
pub fn get_taskbar_info() -> Option<(RECT, bool)> {
    unsafe {
        let taskbar = FindWindowW(windows::core::w!("Shell_TrayWnd"), None).ok()?;
        if taskbar.0.is_null() {
            return None;
        }

        let mut rect = RECT::default();
        if GetWindowRect(taskbar, &mut rect).is_err() {
            return None;
        }

        // 预留：可按屏幕尺寸辅助判断任务栏停靠边（当前仅用矩形宽高比）
        let _screen_width = GetSystemMetrics(SM_CXSCREEN);
        let _screen_height = GetSystemMetrics(SM_CYSCREEN);

        let is_horizontal = (rect.right - rect.left) > (rect.bottom - rect.top);

        Some((rect, is_horizontal))
    }
}

/// 按任务栏层级（`Shell_TrayWnd` → `TrayNotifyWnd` → 时钟类）或全局枚举查找时钟 HWND。
pub fn find_clock_window() -> Option<HWND> {
    unsafe {
        const CLOCK_CLASSES: [&str; 3] = ["ClockButton", "TrayClockWClass", "SystemTray.OmniButton"];

        // 方案 1：沿经典 Shell 托盘层级向下 `FindWindowExW`
        if let Ok(shell_tray) = FindWindowW(windows::core::w!("Shell_TrayWnd"), None) {
            if !shell_tray.0.is_null() {
                if let Ok(tray_notify) =
                    FindWindowExW(Some(shell_tray), None, windows::core::w!("TrayNotifyWnd"), None)
                {
                    if !tray_notify.0.is_null() {
                        for class_name in CLOCK_CLASSES {
                            let wide: Vec<u16> = class_name.encode_utf16().collect();
                            let Ok(clock_btn) = FindWindowExW(
                                Some(tray_notify),
                                None,
                                PCWSTR(wide.as_ptr()),
                                None,
                            ) else {
                                continue;
                            };
                            if !clock_btn.0.is_null() {
                                return Some(clock_btn);
                            }
                        }
                    }
                }
            }
        }

        // 方案 2：枚举顶层窗口找到主任务栏再枚举子窗口（部分 Win11 变体）
        let mut result_hwnd = HWND::default();
        let _ =
            EnumWindows(Some(enum_windows_proc), LPARAM(&mut result_hwnd as *mut HWND as isize));

        if !result_hwnd.0.is_null() {
            return Some(result_hwnd);
        }

        // 多显示器次要任务栏：当前仅占位，后续可扩展相同时钟查找逻辑
        if let Ok(sec_tray) = FindWindowW(windows::core::w!("Shell_SecondaryTrayWnd"), None) {
            if !sec_tray.0.is_null() {
                let _ = sec_tray;
            }
        }

        None
    }
}

/// 顶层枚举回调：找到 `Shell_TrayWnd` 后对其子窗口继续枚举以定位时钟。
unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let class_name_str = get_window_class_name(hwnd);
    if class_name_str == "Shell_TrayWnd" {
        // `lparam` 指向调用方栈上的 `HWND` 输出槽位
        let result_ptr = lparam.0 as *mut HWND;
        let mut found_hwnd = HWND::default();
        let _ = EnumChildWindows(
            Some(hwnd),
            Some(enum_child_windows_proc),
            LPARAM(&mut found_hwnd as *mut HWND as isize),
        );
        if !found_hwnd.0.is_null() {
            *result_ptr = found_hwnd;
            return FALSE;
        }
    }
    TRUE
}

/// 子窗口枚举回调：匹配时钟按钮或经典 `TrayClockWClass` 即写入结果并停止。
unsafe extern "system" fn enum_child_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let class_name_str = get_window_class_name(hwnd);
    if class_name_str == "ClockButton"
        || class_name_str == "TrayClockWClass"
        || class_name_str == "SystemTray.OmniButton"
    {
        let result_ptr = lparam.0 as *mut HWND;
        *result_ptr = hwnd;
        return FALSE;
    }
    TRUE
}