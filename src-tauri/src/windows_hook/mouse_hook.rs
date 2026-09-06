//! 低级鼠标钩子安装、消息泵线程与点击事件投递。

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::thread;
use tokio::sync::mpsc;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::clock_window::{find_clock_window, is_mouse_in_clock_area, update_clock_area_cache};
use super::registry_clock::disable_custom_clock;
use super::state::{
    EVENT_SENDER, HOOK_HANDLE, IS_MENU_OPEN, NATIVE_MENU_TRACKING, TASKBAR_WIDGET_ENABLED,
};
use super::types::{ClickEvent, MouseButton};
use super::window_utils::is_foreground_fullscreen;

/// 封装低级鼠标钩子的安装与从 Tokio 侧消费点击事件的通道。
pub struct WindowsHookManager {
    /// 从钩子线程接收点击事件的异步接收端（仅 [`Self::take_event_receiver`] 可取走一次）。
    event_receiver: Option<mpsc::UnboundedReceiver<ClickEvent>>,
}

/// 设置是否启用任务栏日历组件；关闭时会清理自定义时钟。
///
/// * `enabled` - 为真时安装钩子并允许拦截；为假时清空事件通道并恢复系统时钟。
pub fn set_taskbar_widget_enabled(enabled: bool) {
    TASKBAR_WIDGET_ENABLED.store(enabled, Ordering::SeqCst);
    if !enabled {
        if let Ok(mut global_sender) = EVENT_SENDER.lock() {
            *global_sender = None;
        }
        let _ = disable_custom_clock();
    }
}

/// 退出前的确定性清理：显式卸载低级鼠标钩子并清空事件通道。
///
/// 不依赖「进程终止时系统隐式移除钩子」——若退出瞬间钩子回调正在执行，
/// 隐式清理可能留下短暂的全局输入卡顿/路由异常；显式卸载可彻底避免。
pub fn uninstall_global_mouse_hook() {
    if let Ok(handle) = super::state::HOOK_HANDLE.lock() {
        if let Some(hook) = *handle {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(hook as *mut c_void));
            }
        }
    }
    if let Ok(mut handle) = super::state::HOOK_HANDLE.lock() {
        *handle = None;
    }
    if let Ok(mut global_sender) = super::state::EVENT_SENDER.lock() {
        *global_sender = None;
    }
}

impl WindowsHookManager {
    /// 创建管理器并注册全局点击事件发送端。
    pub fn new() -> Self {
        // `sender` 交给全局 `EVENT_SENDER`，供 `mouse_hook_proc` 投递；`receiver` 由本结构体持有
        let (sender, receiver) = mpsc::unbounded_channel();

        if let Ok(mut global_sender) = EVENT_SENDER.lock() {
            *global_sender = Some(sender);
        }

        Self { event_receiver: Some(receiver) }
    }

    /// 在当前模块句柄上安装 `WH_MOUSE_LL` 低级鼠标钩子。
    pub fn install_mouse_hook(&self) -> Result<()> {
        unsafe {
            // 当前模块实例，用于 `SetWindowsHookExW` 的 `hmod`（与 `mouse_hook_proc` 同模块）
            let module_handle = GetModuleHandleW(None)?;

            let hook = SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook_proc),
                Some(HINSTANCE(module_handle.0)),
                0,
            )?;

            if let Ok(mut handle) = HOOK_HANDLE.lock() {
                *handle = Some(hook.0 as isize);
            }

            println!("Windows 鼠标钩子安装成功");
            Ok(())
        }
    }

    /// 卸载全局鼠标钩子并释放句柄。
    pub fn uninstall_hook(&self) -> Result<()> {
        unsafe {
            if let Ok(mut handle) = HOOK_HANDLE.lock() {
                if let Some(hook) = handle.take() {
                    UnhookWindowsHookEx(HHOOK(hook as *mut c_void))?;
                    println!("Windows 鼠标钩子已卸载");
                }
            }
            Ok(())
        }
    }

    /// 取走点击事件接收端（仅可调用一次，供异步任务消费）。
    pub fn take_event_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<ClickEvent>> {
        self.event_receiver.take()
    }

    /// 查找任务栏时钟子窗口句柄。
    pub fn find_clock_window(&self) -> Option<HWND> {
        find_clock_window()
    }
}

impl Drop for WindowsHookManager {
    fn drop(&mut self) {
        // 释放时卸载钩子并尝试恢复系统默认时钟，避免残留注册表覆盖
        let _ = self.uninstall_hook();
        let _ = disable_custom_clock();
    }
}

/// 在独立线程中安装钩子并运行 `GetMessageW` 消息泵（避免阻塞主线程）。
pub fn start_hook_message_thread() {
    thread::spawn(|| {
        unsafe {
            // UIA 取时钟矩形需要 COM，与钩子同线程（低级钩子在安装线程上回调）
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            // 独立线程中挂载钩子并运行 `GetMessageW` 循环，满足低级钩子的消息泵要求
            if let Ok(module_handle) = GetModuleHandleW(None) {
                match SetWindowsHookExW(
                    WH_MOUSE_LL,
                    Some(mouse_hook_proc),
                    Some(HINSTANCE(module_handle.0)),
                    0,
                ) {
                    Ok(hook) => {
                        if let Ok(mut handle) = HOOK_HANDLE.lock() {
                            *handle = Some(hook.0 as isize);
                        }
                        println!("✅ 已在专用消息泵线程上安装鼠标钩子");
                        // 初始化时做一次 UIA 取矩形；钩子回调内只读缓存，不得在此线程之外重复轮询刷新。
                        update_clock_area_cache();
                    }
                    Err(e) => {
                        eprintln!("❌ 安装鼠标钩子失败: {:?}", e);
                        return;
                    }
                }
            } else {
                eprintln!("❌ 钩子线程无法获取模块句柄");
                return;
            }

            let mut msg = MSG::default();
            // 阻塞式消息泵；仅当线程收到 `WM_QUIT` 时结束（本流程通常长期运行）
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

/// 取证日志文件句柄（诊断构建专用；追加模式，进程内复用）。
static FORENSICS_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// 简述窗口：句柄 + 类名 + 进程 ID；空句柄返回 "null"。
unsafe fn describe_hwnd(hwnd: HWND) -> String {
    if hwnd.0.is_null() {
        return "null".to_string();
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut cls = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut cls);
    let class = String::from_utf16_lossy(&cls[..len as usize]);
    format!("0x{:X}(class={class},pid={pid})", hwnd.0 as usize)
}

/// 在低级钩子回调内记录按钮事件的落点窗口与 GUI 线程状态（诊断构建专用，保持轻量）。
fn forensics_log_event(msg: u32, x: i32, y: i32) {
    let in_clock = is_mouse_in_clock_area(x, y);
    let menu_open = IS_MENU_OPEN.load(Ordering::SeqCst);
    let fullscreen = is_foreground_fullscreen();
    let will_swallow = in_clock && !menu_open && !fullscreen;
    let line = unsafe {
        let under = WindowFromPoint(POINT { x, y });
        let mut gti: GUITHREADINFO = std::mem::zeroed();
        gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        let _ = GetGUIThreadInfo(0, &mut gti);
        let fg = GetForegroundWindow();
        format!(
            "fx: msg=0x{msg:X} at=({x},{y}) under={} cap={} active={} fg={} clock={in_clock} menu={menu_open} fs={fullscreen} decision={}",
            describe_hwnd(under),
            describe_hwnd(gti.hwndCapture),
            describe_hwnd(gti.hwndActive),
            describe_hwnd(fg),
            if will_swallow { "swallow" } else { "pass" }
        )
    };
    if let Ok(mut guard) = FORENSICS_FILE.lock() {
        if guard.is_none() {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(r"D:\agents_tmp\hook_forensics.log")
            {
                Ok(f) => *guard = Some(f),
                Err(_) => return,
            }
        }
        {
            use std::io::Write;
            let _ = writeln!(guard.as_mut().expect("checked"), "{line}");
        }
    }
}

/// 低级鼠标钩子过程：在任务栏时钟区域内吞掉按键并投递 [`ClickEvent`]。
///
/// * `code` - 钩子代码；`<0` 时必须转发。
/// * `wparam` - 鼠标消息 ID（如 `WM_LBUTTONDOWN`）。
/// * `lparam` - 指向 [`MSLLHOOKSTRUCT`] 的指针。
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if !TASKBAR_WIDGET_ENABLED.load(Ordering::SeqCst) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // 当前鼠标消息类型（来自 `wparam`）
    let msg = wparam.0 as u32;

    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN;
    let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP;

    if !is_down && !is_up {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let mouse_struct = *(lparam.0 as *const MSLLHOOKSTRUCT);
    let x = mouse_struct.pt.x;
    let y = mouse_struct.pt.y;

    // 取证日志（诊断构建）：记录每个按钮事件的落点窗口与 GUI 线程状态，
    // 用于定位"桌面左键失灵"时点击实际被谁接收。
    forensics_log_event(msg, x, y);

    // 右键菜单窗口显示期间：按下落在菜单窗口外（任意位置）→ 隐藏菜单；
    // 其余事件一律放行：菜单本身是普通可见窗口，点击直接送达即可，
    // 吞掉会导致菜单收不到点击（表现为点「退出」没反应）。
    // 原生 TrackPopupMenu 跟踪期间：它自带鼠标捕获并自行处理菜单外点击，
    // 钩子对所有事件纯放行。
    if IS_MENU_OPEN.load(Ordering::SeqCst) {
        if NATIVE_MENU_TRACKING.load(Ordering::SeqCst) {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        if is_down
            && !crate::window_manager::menu_rect_contains(x, y)
            && !is_foreground_fullscreen()
        {
            crate::window_manager::hide_from_hook();
        }
        return CallNextHookEx(None, code, wparam, lparam);
    }

    if is_mouse_in_clock_area(x, y) {
        // 全屏前台（游戏/全屏视频等）时不拦截，避免吞掉本应交给前台的鼠标消息
        if is_foreground_fullscreen() {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let mouse_button =
            if msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP { MouseButton::Left } else { MouseButton::Right };

        if is_down {
            crate::dbg_log(&format!(
                "hook: down in clock area ({},{}) msg=0x{:X} button={:?}",
                x, y, msg, mouse_button
            ));

            if mouse_button == MouseButton::Right {
                unsafe {
                    // 取消模式并伪造时钟区 `WM_MOUSELEAVE`（数值 675），收起系统 Tooltip
                    if let Ok(shell_tray) = FindWindowW(w!("Shell_TrayWnd"), None) {
                        let _ = PostMessageW(Some(shell_tray), WM_CANCELMODE, WPARAM(0), LPARAM(0));
                    }
                    if let Some(clock_hwnd) = find_clock_window() {
                        let _ = PostMessageW(Some(clock_hwnd), 675, WPARAM(0), LPARAM(0));
                    }
                }
                // 右键菜单在「抬起」时才投递：若在按下时进入 TrackPopupMenu 循环，
                // 随后的物理抬起会被菜单视为“点击外部”而立即关闭（表现为白框一闪而过）。
            } else if let Ok(sender_guard) = EVENT_SENDER.lock() {
                if let Some(sender) = sender_guard.as_ref() {
                    let event = ClickEvent { x, y, in_clock_area: true, button: mouse_button };
                    let _ = sender.send(event);
                }
            }
        } else if mouse_button == MouseButton::Right {
            // 右键抬起：此时按钮已释放，弹出的菜单不会被随后的抬起事件关闭
            if let Ok(sender_guard) = EVENT_SENDER.lock() {
                if let Some(sender) = sender_guard.as_ref() {
                    let event = ClickEvent { x, y, in_clock_area: true, button: mouse_button };
                    let _ = sender.send(event);
                }
            }
        }

        // 在时钟区域内吞掉消息，阻止系统弹出原生任务栏右键菜单
        return LRESULT(1);
    }

    // 非时钟区域：放行消息，同时把左键按下投递给监听端——
    // 用于 WIN+D 之后桌面日历被最小化时的自动恢复（桌面处于前台即可恢复）。
    if is_down && msg == WM_LBUTTONDOWN {
        if let Ok(sender_guard) = EVENT_SENDER.lock() {
            if let Some(sender) = sender_guard.as_ref() {
                let event = ClickEvent { x, y, in_clock_area: false, button: MouseButton::Left };
                let _ = sender.send(event);
            }
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}
