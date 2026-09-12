// clockbar_probe — A 阶段只读探针（方案 v2 §5 A）
// std-only：FFI 手写声明，不依赖任何 crate。
// 子命令：
//   cycle <n> [dwell_ms] n 次 attach/detach 循环验收（门槛 ≥20）
//   dump <secs>         attach、收集 secs 秒树事件、打印 stats、detach
//   raw <endpoint>      单次 attach 指定 endPointName（未知清单#1 实验用），不退出
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

type H = *mut core::ffi::c_void;
type MsgQ = Arc<Mutex<Vec<String>>>;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> H;
    fn GetProcAddress(module: H, name: *const u8) -> H;
    fn GetLastError() -> u32;
    fn CreateNamedPipeW(name: *const u16, openmode: u32, pipemode: u32,
        maxinstances: u32, outbuf: u32, inbuf: u32, deftimeout: u32, sa: *const u8) -> H;
    fn ConnectNamedPipe(h: H, ov: *const u8) -> i32;
    fn DisconnectNamedPipe(h: H) -> i32;
    fn ReadFile(h: H, buf: *mut u8, len: u32, n: *mut u32, ov: *const u8) -> i32;
    fn WriteFile(h: H, buf: *const u8, len: u32, n: *mut u32, ov: *const u8) -> i32;
    fn CloseHandle(h: H) -> i32;
}
#[link(name = "user32")]
extern "system" {
    fn GetShellWindow() -> H;
    fn GetWindowThreadProcessId(hwnd: H, pid: *mut u32) -> u32;
    fn FindWindowW(cls: *const u16, name: *const u16) -> H;
    fn SetWindowsHookExW(id: i32, proc: H, module: H, tid: u32) -> H;
    fn UnhookWindowsHookEx(hhk: H) -> i32;
}

type InitXamlDiagEx = unsafe extern "system" fn(
    endPointName: *const u16, pid: u32, wszDllXamlDiagnostics: *const u16,
    wszTAPDllName: *const u16, tapClsid: *const u8, wszInitializationData: *const u16,
) -> i32;

const PIPE_NAME: &str = r"\\.\pipe\lical-clockbar-a1";
const SDK_DLL: &str = r"D:\environment\WindowsKits\10\bin\x64\XamlDiagnostics\xamldiagnostics.dll";
const WUX_DLL: &str = "Windows.UI.Xaml.dll"; // 系统目录，POC 证实其导出 InitializeXamlDiagnosticsEx
const TAP_DLL: &str = r"D:\project\li-calendar\src-tauri\clockbar\bin\lical_clock_tap15.dll";
const TAP_VER: &str = "15";
// CLSID {D4C1B77E-4E2F-4E7A-9B31-5F0A6C2E8B14}
// GUID 内存布局（LE）：Data1 u32 | Data2/Data3 u16 拼一个 u32 | Data4[0..4] | Data4[4..8]
const TAP_CLSID: [u32; 4] = [0xD4C1_B77E, 0x4E7A_4E2F, 0x0A5F_319B, 0x148B_2E6C];

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }
fn hex(hr: i32) -> String { format!("0x{:08X}", hr as u32) }

fn explorer_pid() -> Option<u32> {
    unsafe {
        let hwnd = GetShellWindow();
        if hwnd.is_null() { return None; }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (pid != 0).then_some(pid)
    }
}

// 服务端已连接实例句柄（读线程登记；主线程写命令）
static G_PIPE: Mutex<Option<usize>> = Mutex::new(None); // 句柄按 usize 存（Send）
static STOP: AtomicBool = AtomicBool::new(false);

fn pipe_write(bytes: &[u8]) -> bool {
    unsafe {
        if let Ok(g) = G_PIPE.lock() {
            if let Some(hu) = *g { let h = hu as H;
                let mut data = bytes.to_vec();
                data.push(b'\n');
                let mut n = 0u32;
                return WriteFile(h, data.as_ptr(), data.len() as u32, &mut n, std::ptr::null()) != 0;
            }
        }
    }
    false
}

/// 启动 pipe 服务端（单实例、循环接受重连）；返回行消息队列的窥视接口。
fn pipe_server() -> Arc<Mutex<Vec<String>>> {
    let msgs = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx): (_, Receiver<String>) = channel();
    {
        let msgs2 = msgs.clone();
        std::thread::spawn(move || {
            while let Ok(m) = rx.recv() {
                if let Ok(mut q) = msgs2.lock() {
                    q.push(m);
                    if q.len() > 8000 { let cut = q.len() - 8000; q.drain(..cut); }
                }
            }
        });
    }
    let name = wide(PIPE_NAME);
    unsafe {
        let h = CreateNamedPipeW(name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | GENERIC_READ | GENERIC_WRITE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1, 65536, 65536, 0, std::ptr::null());
        if h as isize == -1 {
            eprintln!("[probe] CreateNamedPipeW failed gle={}", GetLastError());
            std::process::exit(2);
        }
        let hu = h as usize; // 句柄按 usize 跨线程（Send）
        std::thread::spawn(move || {
            let h = hu as H;
            while !STOP.load(Ordering::Relaxed) {
                let _ = ConnectNamedPipe(h, std::ptr::null());
                if let Ok(mut g) = G_PIPE.lock() { *g = Some(h as usize); }
                let mut carry = String::new();
                'read: loop {
                    let mut buf = [0u8; 4096];
                    let mut n = 0u32;
                    let ok = ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null());
                    if ok == 0 {
                        eprintln!("[srv] read end gle={} n={}", GetLastError(), n);
                        break 'read;
                    }
                    if n == 0 { continue; } // TRUE+0 = 虚唤醒（tick 实验证实连接仍活），继续读
                    carry.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                    while let Some(pos) = carry.find('\n') {
                        let line: String = carry.drain(..=pos).collect();
                        let line = line.trim_end_matches(['\n', '\r', '\0']).to_string();
                        if !line.is_empty() { let _ = tx.send(line); }
                    }
                    if carry.len() > 8192 { carry.clear(); } // 有界
                }
                if let Ok(mut g) = G_PIPE.lock() { *g = None; }
                DisconnectNamedPipe(h);
            }
            CloseHandle(h);
        });
    }
    msgs
}

fn wait_for(msgs: &MsgQ, pred: impl Fn(&str) -> bool, timeout_ms: u64) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut seen = 0usize;
    loop {
        let line = {
            let Ok(q) = msgs.lock() else { return None };
            if seen < q.len() { let l = q[seen].clone(); seen += 1; Some(l) } else { None }
        };
        if let Some(l) = line {
            if pred(&l) { return Some(l); }
            continue;
        }
        if std::time::Instant::now() >= deadline { return None; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn do_attach(msgs: &MsgQ, endpoint: &str) -> Result<(), String> {
    let pid = explorer_pid().ok_or("no explorer (GetShellWindow failed)")?;
    println!("[probe] explorer pid={pid}");
    unsafe {
        // POC 形态：Windows.UI.Xaml.dll（system32）导出 InitializeXamlDiagnosticsEx，
        // wszDllXamlDiagnostics 传空串（目标进程内由系统解析）。
        let engine = LoadLibraryW(wide(WUX_DLL).as_ptr());
        if engine.is_null() {
            return Err(format!("LoadLibrary({WUX_DLL}) failed gle={}", GetLastError()));
        }
        let proc = GetProcAddress(engine, b"InitializeXamlDiagnosticsEx\0".as_ptr());
        if proc.is_null() { return Err(format!("GetProcAddress({WUX_DLL}!InitializeXamlDiagnosticsEx) failed gle={}", GetLastError())); }
        let init: InitXamlDiagEx = std::mem::transmute(proc);
        println!("[probe] InitializeXamlDiagnosticsEx(endpoint={endpoint:?}, engine={WUX_DLL}, diagDll=\"\") ...");
        let hr = init(wide(endpoint).as_ptr(), pid, wide("").as_ptr(), wide(TAP_DLL).as_ptr(),
                      TAP_CLSID.as_ptr().cast(), std::ptr::null());
        println!("[probe] init hr={}", hex(hr));
        if hr != 0 { return Err(format!("InitializeXamlDiagnosticsEx -> {}", hex(hr))); }
    }
    match wait_for(msgs, |l| l.contains(r#""t":"loaded""#), 15_000) {
        Some(l) => { println!("[probe] tap: {l}"); Ok(()) }
        None => Err("no `loaded` from tap within 15s (engine may have failed asynchronously)".into())
    }
}

fn do_detach(msgs: &MsgQ) -> Result<(), String> {
    if !pipe_write(b"unadvise") { return Err("pipe write failed (tap not connected?)".into()); }
    match wait_for(msgs, |l| l.contains(r#""t":"unadvised""#), 10_000) {
        Some(l) => { println!("[probe] tap: {l}"); }
        None => return Err("no `unadvised` ack within 10s".into()),
    }
    // 断开实例 → tap 客户端进入重连等待（会话结束）
    if let Ok(mut g) = G_PIPE.lock() { *g = None; }
    Ok(())
}

fn ask_stats(msgs: &MsgQ) -> Option<String> {
    if !pipe_write(b"stats") { return Some("(pipe disconnected)".into()); }
    wait_for(msgs, |l| l.contains(r#""t":"stats""#), 5_000)
}

/// 钩子注入（TranslucentTB 生产通道）：WH_CALLWNDPROC 挂任务栏线程载入 TAP DLL，
/// DllMain 检测 explorer 宿主后自行进程内初始化。返回 hook 句柄（卸钩用）。
fn hook_inject(msgs: &MsgQ) -> Result<H, String> {
    const WH_CALLWNDPROC: i32 = 4;
    unsafe {
        let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if tray.is_null() { return Err("Shell_TrayWnd not found".into()); }
        let tid = GetWindowThreadProcessId(tray, std::ptr::null_mut());
        if tid == 0 { return Err("GetWindowThreadProcessId failed".into()); }
        println!("[probe] taskbar thread={tid}");
        // 本进程先 LoadLibrary 拿 HMODULE（DllMain 检测非 explorer，不会启动安装）
        let hmod = LoadLibraryW(wide(TAP_DLL).as_ptr());
        if hmod.is_null() { return Err(format!("LoadLibrary({TAP_DLL}) gle={}", GetLastError())); }
        let proc = GetProcAddress(hmod, b"LicalHookProc\0".as_ptr());
        if proc.is_null() { return Err("GetProcAddress(LicalHookProc) failed".into()); }
        let hhk = SetWindowsHookExW(WH_CALLWNDPROC, proc, hmod, tid);
        if hhk.is_null() { return Err(format!("SetWindowsHookExW gle={}", GetLastError())); }
        println!("[probe] hook installed, poking taskbar thread to force DLL load...");
        extern "system" { #[link(name = "user32")] fn SendMessageTimeoutW(hwnd: H, msg: u32, wp: usize, lp: isize, flags: u32, timeout: u32, res: *mut usize) -> isize; }
        let mut res = 0usize;
        SendMessageTimeoutW(tray, 0x0000, 0, 0, 2, 2000, &mut res);
        let want = format!(r#""ver":{}"#, TAP_VER);
        match wait_for(msgs, |l| l.contains(r#""t":"loaded""#) && l.contains(&want), 45_000) {
            Some(l) => { println!("[probe] tap: {l}"); Ok(hhk) }
            None => { UnhookWindowsHookEx(hhk); Err("no matching-version `loaded` within 45s".into()) }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let msgs = pipe_server();

    match cmd {
        "cycle" => {
            let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
            let dwell: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2000);
            let mut fails = 0usize;
            for i in 1..=n {
                if let Err(e) = do_attach(&msgs, "VisualDiagConnection1") {
                    fails += 1;
                    eprintln!("[probe] cycle {i}/{n} ATTACH FAILED: {e}");
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(dwell));
                let stats = ask_stats(&msgs);
                if let Err(e) = do_detach(&msgs) {
                    fails += 1;
                    eprintln!("[probe] cycle {i}/{n} DETACH FAILED: {e}");
                }
                println!("[probe] cycle {i}/{n} ok stats={}", stats.unwrap_or_default());
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            println!("[probe] CYCLE DONE n={n} fails={fails}");
            if fails > 0 { std::process::exit(1); }
        }
        "dump" => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            if let Err(e) = do_attach(&msgs, "VisualDiagConnection1") {
                eprintln!("[probe] ATTACH FAILED: {e}");
                std::process::exit(1);
            }
            std::thread::sleep(std::time::Duration::from_secs(secs));
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            let _ = do_detach(&msgs);
            println!("[probe] dump done -> D:\\agents_tmp\\clockbar_tap.log");
        }
        "selfdump" => {
            // 外部 attach（引导 DLL 加载+pipe 建立）→ 进程内自初始化（Windhawk 形态）→ 观察树
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
            if let Err(e) = do_attach(&msgs, "VisualDiagConnection1") {
                eprintln!("[probe] ATTACH FAILED: {e}");
                std::process::exit(1);
            }
            if !pipe_write(b"selfinit") {
                eprintln!("[probe] selfinit write failed");
                std::process::exit(1);
            }
            match wait_for(&msgs, |l| l.contains(r#""t":"selfinit""#), 10_000) {
                Some(l) => println!("[probe] tap: {l}"),
                None => eprintln!("[probe] no selfinit ack"),
            }
            std::thread::sleep(std::time::Duration::from_secs(secs));
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            let _ = do_detach(&msgs);
            println!("[probe] selfdump done -> D:\\agents_tmp\\clockbar_tap.log");
        }
        "hookdump" => {
            // 钩子注入 → 收集树 → 卸钩（DLL 驻留）
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let hhk = match hook_inject(&msgs) {
                Ok(h) => h,
                Err(e) => { eprintln!("[probe] HOOK INJECT FAILED: {e}"); std::process::exit(1); }
            };
            std::thread::sleep(std::time::Duration::from_secs(secs));
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            unsafe { UnhookWindowsHookEx(hhk); }
            println!("[probe] hookdump done -> D:\\agents_tmp\\clockbar_tap.log");
        }
        "hookcycle" => {
            // 钩子注入一次（模块驻留）→ N 次 advise/unadvise 会话循环（A 阶段 ≥20 门槛）
            let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
            let dwell: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1500);
            let hhk = match hook_inject(&msgs) {
                Ok(h) => h,
                Err(e) => { eprintln!("[probe] HOOK INJECT FAILED: {e}"); std::process::exit(1); }
            };
            unsafe { UnhookWindowsHookEx(hhk); } // 初始化已把 DLL 钉住，钩子即可卸
            let mut fails = 0usize;
            for i in 1..=n {
                let ok = pipe_write(b"advise"); println!("[dbg] advise write ok={ok}");
                if !ok { fails += 1; eprintln!("[probe] cycle {i}/{n} advise write failed"); continue; }
                match wait_for(&msgs, |l| l.contains(r#""t":"advised""#), 10_000) {
                    Some(l) => println!("[probe] cycle {i}/{n} {l}"),
                    None => { fails += 1; eprintln!("[probe] cycle {i}/{n} NO advised ack"); continue; }
                }
                std::thread::sleep(std::time::Duration::from_millis(dwell));
                if !pipe_write(b"unadvise") { fails += 1; eprintln!("[probe] cycle {i}/{n} unadvise write failed"); continue; }
                match wait_for(&msgs, |l| l.contains(r#""t":"unadvised""#), 10_000) {
                    Some(l) => println!("[probe] cycle {i}/{n} {l}"),
                    None => { fails += 1; eprintln!("[probe] cycle {i}/{n} NO unadvised ack"); }
                }
            }
            println!("[probe] HOOKCYCLE DONE n={n} fails={fails} final-stats={}", ask_stats(&msgs).unwrap_or_default());
            if fails > 0 { std::process::exit(1); }
        }
        "pipetest" => {
            // 独立管道的本地收发对照（不与 tap 抢实例）：验证双端读写实现是否健全
            let local: &str = r"\\.\pipe\lical-localtest";
            let lhu: usize;
            unsafe {
                let lh = CreateNamedPipeW(wide(local).as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | GENERIC_READ | GENERIC_WRITE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1, 65536, 65536, 0, std::ptr::null());
                if lh as isize == -1 { println!("[probe] local pipe create gle={}", GetLastError()); }
                lhu = lh as usize;
                let name2 = local.to_string();
                std::thread::spawn(move || {
                    #[link(name = "kernel32")]
                    extern "system" {
                        fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *const u8,
                            disp: u32, flags: u32, tmpl: H) -> H;
                        fn Sleep(ms: u32);
                        fn WriteFile(h: H, buf: *const u8, len: u32, n: *mut u32, ov: *const u8) -> i32;
                        fn ReadFile(h: H, buf: *mut u8, len: u32, n: *mut u32, ov: *const u8) -> i32;
                    }
                    const GENERIC_READ: u32 = 0x8000_0000;
                    const GENERIC_WRITE: u32 = 0x4000_0000;
                    const OPEN_EXISTING: u32 = 3;
                    Sleep(500);
                    let h = CreateFileW(wide(&name2).as_ptr(), GENERIC_READ | GENERIC_WRITE, 0,
                        std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
                    if h as isize == -1 { eprintln!("[cl] open failed"); return; }
                    let mut n = 0u32;
                    if WriteFile(h, b"ping\n".as_ptr(), 5, &mut n, std::ptr::null()) == 0 { eprintln!("[cl] write failed"); }
                    let mut buf = [0u8; 256];
                    let ok = ReadFile(h, buf.as_mut_ptr(), 256, &mut n, std::ptr::null());
                    eprintln!("[cl] reply ok={} n={} '{}'", ok != 0, n, String::from_utf8_lossy(&buf[..n as usize]));
                });
                // 服务端：延迟接受（模拟 tap 客户端抢先连接的时序）→ 读 ping → 回 pong
                std::thread::spawn(move || {
                    let lh = lhu as H;
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                    let c = ConnectNamedPipe(lh, std::ptr::null());
                    eprintln!("[srv] connect ret={} gle={}", c, GetLastError());
                    let mut buf = [0u8; 256];
                    let mut n = 0u32;
                    let ok = ReadFile(lh, buf.as_mut_ptr(), 256, &mut n, std::ptr::null());
                    eprintln!("[srv] read ok={} n={} '{}'", ok != 0, n, String::from_utf8_lossy(&buf[..n as usize]));
                    let mut wn = 0u32;
                    let wok = WriteFile(lh, b"pong\n".as_ptr(), 5, &mut wn, std::ptr::null());
                    eprintln!("[srv] write ok={}", wok != 0);
                });
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        "raw" => {
            let endpoint = args.get(2).cloned().unwrap_or("VisualDiagConnection1".into());
            if let Err(e) = do_attach(&msgs, &endpoint) {
                eprintln!("[probe] ATTACH FAILED: {e}");
                std::process::exit(1);
            }
            loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
        }
        _ => {
            eprintln!("usage: clockbar_probe <cycle n [dwell_ms] | dump secs | raw endpoint>");
            std::process::exit(1);
        }
    }
}
