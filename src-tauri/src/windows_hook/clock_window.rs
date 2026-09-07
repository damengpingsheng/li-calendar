//! 任务栏时钟窗口查找、矩形探测与点击区域判定。

use std::ffi::c_void;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::state::CLOCK_AREA_RECT_CACHE;
use super::window_utils::get_window_class_name;

/// 时钟矩形诊断日志：存在标记文件 `D:\agents_tmp\clockrect_debug` 时才写
/// （周期探测每 2s 一条，常开会刷爆 menu_dbg.log）。
fn clockrect_log(msg: &str) {
    if std::path::Path::new(r"D:\agents_tmp\clockrect_debug").exists() {
        crate::dbg_log(&format!("clockrect: {msg}"));
    }
}

/// 校验/修正时钟矩形：物理矩形必须完整落在任务栏带内（允许 20px 容差，越界部分收拢）。
///
/// UIA `BoundingRectangle` 在进程 DPI 感知与系统不匹配时会返回**逻辑坐标**
/// （本机 4K/175% 实测约 (2085,1186)），与物理坐标差一个缩放系数——
/// 按系统 DPI 缩放修正后若能落入任务栏带则采用修正值；仍不符则返回 None。
unsafe fn validate_or_scale_clock_rect(rect: RECT) -> Option<RECT> {
    let Some((taskbar, _)) = get_taskbar_info() else {
        return Some(rect);
    };
    const TOL: i32 = 20;
    let fits = |r: &RECT| {
        r.right > r.left
            && r.bottom > r.top
            && r.left >= taskbar.left - TOL
            && r.right <= taskbar.right + TOL
            && r.top >= taskbar.top - TOL
            && r.bottom <= taskbar.bottom + TOL
    };
    let clamp = |mut r: RECT| {
        r.left = r.left.clamp(taskbar.left, taskbar.right);
        r.right = r.right.clamp(r.left, taskbar.right);
        r.top = r.top.clamp(taskbar.top, taskbar.bottom);
        r.bottom = r.bottom.clamp(r.top, taskbar.bottom);
        r
    };
    if fits(&rect) {
        return Some(clamp(rect));
    }
    let dpi = GetDpiForSystem();
    let scale = dpi as f32 / 96.0;
    if scale <= 0.01 {
        return None;
    }
    let to_i32 = |v: f32| v as i32;
    let scaled = RECT {
        left: to_i32(rect.left as f32 * scale),
        top: to_i32(rect.top as f32 * scale),
        right: to_i32(rect.right as f32 * scale),
        bottom: to_i32(rect.bottom as f32 * scale),
    };
    if fits(&scaled) {
        Some(clamp(scaled))
    } else {
        None
    }
}

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
                if let Some(valid) = validate_or_scale_clock_rect(rect) {
                    return Some(valid);
                }
            }
        }

        // 原 UIA 兜底（返回值经 validate_or_scale_clock_rect 校验/DPI 修正）。
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
                    if let Some(valid) = validate_or_scale_clock_rect(rect) {
                        return Some(valid);
                    }
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
                    if let Some(valid) = validate_or_scale_clock_rect(rect) {
                        return Some(valid);
                    }
                }
            }
        }

        None
    }
}

/// 将 UIA 得到的时钟矩形写入 [`super::state::CLOCK_AREA_RECT_CACHE`]。
pub fn update_clock_area_cache() {
    if let Some(rect) = get_clock_rect_via_uia() {
        clockrect_log(&format!(
            "uia rect=({},{},{},{})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
        persist_clock_rect(&rect);
        if let Ok(mut w) = CLOCK_AREA_RECT_CACHE.write() {
            *w = Some(rect);
        }
        return;
    }
    // 探测失败（Win11 XAML 任务栏可能无常驻经典时钟窗口）：
    // 回退到持久化的最后已知正确物理矩形（注册表 HKCU\Software\liCalendar\ClockRect）。
    if let Some(rect) = load_persisted_clock_rect() {
        clockrect_log(&format!(
            "registry-echo rect=({},{},{},{})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
        if let Ok(mut w) = CLOCK_AREA_RECT_CACHE.write() {
            *w = Some(rect);
        }
    }
}

/// 持久化最后已知正确的时钟物理矩形（HKCU\Software\liCalendar\ClockRect，"l,t,r,b"）。
fn persist_clock_rect(rect: &RECT) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\liCalendar"),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            None,
            &mut key,
            None,
        ) != WIN32_ERROR(0)
        {
            return;
        }
        let text = format!("{},{},{},{}", rect.left, rect.top, rect.right, rect.bottom);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let _ = RegSetValueExW(
            key,
            w!("ClockRect"),
            Some(0),
            REG_SZ,
            Some(std::slice::from_raw_parts(
                wide.as_ptr().cast::<u8>(),
                wide.len() * 2,
            )),
        );
        let _ = RegCloseKey(key);
    }
}

/// 读取持久化的时钟矩形（读出后经任务栏带校验/修正）。
fn load_persisted_clock_rect() -> Option<RECT> {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Software\\liCalendar"), Some(0), KEY_READ, &mut key)
            != WIN32_ERROR(0)
        {
            return None;
        }
        let mut buf = [0u16; 64];
        let mut size = (buf.len() * 2) as u32;
        let mut kind = REG_VALUE_TYPE::default();
        if RegQueryValueExW(
            key,
            w!("ClockRect"),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        ) != WIN32_ERROR(0)
        {
            let _ = RegCloseKey(key);
            return None;
        }
        let _ = RegCloseKey(key);
        let len = (size as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..len]);
        let parts: Vec<i32> = text
            .trim_end_matches('\0')
            .split(',')
            .filter_map(|p| p.trim().parse::<i32>().ok())
            .collect();
        if parts.len() != 4 {
            return None;
        }
        validate_or_scale_clock_rect(RECT {
            left: parts[0],
            top: parts[1],
            right: parts[2],
            bottom: parts[3],
        })
    }
}

/// 时钟窗口句柄缓存（isize 形式；配合 IsWindow 校验，避免每次点击都全量枚举窗口）。
static CLOCK_HWND_CACHE: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

/// 钩子下探使用：点击位于任务栏带内时，轻量刷新时钟矩形缓存。
///
/// 任务栏会在自定义时钟文本应用、图标增减等时机重排，时钟矩形随之变化——
/// 启动时探测的缓存矩形会过期（表现为时钟右键/左键无反应、弹原生菜单）。
/// 此函数仅做 IsWindow 校验 + GetWindowRect（微秒级、无 UIA），钩子回调可安全调用。
pub fn refresh_clock_rect_if_in_taskbar(x: i32, y: i32) {
    let Some((taskbar, _)) = get_taskbar_info() else {
        return;
    };
    if x < taskbar.left || x > taskbar.right || y < taskbar.top || y > taskbar.bottom {
        return;
    }
    unsafe {
        let hwnd = {
            let cached = CLOCK_HWND_CACHE.lock().ok().and_then(|g| *g);
            match cached {
                Some(h) if IsWindow(Some(HWND(h as *mut c_void))).as_bool() => {
                    HWND(h as *mut c_void)
                }
                _ => {
                    let found = find_clock_window().unwrap_or_default();
                    if !found.0.is_null() {
                        if let Ok(mut g) = CLOCK_HWND_CACHE.lock() {
                            *g = Some(found.0 as isize);
                        }
                    }
                    found
                }
            }
        };
        if hwnd.0.is_null() {
            return;
        }
        let mut rect = RECT::default();
        let ok = GetWindowRect(hwnd, &mut rect).is_ok()
            && rect.right > rect.left
            && rect.bottom > rect.top
            && validate_or_scale_clock_rect(rect).is_some();
        if !ok {
            return;
        }
        clockrect_log(&format!(
            "hook-hwnd rect=({},{},{},{})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
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