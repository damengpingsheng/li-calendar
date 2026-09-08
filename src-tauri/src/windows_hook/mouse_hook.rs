//! 低级鼠标钩子安装、消息泵线程与点击事件投递。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tokio::sync::mpsc;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::clock_window::{
    find_clock_window, is_mouse_in_clock_area, refresh_clock_rect_if_in_taskbar,
    update_clock_area_cache,
};
use super::registry_clock::disable_custom_clock;
use tauri::Manager;
use super::state::{
    EVENT_SENDER, HOOK_HANDLE, IS_MENU_OPEN, NATIVE_MENU_TRACKING, TASKBAR_WIDGET_ENABLED,
    WIN_EVENT_HANDLES,
};
use super::types::{ClickEvent, MouseButton};
use super::window_utils::is_foreground_fullscreen;

/// 封装低级鼠标钩子的安装与从 Tokio 侧消费点击事件的通道。
pub struct WindowsHookManager {
    /// 从钩子线程接收点击事件的异步接收端（仅 [`Self::take_event_receiver`] 可取走一次）。
    event_receiver: Option<mpsc::UnboundedReceiver<ClickEvent>>,
}

/// 时钟区最近一次左键按下是否被放行（成对处置：抬起按此决定吞放）。
static LEFT_LAST_PASSED: AtomicBool = AtomicBool::new(false);
/// 时钟区最近一次右键按下是否被放行（成对处置：抬起按此决定吞放）。
static RIGHT_LAST_PASSED: AtomicBool = AtomicBool::new(false);
/// 时钟区内的右键按下了刚关闭菜单：随后的右键抬起被吞掉，但作为一次
/// **新的右键**投递——按「每次右键都重新弹出菜单」的预期，菜单在抬起时
/// 重新弹出（而非静默吞掉后需要第三次右键才再弹出）。
static NATIVE_DISMISS_PENDING: AtomicBool = AtomicBool::new(false);

/// 设置是否启用任务栏日历组件；关闭时会清理自定义时钟。
///
/// * `enabled` - 为真时安装钩子并允许拦截；为假时清空事件通道并恢复系统时钟。
pub fn set_taskbar_widget_enabled(enabled: bool) {
    TASKBAR_WIDGET_ENABLED.store(enabled, Ordering::SeqCst);
    // Phase 0：覆盖层随开关显隐——关闭后钩子放行，点击若落在无处理器的
    // 覆盖层上会表现为"点时钟没反应"，必须同步隐藏；重新开启走完整贴合
    // 流程（已建窗则重贴，未建窗则补建）。
    let overlay_app = super::app_handle();
    if enabled {
        std::thread::Builder::new()
            .name("clock-overlay-reenable".into())
            .spawn(move || {
                if let Some(app) = overlay_app {
                    crate::window_manager::ensure_clock_overlay_attached(&app);
                }
            })
            .ok();
    } else if let Some(app) = overlay_app {
        if let Some(window) = app.get_webview_window("clock_overlay") {
            let _ = window.hide();
        }
    }
    if !enabled {
        if let Ok(mut global_sender) = EVENT_SENDER.lock() {
            *global_sender = None;
        }
        let _ = disable_custom_clock();
    }
}

/// 退出前的确定性清理：显式卸载低级鼠标钩子与 WinEvent 钩子并清空事件通道。
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
    // WinEvent 钩子随鼠标钩子一同卸载（同为显式清理，不留给进程终止）
    if let Ok(mut handles) = WIN_EVENT_HANDLES.lock() {
        for hook in handles.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut c_void));
            }
        }
    }
    if let Ok(mut global_sender) = super::state::EVENT_SENDER.lock() {
        *global_sender = None;
    }
}

/// WinEvent 回调：前台切换（全屏进出）或任务栏窗口位移（自动隐藏滑入/滑出）
/// 时立即刷新覆盖层可见性——轮询有 2s 滞后，事件驱动才能与任务栏动作同步。
///
/// 在钩子消息泵线程上回调（OUTOFCONTEXT），只做轻量判定与 ShowWindow（微秒级），
/// 不碰 UIA；`EVENT_OBJECT_LOCATIONCHANGE` 全局量极大，必须先按窗口句柄过滤。
unsafe extern "system" fn overlay_win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_thread: u32,
    _event_time: u32,
) {
    // 只关心窗口本体（标题栏/滚动条等子对象对象 ID 非零，直接忽略）
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE {
        // 位移事件来自所有窗口（拖动任意窗口都触发），只放行两类：
        // 1) 任务栏自身——自动隐藏滑入/滑出跟随；
        // 2) 前台窗口——全屏进入/退出是前台窗口自身的矩形展开/缩回（前台
        //    不变、任务栏不动，FOREGROUND 事件收不到），必须逐帧做全屏检测，
        //    否则显隐要等 2s 兜底轮询（进入时浮在视频上"闪现"、退出时原生
        //    时钟露出一段）。
        if hwnd.0.is_null() {
            return;
        }
        let is_foreground = GetForegroundWindow() == hwnd;
        if !is_foreground && get_window_class_name_checked(hwnd) != "Shell_TrayWnd" {
            return;
        }
    }
    // EVENT_SYSTEM_FOREGROUND（前台切换→全屏检测）无需过滤，直接刷新
    if let Some(app) = super::app_handle() {
        crate::window_manager::update_clock_overlay_visibility(&app);
    }
}

/// 回调内安全读取类名（缓冲不足/失败返回空串，绝不 panic——extern 回调铁律）。
unsafe fn get_window_class_name_checked(hwnd: HWND) -> String {
    let mut buf = [0u16; 32];
    let len = GetClassNameW(hwnd, &mut buf);
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// 安装覆盖层可见性所需的 WinEvent 钩子（须在带消息泵的线程上调用）。
unsafe fn install_overlay_win_event_hooks() {
    // 前台切换：全屏应用进入/退出
    // 任务栏位移：自动隐藏滑入/滑出（每帧触发，回调内先按类名过滤）
    for event_range in [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
    ] {
        // windows 0.62：SetWinEventHook 直接返回 HWINEVENTHOOK（失败为空句柄）
        let hook = SetWinEventHook(
            event_range.0,
            event_range.1,
            None,
            Some(overlay_win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if !hook.0.is_null() {
            if let Ok(mut handles) = WIN_EVENT_HANDLES.lock() {
                handles.push(hook.0 as isize);
            }
        }
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
            // WinEvent 钩子随鼠标钩子一同卸载（Drop 路径对称清理）
            if let Ok(mut handles) = WIN_EVENT_HANDLES.lock() {
                for hook in handles.drain(..) {
                    let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut c_void));
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
                        // WinEvent（前台切换/任务栏位移）驱动覆盖层显隐，与鼠标钩子同泵线程
                        install_overlay_win_event_hooks();
                        // 初始化时做一次 UIA 取矩形；钩子回调内只读缓存，不得在此线程之外重复轮询刷新。
                        update_clock_area_cache();
                        // 周期性重探时钟矩形（独立线程，UIA 不进钩子回调）：
                        // 启动时探测发生在自定义时钟文本写入前后，任务栏重排会令矩形过期；
                        // 周期刷新保证任何重排后最多 ~2 秒自愈（表现为点时钟无反应/弹原生菜单）。
                        std::thread::spawn(|| {
                            let mut tick: u32 = 0;
                            loop {
                                // 遮盖保持期 150ms 加密轮询（归位即缩：UIA 确认原生
                                // 归位后立即收缩，把遮盖保持期压到"原生归位时间+
                                // 一次读数"）；稳态 500ms 照旧。加密只持续全屏退出的
                                // 瞬态窗口（≤2s），UIA 重操作的开销可忽略。
                                let mask_held =
                                    crate::window_manager::clock_overlay_mask_held();
                                std::thread::sleep(std::time::Duration::from_millis(
                                    if mask_held { 150 } else { 500 },
                                ));
                                if !TASKBAR_WIDGET_ENABLED.load(Ordering::SeqCst) {
                                    continue;
                                }
                                tick = tick.wrapping_add(1);
                                // 每 500ms：轻量可见性兜底（全屏/滑出检测均为纯 Win32 微秒级）。
                                // 教训：依赖事件驱动的显隐在事件缺失的路径上（部分应用退出全屏
                                // 时无前台切换、无窗口位移）要等兜底轮询，2s 太慢肉眼可见。
                                if let Some(app) = super::app_handle() {
                                    crate::window_manager::update_clock_overlay_visibility(&app);
                                }
                                // 每 2s：UIA 时钟矩形重探（重操作，维持 2s 节奏）+ 重贴；
                                // 遮盖保持期加密到每针（150ms）——UIA 确认原生归位后
                                // 同一针立刻重贴收缩，无需等下个 2s 周期。
                                if mask_held || tick % 4 == 0 {
                                    update_clock_area_cache();
                                    if let Some(app) = super::app_handle() {
                                        crate::window_manager::relocate_clock_overlay_endorsed(
                                            &app,
                                        );
                                    }
                                }
                            }
                        });
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
    // 用于定位”桌面左键失灵”时点击实际被谁接收。
    forensics_log_event(msg, x, y);

    // 任务栏带内按下时轻量重探时钟矩形（IsWindow+GetWindowRect，微秒级）：
    // 任务栏重排（自定义文本应用、图标增减等）会让启动时缓存的时钟矩形过期，
    // 不自愈则时钟点击全部放行给原生（表现为点时钟无反应/弹原生菜单）。
    if is_down {
        crate::windows_hook::refresh_clock_rect_if_in_taskbar(x, y);
    }

    let in_clock = is_mouse_in_clock_area(x, y);
    let menu_open = IS_MENU_OPEN.load(Ordering::SeqCst);
    let native_tracking = NATIVE_MENU_TRACKING.load(Ordering::SeqCst);
    let fullscreen = is_foreground_fullscreen();
    // 本对按下/抬起不归我们处置（整体放行）的条件：
    // 不在时钟区、全屏前台（游戏/视频）、或右键菜单正在显示。
    let passthrough = !in_clock
        || menu_open
        || fullscreen;

    // 成对处置铁律：系统只能看到完整的”按下+抬起”，绝不能出现只吞其一的孤儿
    // 事件——孤儿事件曾使任务栏输入状态卡死（表现为桌面图标左键点选失灵，
    // 右键一次重建状态后恢复）。每次按下记录是否放行，抬起按配对决定吞放。
    if is_down {
        let passed = passthrough;
        if msg == WM_LBUTTONDOWN {
            LEFT_LAST_PASSED.store(passed, Ordering::SeqCst);
        } else {
            RIGHT_LAST_PASSED.store(passed, Ordering::SeqCst);
        }
    }

    // 右键菜单显示期间（原生路径，无前台——TPM 不处理菜单项点击与菜单外关闭，
    // 由钩子全权代管交互），按三区域处置：
    // - 时钟区按下：关闭菜单 + 整体吞掉（右键置 pending，随后的抬起吞掉并把
    //   这次右键作为新右键投递 → 菜单重新弹出，符合"每次右键都重新弹出"）；
    // - 菜单外其他位置按下：关闭菜单 + 放行（点击送达下层目标）；
    // - 抬起在菜单内 → 按上/下半路由：上半=设置，下半=退出（左右键均支持）；
    // - 菜单内的其他事件放行（TPM 菜单窗口自行处理/忽略）。
    if menu_open && native_tracking {
        if is_down && !crate::window_manager::menu_rect_contains(x, y) {
            crate::window_manager::dismiss_native_menu_from_hook();
            if is_mouse_in_clock_area(x, y) {
                if msg == WM_RBUTTONDOWN {
                    NATIVE_DISMISS_PENDING.store(true, Ordering::SeqCst);
                }
                return LRESULT(1);
            }
            return CallNextHookEx(None, code, wparam, lparam);
        }
        if is_up
            && (msg == WM_RBUTTONUP || msg == WM_LBUTTONUP)
            && crate::window_manager::menu_rect_contains(x, y)
        {
            if let Some((rx1, ry1, rx2, ry2)) = crate::window_manager::native_menu_rect() {
                if x >= rx1 && x <= rx2 && y >= ry1 && y <= ry2 {
                    crate::window_manager::dismiss_native_menu_from_hook();
                    if let Some(app) = crate::windows_hook::app_handle() {
                        let exit = y > (ry1 + ry2) / 2;
                        std::thread::spawn(move || {
                            if exit {
                                crate::request_app_exit(&app);
                            } else {
                                crate::window_manager::show_or_create_main_window(&app);
                            }
                        });
                    }
                    return LRESULT(1);
                }
            }
        }
        return CallNextHookEx(None, code, wparam, lparam);
    }

    if !in_clock {
        // 非时钟区域：放行，同时把左键按下投递给监听端——
        // 用于 WIN+D 之后桌面日历被最小化时的自动恢复（桌面处于前台即可恢复）。
        if is_down && msg == WM_LBUTTONDOWN {
            if let Ok(sender_guard) = EVENT_SENDER.lock() {
                if let Some(sender) = sender_guard.as_ref() {
                    let event = ClickEvent { x, y, in_clock_area: false, button: MouseButton::Left };
                    let _ = sender.send(event);
                }
            }
        }
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // ---- 时钟区域正常拦截路径（fs=false、无菜单）----
    let is_left = msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP;

    if is_up {
        // 抬起：仅当对应按下也被吞时才吞并触发动作；按下被放行过则配对放行
        if is_left {
            if LEFT_LAST_PASSED.load(Ordering::SeqCst) {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            return LRESULT(1);
        }
        // 菜单刚被本次右键的按下关闭：吞掉这对事件的同时，把这次右键作为
        // 新的右键投递——菜单在抬起时重新弹出（与"每次右键都重新弹出"的
        // 系统菜单习惯一致）。重新弹出走监听端的完整路径（含 tooltip 压制）。
        if NATIVE_DISMISS_PENDING.swap(false, Ordering::SeqCst) {
            if let Ok(sender_guard) = EVENT_SENDER.lock() {
                if let Some(sender) = sender_guard.as_ref() {
                    let event =
                        ClickEvent { x, y, in_clock_area: true, button: MouseButton::Right };
                    let _ = sender.send(event);
                }
            }
            return LRESULT(1);
        }
        if RIGHT_LAST_PASSED.load(Ordering::SeqCst) {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        // 右键抬起：按钮已释放，弹出的菜单不会被随后的抬起事件关闭
        if let Ok(sender_guard) = EVENT_SENDER.lock() {
            if let Some(sender) = sender_guard.as_ref() {
                let event = ClickEvent { x, y, in_clock_area: true, button: MouseButton::Right };
                let _ = sender.send(event);
            }
        }
        return LRESULT(1);
    }

    // ---- 按下 ----
    if is_left {
        LEFT_LAST_PASSED.store(false, Ordering::SeqCst);
        // 左键按下：投递切换月历事件
        if let Ok(sender_guard) = EVENT_SENDER.lock() {
            if let Some(sender) = sender_guard.as_ref() {
                let event = ClickEvent { x, y, in_clock_area: true, button: MouseButton::Left };
                let _ = sender.send(event);
            }
        }
        return LRESULT(1);
    }

    RIGHT_LAST_PASSED.store(false, Ordering::SeqCst);
    unsafe {
        // 取消模式并伪造时钟区 `WM_MOUSELEAVE`（数值 675），收起系统 Tooltip
        if let Ok(shell_tray) = FindWindowW(w!("Shell_TrayWnd"), None) {
            let _ = PostMessageW(Some(shell_tray), WM_CANCELMODE, WPARAM(0), LPARAM(0));
        }
        if let Some(clock_hwnd) = find_clock_window() {
            let _ = PostMessageW(Some(clock_hwnd), 675, WPARAM(0), LPARAM(0));
        }
    }
    // 右键菜单在「抬起」时才投递：若在按下时弹菜单，随后的物理抬起会被
    // 菜单视为“点击外部”而立即关闭（表现为白框一闪而过）。
    LRESULT(1)
}
