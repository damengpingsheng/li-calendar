//! session.rs——TAP 会话原语（E0 生产迁移，探针 ensure_session/hook_inject/detach 同源）。
//! 会话生命周期所有权在 watch.rs；本文件只提供无状态原语与消息队列。

use super::ffi;
use super::pipe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const TAP_VER: u32 = 60;
// CLSID {D4C1B77E-4E2F-4E7A-9B31-5F0A6C2E8B14}
// GUID 内存布局（LE）：Data1 u32 | Data2/Data3 u16 拼一个 u32 | Data4[0..4] | Data4[4..8]
pub const TAP_CLSID: [u32; 4] = [0xD4C1_B77E, 0x4E7A_4E2F, 0x0A5F_319B, 0x148B_2E6C];
pub const WUX_DLL: &str = "Windows.UI.Xaml.dll";

type MsgQ = Arc<Mutex<Vec<String>>>;


static MSGQ: Mutex<Option<MsgQ>> = Mutex::new(None);

/// 全局消息队列（惰性创建；pipe 读线程与 session 原语共用）。
pub fn msg_queue() -> MsgQ {
    let mut g = MSGQ.lock().unwrap();
    if g.is_none() {
        *g = Some(Arc::new(Mutex::new(Vec::new())));
    }
    g.as_ref().unwrap().clone()
}

pub(crate) fn push_line(q: &MsgQ, line: &str) {
    if let Ok(mut v) = q.lock() {
        v.push(line.to_string());
        if v.len() > 8000 {
            let cut = v.len() - 8000;
            v.drain(..cut);
        }
    }
}

// 【B 阶段血泪 #7 续】全局消费游标：每条消息全进程只消费一次，防止陈旧响应污染判定。
static CURSOR: AtomicUsize = AtomicUsize::new(0);

/// 从队列按游标取一条匹配消息（带超时轮询）。
pub fn wait_for(pred: impl Fn(&str) -> bool, timeout_ms: u64) -> Option<String> {
    let q = msg_queue();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let line = {
            let Ok(v) = q.lock() else { return None };
            let c = CURSOR.load(Ordering::Relaxed);
            if c < v.len() {
                let l = v[c].clone();
                CURSOR.store(c + 1, Ordering::Relaxed);
                Some(l)
            } else {
                None
            }
        };
        if let Some(l) = line {
            if pred(&l) {
                return Some(l);
            }
            continue;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// 排空历史消息并复位游标（防陈旧 loaded/响应污染新会话判定）。
pub fn reset_cursor() {
    CURSOR.store(0, Ordering::Relaxed);
    if let Ok(mut q) = msg_queue().lock() {
        q.clear();
    }
}

/// loaded 谓词（版本匹配；S 修复：原硬编码 "ver":49 对 v50/v51 永不命中，
/// poke 循环首轮永远失败、靠 15s 重试路径的 pipe 重连兜底——对齐 TAP_VER 常量）。
fn is_loaded_tap(l: &str) -> bool {
    l.contains(r#""t":"loaded""#) && l.contains(&format!(r#""ver":{TAP_VER}"#))
}

/// explorer 进程年龄（秒）；无法判定返回 u64::MAX（不阻塞注入）。
/// 【E1 实证】explorer 出生 ~4s 内注入的两次（41176/14916）XAML 诊断端口永久不就绪
/// （tap 沉降期长预算重试 1h 仍 0x80070490），≥17s 注入的三次全部成功——宿主侧
/// 注入前置门：进程年龄不足不注入。
pub fn explorer_age_secs(pid: u32) -> u64 {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    unsafe {
        let h = ffi::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return u64::MAX;
        }
        let (mut c, mut e, mut k, mut u) = (0i64, 0i64, 0i64, 0i64);
        let ok = ffi::GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        ffi::CloseHandle(h);
        if ok == 0 || c == 0 {
            return u64::MAX;
        }
        // FILETIME → 与 GetSystemTimeAsFileTime 同基准（100ns since 1601）
        #[link(name = "kernel32")]
        extern "system" {
            fn GetSystemTimeAsFileTime(lp: *mut i64);
        }
        let mut now = 0i64;
        GetSystemTimeAsFileTime(&mut now);
        (((now - c) / 10_000_000) as u64).max(0)
    }
}

/// tap DLL 是否已驻留目标 explorer（模块枚举，前缀匹配 lical_clock_tap*）。
pub fn tap_dll_resident(pid: u32) -> bool {
    const TH32CS_SNAPMODULE: u32 = 0x8;
    const TH32CS_SNAPMODULE32: u32 = 0x10;
    #[repr(C)]
    struct ModuleEntry32W {
        dw_size: u32,
        _th32_module_id: u32,
        th32_process_id: u32,
        _glblcnt_usage: u32,
        _proccnt_usage: u32,
        _mod_base_addr: *mut u8,
        _mod_base_size: u32,
        sz_module: [u16; 256],
        _sz_exe_path: [u16; 260],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> ffi::H;
        fn Module32FirstW(snap: ffi::H, entry: *mut ModuleEntry32W) -> i32;
        fn Module32NextW(snap: ffi::H, entry: *mut ModuleEntry32W) -> i32;
    }
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
        if snap.is_null() || snap as isize == -1 {
            return false;
        }
        let mut entry = ModuleEntry32W {
            dw_size: std::mem::size_of::<ModuleEntry32W>() as u32,
            _th32_module_id: 0,
            th32_process_id: 0,
            _glblcnt_usage: 0,
            _proccnt_usage: 0,
            _mod_base_addr: std::ptr::null_mut(),
            _mod_base_size: 0,
            sz_module: [0; 256],
            _sz_exe_path: [0; 260],
        };
        let mut found = false;
        if Module32FirstW(snap, &mut entry) != 0 {
            loop {
                let len = entry
                    .sz_module
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(0);
                let name = String::from_utf16_lossy(&entry.sz_module[..len]);
                if name.len() >= 16
                    && name[..16].eq_ignore_ascii_case("lical_clock_tap")
                {
                    found = true;
                    break;
                }
                if Module32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        ffi::CloseHandle(snap);
        found
    }
}

/// XAML 诊断端口就绪探测（E1 第 3 轮教训产物，注入前置门）：
/// 宿主用不存在的 TAP 路径做一次外部 InitializeXamlDiagnosticsEx——
/// 0x80070490(E_NOTFOUND)=目标端口未就绪（沉降期，勿注入——tap Install 预算仅
/// 60 次≈1min，过早注入=预算耗尽永久躺平）；其余 hr=端口就绪（垃圾路径在引擎
/// LoadLibrary 处失败，无激活副作用）。探测在独立线程执行（每线程仅可初始化
/// 一次的语义保护，A 阶段实测）。

/// 钩子注入（TranslucentTB 生产通道，A 阶段定案）：WH_CALLWNDPROC 挂任务栏线程载入
/// TAP DLL，DllMain 检测 explorer 宿主后自行进程内初始化。成功后引擎已永久钉住 DLL，卸钩。
pub fn hook_inject() -> Result<(), String> {
    const WH_CALLWNDPROC: i32 = 4;
    // E1 教训：DLL 已驻留但会话未建立——v48 时=Install 线程已躺平（注入/poke 永远
    // 无效）；v49 长预算下也可能是沉降期重试中。两种情况本错误都只让 watch 按节奏
    // 重试，tap Install 成功即自连管道收敛。
    if let Some(pid) = super::watch::explorer_pid() {
        if tap_dll_resident(pid) {
            return Err(
                "tap dll resident but inactive (install thread gave up during settling) — explorer restart required".into(),
            );
        }
    }
    let Some(dll) = super::tap_dll_path() else {
        return Err("TAP DLL not found (deploy dir & dev bin)".into());
    };
    unsafe {
        let tray = ffi::FindWindowW(ffi::wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if tray.is_null() {
            return Err("Shell_TrayWnd not found".into());
        }
        let tid = ffi::GetWindowThreadProcessId(tray, std::ptr::null_mut());
        if tid == 0 {
            return Err("GetWindowThreadProcessId failed".into());
        }
        // 本进程先 LoadLibrary 拿 HMODULE（DllMain 检测非 explorer，不会启动安装）
        let hmod = ffi::LoadLibraryW(ffi::wide(&dll.to_string_lossy()).as_ptr());
        if hmod.is_null() {
            return Err(format!("LoadLibrary({dll:?}) gle={}", ffi::GetLastError()));
        }
        let proc = ffi::GetProcAddress(hmod, b"LicalHookProc\0".as_ptr());
        if proc.is_null() {
            return Err("GetProcAddress(LicalHookProc) failed".into());
        }
        let hhk = ffi::SetWindowsHookExW(WH_CALLWNDPROC, proc, hmod, tid);
        if hhk.is_null() {
            return Err(format!("SetWindowsHookExW gle={}", ffi::GetLastError()));
        }
        super::dbg_log("session: hook installed, poking taskbar thread...");
        // poke 多次：任务栏线程瞬时忙时 SMTO 会静默放弃投递（B 阶段实测），loaded 到达即止
        for poke in 1..=10 {
            let mut res = 0usize;
            let ok = ffi::SendMessageTimeoutW(tray, 0x0000, 0, 0, 2, 2000, &mut res);
            super::dbg_log(&format!("session: poke {poke} send_ret={ok}"));
            if wait_for(is_loaded_tap, 2_000).is_some() {
                ffi::UnhookWindowsHookEx(hhk);
                super::dbg_log("session: tap loaded, hook removed");
                return Ok(());
            }
        }
        ffi::UnhookWindowsHookEx(hhk);
        Err("no matching-version `loaded` within poke loop".into())
    }
}

/// 建立会话：确认 tap 已连接（loaded；无则钩子注入）→ advise → 聚合等待 advised+ready。
/// ready 可能先于 advised 到达（tap advise ack 固定延迟 200ms），必须聚合等待。
pub fn ensure_session() -> Result<(), String> {
    // 驻留判定：pipe 已连接（或 loaded 已在队列）。新 explorer 重启后两者皆无 → 注入。
    let connected = pipe::G_PIPE.lock().map(|g| g.is_some()).unwrap_or(false);
    if !connected {
        // 【E1 实测教训】上一会话的陈旧 loaded 残留在队列会让注入被跳过（探针每次
        // 新进程无此问题）——管道未连接时先清历史再判驻留。
        reset_cursor();
        if wait_for(is_loaded_tap, 1_500).is_none() {
            super::dbg_log("session: no resident tap; hook-injecting...");
            hook_inject()?;
        }
    }
    if !pipe::pipe_write(b"advise") {
        return Err("advise write failed (pipe not connected)".into());
    }
    let mut got_adv = false;
    let mut got_ready = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !(got_adv && got_ready) {
        match wait_for(
            |l| l.contains(r#""t":"advised""#) || l.contains(r#""t":"ready""#),
            2_000,
        ) {
            Some(l) => {
                super::dbg_log(&format!("session: MSG {l}"));
                if l.contains(r#""t":"advised""#) {
                    got_adv = true;
                }
                if l.contains(r#""t":"ready""#) {
                    got_ready = true;
                }
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "session incomplete: advised={got_adv} ready={got_ready} (25s)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 下发 c1set 行（数据+样式 JSON）。返回 tap 的 c1set ack 摘要。
pub fn send_c1set(json: &str) -> Option<String> {
    let mut line = String::from("c1set ");
    line.push_str(json);
    if !pipe::pipe_write(line.as_bytes()) {
        return None;
    }
    wait_for(|l| l.contains(r#""t":"c1set""#), 15_000)
}

/// 优雅拆除（E3 全清理语义）：c1free 摘面板 → unadvise 退订 → 断开连接。
/// tap 侧断线另有 AUTO 恢复（B0）双保险。全部尽力而为，任何一步失败不阻塞退出。
pub fn detach_best_effort() {
    if pipe::pipe_write(b"c1free") {
        let _ = wait_for(|l| l.contains(r#""t":"c1free""#), 3_000);
    }
    if pipe::pipe_write(b"unadvise") {
        let _ = wait_for(|l| l.contains(r#""t":"unadvised""#), 3_000);
    }
    pipe::disconnect_server();
}
