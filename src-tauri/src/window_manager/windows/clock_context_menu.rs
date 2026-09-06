//! 任务栏时钟右键菜单窗口：置顶小窗展示「设置 / 退出」。
//!
//! 早期实现使用 Win32 `TrackPopupMenu`：需要属主窗口、前台权限（ALT 解锁）、
//! 模态循环与低级钩子放行配合，极易因前台竞态出现闪退/无响应，且
//! `AttachThreadInput` 会在进程退出时污染 explorer 线程输入队列（桌面左键失灵）。
//! 改用普通置顶 Tauri 窗口后，点击直接落在可见窗口上，不再依赖前台与队列状态。
//!
//! 隐藏时机（刻意设计）：**不因失焦隐藏**。系统或安全软件可能在菜单显示后
//! 数百毫秒内抢走焦点（真实用户环境已实测复现），若失焦即隐藏，用户的点击会
//! 落在已被隐藏的空处——表现为「点退出没反应」。菜单保持可见，直到：
//! 动作执行 / Esc / 点击菜单外部（由低级钩子判定并隐藏，点击同时放行给下层）。
use crate::windows_hook::IS_MENU_OPEN;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::RwLock;
use tauri::{AppHandle, Manager, WebviewWindow};
use windows::core::BOOL;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LRESULT, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, ReleaseCapture, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, EnumWindows,
    FindWindowW, GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId,
    HWND_TOPMOST, IsWindowVisible, MF_STRING, PostMessageW, RegisterClassW,
    SetForegroundWindow, SetWindowPos, ShowWindow, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA,
    SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW, TPM_BOTTOMALIGN, TPM_CENTERALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WINDOW_EX_STYLE, WM_CANCELMODE, WNDCLASSW, WS_POPUP,
};

/// 菜单窗口逻辑尺寸（与前端样式保持一致）。项目做大以保证点击命中宽容度。
const MENU_WIDTH: f64 = 150.0;
const MENU_HEIGHT: f64 = 112.0;

/// 原生菜单命令 ID（与旧 TrackPopupMenu 实现一致）。
const MENU_SETTINGS_ID: u32 = 1001;
const MENU_EXIT_ID: u32 = 1002;

/// 菜单当前屏幕矩形（物理坐标 x1,y1,x2,y2），供低级钩子判定「菜单外点击」。
/// `None` 表示菜单未显示。
static MENU_RECT: RwLock<Option<(i32, i32, i32, i32)>> = RwLock::new(None);

/// 供低级钩子查询：某物理坐标是否落在菜单窗口内。
pub fn menu_rect_contains(x: i32, y: i32) -> bool {
    match MENU_RECT.read().ok().and_then(|guard| *guard) {
        Some((x1, y1, x2, y2)) => x >= x1 && x < x2 && y >= y1 && y < y2,
        None => false,
    }
}

/// 供低级钩子调用：隐藏菜单（点击落在菜单外时）。
pub fn hide_from_hook() {
    match crate::windows_hook::app_handle() {
        Some(app_handle) => hide_clock_context_menu(&app_handle),
        None => {
            // 没有应用句柄时仅复位放行状态与菜单矩形，避免钩子持续放行。
            IS_MENU_OPEN.store(false, Ordering::SeqCst);
            MENU_RECT.write().map(|mut guard| *guard = None).ok();
        }
    }
}

/// 确保菜单窗口存在（启动时预创建，之后复用隐藏实例）。
fn ensure_window(app_handle: &AppHandle) -> Option<WebviewWindow> {
    if let Some(window) = app_handle.get_webview_window("clock_context_menu") {
        return Some(window);
    }
    let mut builder = tauri::WebviewWindowBuilder::new(
        app_handle,
        "clock_context_menu",
        tauri::WebviewUrl::App("index.html?window=clock_context_menu".into()),
    );
    // 固定 WebView2 用户数据目录，与其他窗口保持同一策略。
    builder = builder.data_directory(std::path::PathBuf::from(
        r"D:\Program Files\li-calendar\webview-data\clock-context-menu",
    ));
    let window = builder
        .title("")
        .inner_size(MENU_WIDTH, MENU_HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .shadow(false)
        .build()
        .ok()?;
    Some(window)
}

/// 应用启动时预创建并预加载菜单窗口（隐藏状态）。
/// 否则首次右键时才创建 WebView，加载的一秒内点击无效。
/// 仅 Tauri 菜单模式需要；原生菜单模式跳过（无 WebView 加载问题）。
pub fn precreate_clock_context_menu(app_handle: &AppHandle) {
    if !std::path::Path::new(r"D:\agents_tmp\ccm_tauri_menu").exists() {
        return;
    }
    let _ = ensure_window(app_handle);
}

/// 供低级钩子调用：以 `WM_CANCELMODE` 关闭正在跟踪的原生菜单。
/// 菜单开着时的第二次右键必须由我们主动关闭菜单并整体吞掉——若把这对事件
/// 放行给系统，任务栏会弹出原生时钟菜单并使桌面左键卡死（实测复现）。
pub fn dismiss_native_menu_from_hook() {
    let hwnd = MENU_OWNER_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut std::ffi::c_void)),
                WM_CANCELMODE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// 枚举回调上下文：任务栏线程 ID。
static HIDE_TARGET_THREAD: AtomicIsize = AtomicIsize::new(0);

/// 枚举回调：隐藏任务栏线程上所有可见的 tooltip 窗口。
unsafe extern "system" fn enum_hide_tooltip_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let target_thread = lparam.0 as u32;
    let mut pid = 0u32;
    let thread = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if thread == target_thread {
        let mut buf = [0u16; 32];
        let len = GetClassNameW(hwnd, &mut buf);
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        if class.starts_with("tooltips_class32") && IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    true.into()
}

/// 隐藏任务栏的所有 tooltip，并取消其鼠标模式。
/// tooltip 悬浮在时钟正上方（即菜单「退出」项位置），置顶且会吃掉点击。
fn hide_taskbar_tooltips() {
    unsafe {
        let Ok(tray) = FindWindowW(w!("Shell_TrayWnd"), None) else {
            return;
        };
        let _ = PostMessageW(Some(tray), WM_CANCELMODE, WPARAM(0), LPARAM(0));
        if let Some(clock_hwnd) = crate::windows_hook::find_clock_window() {
            let _ = PostMessageW(Some(clock_hwnd), 675, WPARAM(0), LPARAM(0));
        }
        let tray_thread = GetWindowThreadProcessId(tray, None);
        HIDE_TARGET_THREAD.store(tray_thread as isize, Ordering::SeqCst);
        let _ = EnumWindows(
            Some(enum_hide_tooltip_proc),
            LPARAM(HIDE_TARGET_THREAD.load(Ordering::SeqCst)),
        );
    }
}

/// 在时钟点击位置附近显示右键菜单（物理坐标），并尝试获取焦点。
pub fn show_clock_context_menu(
    app_handle: &AppHandle,
    click_x: i32,
    click_y: i32,
) -> Result<(), String> {
    let window = ensure_window(app_handle).ok_or_else(|| "创建右键菜单窗口失败".to_string())?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let width = (MENU_WIDTH * scale).ceil() as i32;
    let height = (MENU_HEIGHT * scale).ceil() as i32;
    // 菜单整体置于点击点上方，让光标落点正好在「退出」项中心附近
    // （退出项位于菜单下部约 3/4 处）；水平方向以点击点居中。
    // 关键：钳制边界用 **SPI_GETWORKAREA 工作区**（系统扣除任务栏后的真实可用
    // 区域）。UIA 的任务栏/时钟矩形比可见任务栏偏低数十像素，按它们钳制会让
    // 「退出」项被任务栏遮住（表现为菜单后置、点退出无反应）。
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let mut work = windows::Win32::Foundation::RECT::default();
    let mut work_bottom = screen_height;
    let mut x_min = 0;
    let mut x_max = screen_width;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut windows::Win32::Foundation::RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    if work.bottom > 0 && work.bottom < screen_height {
        work_bottom = work.bottom;
    }
    if work.left > 0 {
        x_min = work.left;
    }
    if work.right > 0 && work.right < screen_width {
        x_max = work.right;
    }
    let mut x = (click_x - width / 2).clamp(x_min, (x_max - width).max(x_min));
    let mut y = (click_y - height * 3 / 4).clamp(0, (work_bottom - height).max(0));
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    let _ = window.show();
    // 收起任务栏 tooltip：它会悬浮在时钟正上方（即菜单「退出」项的位置），
    // 是比菜单更晚弹出的置顶窗口，会**盖住菜单并吃掉点击**（实测截图确认）。
    hide_taskbar_tooltips();
    // tooltip 会在光标停留在时钟上时重新弹出，持续压制并保持菜单在置顶层顶部。
    let topmost_window = window.clone();
    tauri::async_runtime::spawn(async move {
        for delay_ms in [600u64, 1500u64] {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            if !IS_MENU_OPEN.load(Ordering::SeqCst) {
                return;
            }
            hide_taskbar_tooltips();
            unsafe {
                if let Some(h) = topmost_window.hwnd().ok() {
                    let _ = SetWindowPos(
                        HWND(h.0),
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
    });
    // 菜单与时钟判定区重叠，显示期间钩子放行该区域事件，保证点击可达菜单。
    // 注意：**不因失焦隐藏**——真实用户环境中焦点常在数百毫秒内被抢走，
    // 若失焦即隐藏，用户点击「退出」时会落在已被隐藏的空处（点退出没反应）。
    let _ = window.set_focus();
    IS_MENU_OPEN.store(true, Ordering::SeqCst);
    MENU_RECT
        .write()
        .map(|mut guard| *guard = Some((x, y, x + width, y + height)))
        .ok();
    crate::dbg_log(&format!(
        "ccm shown at ({x},{y}) click=({click_x},{click_y}) scale={scale}"
    ));
    Ok(())
}

/// 隐藏右键菜单（选择动作、Esc 键或点击菜单外部时调用）。
pub fn hide_clock_context_menu(app_handle: &AppHandle) {
    if let Some(window) = app_handle.get_webview_window("clock_context_menu") {
        let _ = window.hide();
    }
    IS_MENU_OPEN.store(false, Ordering::SeqCst);
    MENU_RECT.write().map(|mut guard| *guard = None).ok();
}

// ---- 原生 TrackPopupMenu 路径（用户偏好的系统风格菜单；Tauri 窗口路径保留为备用）----

/// 原生菜单属主窗口句柄缓存（TrackPopupMenu 必须有属主，空句柄会不显示/闪退）。
static MENU_OWNER_HWND: AtomicIsize = AtomicIsize::new(0);

/// 属主窗口过程：全部转发 `DefWindowProcW`。
unsafe extern "system" fn menu_owner_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 惰性创建隐藏属主窗口（消息窗口语义，无需显示）。
unsafe fn ensure_menu_owner_window() -> Option<HWND> {
    let cached = MENU_OWNER_HWND.load(Ordering::SeqCst);
    if cached != 0 {
        return Some(HWND(cached as *mut std::ffi::c_void));
    }
    let class_name = w!("liCalendarMenuOwnerWnd");
    let hmodule = GetModuleHandleW(None).ok()?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(menu_owner_wnd_proc),
        hInstance: hmodule.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hmodule.into()),
            None,
        )
        .ok()?;
        MENU_OWNER_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        Some(hwnd)
    }
}

/// 前台获取（无 AttachThreadInput）：直接 SetForegroundWindow，失败则瞬时 ALT
/// 解锁前台保护后重试；同步等待按键排空再检查（ALT 异步送达菜单会致其自关）。
unsafe fn acquire_foreground(owner: HWND) -> bool {
    for _ in 0..3 {
        let _ = SetForegroundWindow(owner);
        std::thread::sleep(std::time::Duration::from_millis(30));
        if GetForegroundWindow() == owner {
            return true;
        }
        unsafe {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = SetForegroundWindow(owner);
        std::thread::sleep(std::time::Duration::from_millis(40));
        if GetForegroundWindow() == owner {
            return true;
        }
    }
    false
}

/// 归还前台：优先原前台窗口，仍不奏效则落到桌面（Progman）。
unsafe fn restore_foreground(pre_menu: HWND, owner: HWND) {
    let target = if pre_menu.0.is_null() || pre_menu == owner {
        FindWindowW(w!("Progman"), None).unwrap_or_default()
    } else {
        pre_menu
    };
    if !target.0.is_null() && target != owner {
        let _ = SetForegroundWindow(target);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if GetForegroundWindow() == owner {
        let desktop = FindWindowW(w!("Progman"), None).unwrap_or_default();
        if !desktop.0.is_null() && desktop != owner {
            let _ = SetForegroundWindow(desktop);
        }
    }
}

/// 原生 TrackPopupMenu 菜单：同步跟踪至选择/取消，返回所选动作。
/// 需要的防护全部沿用现版经验：属主窗口、ALT 解锁前台（无 AttachThreadInput）、
/// tooltip 压制、工作区定位（BOTTOMALIGN 使菜单整体位于任务栏上方）、硬退出。
pub fn track_native_clock_menu(click_x: i32, click_y: i32) -> Option<&'static str> {
    IS_MENU_OPEN.store(true, Ordering::SeqCst);
    crate::windows_hook::NATIVE_MENU_TRACKING.store(true, Ordering::SeqCst);
    let result;
    unsafe {
        let Some(owner) = ensure_menu_owner_window() else {
            crate::dbg_log("native: ensure_menu_owner_window failed");
            IS_MENU_OPEN.store(false, Ordering::SeqCst);
            crate::windows_hook::NATIVE_MENU_TRACKING.store(false, Ordering::SeqCst);
            return None;
        };
        let pre_menu_foreground = GetForegroundWindow();

        // 释放本线程可能残留的鼠标捕获（上一轮菜单关闭时可能遗留）。残留捕获会把
        // 后续所有点击路由到属主窗口——桌面图标点击将全部失效、新菜单瞬关。
        unsafe {
            let _ = ReleaseCapture();
        }

        // 说明：不做 SetForegroundWindow/ALT 抢前台。实测抢前台（尤其 ALT 注入的
        // 异步按键在 TPM 启动后送达）会导致菜单瞬关；而 TPM 自带鼠标捕获，
        // 菜单显示与菜单外点击关闭均不依赖前台。

        // tooltip 会盖在菜单「退出」项上方（视觉遮挡）；TPM 持有捕获不影响点击送达
        hide_taskbar_tooltips();

        let Ok(hmenu) = CreatePopupMenu() else {
            IS_MENU_OPEN.store(false, Ordering::SeqCst);
            crate::windows_hook::NATIVE_MENU_TRACKING.store(false, Ordering::SeqCst);
            return None;
        };
        let _ = AppendMenuW(hmenu, MF_STRING, MENU_SETTINGS_ID as usize, w!("设置"));
        let _ = AppendMenuW(hmenu, MF_STRING, MENU_EXIT_ID as usize, w!("退出"));

        // 菜单底边贴工作区底缘（任务栏上方），BOTTOMALIGN + 居中于点击点横坐标
        let screen_height = GetSystemMetrics(SM_CYSCREEN);
        let screen_width = GetSystemMetrics(SM_CXSCREEN);
        let mut work = windows::Win32::Foundation::RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut windows::Win32::Foundation::RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let menu_y = if work.bottom > 0 && work.bottom < screen_height {
            work.bottom
        } else {
            screen_height
        };
        let menu_x = click_x.clamp(0, (screen_width - 160).max(0));
        crate::dbg_log(&format!(
            "native: TrackPopupMenu at ({menu_x},{menu_y}) click=({click_x},{click_y})"
        ));

        // TPM_RETURNCMD：同步返回所选命令 ID；取消/点外部返回 0。
        // 瞬关重试：菜单在 <150ms 内无选择消失（前台被系统瞬时抢走/未及显示）→
        // 重试最多 2 次——人手从菜单弹出到点击不可能快于 150ms。
        let mut ret = BOOL(0);
        let mut attempt: u32 = 0;
        let track_start;
        loop {
            let start = std::time::Instant::now();
            ret = TrackPopupMenu(
                hmenu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_CENTERALIGN | TPM_BOTTOMALIGN,
                menu_x,
                menu_y,
                None,
                owner,
                None,
            );
            let elapsed = start.elapsed();
            crate::dbg_log(&format!(
                "native: TPM attempt={attempt} ret={} elapsed={elapsed:?}",
                ret.0
            ));
            if ret.0 != 0 || elapsed >= std::time::Duration::from_millis(150) || attempt >= 2 {
                track_start = start;
                break;
            }
            attempt += 1;
            // 重新抢前台再弹一次
            let _ = SetForegroundWindow(owner);
            std::thread::sleep(std::time::Duration::from_millis(30));
            if GetForegroundWindow() != owner {
                unsafe {
                    keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
                    keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                let _ = SetForegroundWindow(owner);
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        }
        let _ = DestroyMenu(hmenu);
        let _ = PostMessageW(Some(owner), 0, WPARAM(0), LPARAM(0)); // WM_NULL 收尾
        // 跟踪结束后释放 TPM 遗留的鼠标捕获，避免影响后续点击路由。
        unsafe {
            let _ = ReleaseCapture();
        }
        let _ = track_start;

        restore_foreground(pre_menu_foreground, owner);
        let cmd = match ret.0 as u32 {
            MENU_EXIT_ID => Some("exit"),
            MENU_SETTINGS_ID => Some("settings"),
            _ => None,
        };
        result = cmd;
    }
    IS_MENU_OPEN.store(false, Ordering::SeqCst);
    crate::windows_hook::NATIVE_MENU_TRACKING.store(false, Ordering::SeqCst);
    crate::dbg_log(&format!("native: result={:?}", result));
    result
}
