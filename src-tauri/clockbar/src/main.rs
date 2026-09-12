// clockbar_probe — B 阶段可逆修改探针（方案 v2 §5 B；A 阶段子命令保留）
// std-only：FFI 手写声明，不依赖任何 crate。
// 子命令：
//   cycle <n> [dwell_ms]     A 阶段：n 次 attach/detach 循环
//   dump <secs> / selfdump / hookdump / hookcycle / pipetest / raw   A 阶段保留
//   btest                    B1：路线A/B 各一次 settext+restore，全程回读+截图
//   bkill                    B0：settext2 后硬杀自身，验证 tap 断线自动恢复
//   bstress <secs>           修改态驻留观察（外部脚本同时打全屏压力/主题切换）
//   bwatch <secs>            纯观察模式（不改文本），记录 ready/lost/gen 事件
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

const PIPE_NAME: &str = r"\\.\pipe\lical-clockbar-b46"; // 与 tap.cpp TAPVER 版本化一致
const SDK_DLL: &str = r"D:\environment\WindowsKits\10\bin\x64\XamlDiagnostics\xamldiagnostics.dll";
const WUX_DLL: &str = "Windows.UI.Xaml.dll"; // 系统目录，POC 证实其导出 InitializeXamlDiagnosticsEx
const TAP_DLL: &str = r"D:\project\li-calendar\src-tauri\clockbar\bin\lical_clock_tap46.dll";
const TAP_VER: &str = "46";
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
        let hu = h as usize; eprintln!("[srv] pipe handle=0x{:x}", hu); // 句柄按 usize 跨线程（Send）
        std::thread::spawn(move || {
            let h = hu as H;
            while !STOP.load(Ordering::Relaxed) {
                let _ = ConnectNamedPipe(h, std::ptr::null());
                if let Ok(mut g) = G_PIPE.lock() { *g = Some(h as usize); }
                let mut carry = String::new();
                // 【B 阶段血泪 #7】阻塞式 ReadFile 的 TRUE+0 虚唤醒会吞掉客户端已写入的
                // 数据（实测：tap ok=1 写入 3 条，host 仅见 TRUE+0，数据永久丢失，且丢失
                // 哪条呈随机性）。改为 PeekNamedPipe 轮询：确认有字节才 ReadFile，绕开该
                // 内核行为；空轮询 20ms 间隔（B 阶段消息频率极低，CPU 代价可忽略）。
                extern "system" {
                    fn PeekNamedPipe(h: H, buf: *mut u8, bufsize: u32, read: *mut u32,
                        avail: *mut u32, left: *mut u32) -> i32;
                }
                'read: loop {
                    let mut avail: u32 = 0;
                    let pok = PeekNamedPipe(h, std::ptr::null_mut(), 0, std::ptr::null_mut(),
                        &mut avail, std::ptr::null_mut());
                    if pok == 0 {
                        eprintln!("[srv] peek end gle={}", GetLastError());
                        break 'read;
                    }
                    if avail == 0 { std::thread::sleep(std::time::Duration::from_millis(20)); continue; }
                    let mut buf = [0u8; 4096];
                    let mut n = 0u32;
                    let ok = ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null());
                    if ok == 0 {
                        eprintln!("[srv] read end gle={} n={}", GetLastError(), n);
                        break 'read;
                    }
                    if n == 0 { continue; }
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

// 【B 阶段修复】全局消费游标：每条消息全进程只消费一次。旧的局部 seen 设计会让
// 后续 wait_for 从队列头重扫，匹配到陈旧响应（实测 B 的 settext 匹配到 A 的旧 setres）。
static CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn take_matching(msgs: &MsgQ, pred: impl Fn(&str) -> bool, timeout_ms: u64) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let line = {
            let Ok(q) = msgs.lock() else { return None };
            let c = CURSOR.load(Ordering::Relaxed);
            if c < q.len() {
                let l = q[c].clone();
                CURSOR.store(c + 1, Ordering::Relaxed);
                Some(l)
            } else { None }
        };
        if let Some(l) = line {
            if pred(&l) { return Some(l); }
            continue;
        }
        if std::time::Instant::now() >= deadline { return None; }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for(msgs: &MsgQ, pred: impl Fn(&str) -> bool, timeout_ms: u64) -> Option<String> {
    take_matching(msgs, pred, timeout_ms)
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

// ── B 阶段 ────────────────────────────────────────────

fn shot(tag: &str) {
    // C 阶段：长条比 B 阶段 560px 窗口宽，用会话目录的加宽截图（右锚 1500x160）
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass",
               "-File", r"D:\agents_tmp\c_stage_20260913\cshot.ps1", tag])
        .status();
}

/// 打印队列内新到达的消息（带本地时间戳），持续 secs 秒
fn drain_msgs(msgs: &MsgQ, secs: u64, label: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut seen = 0usize;
    while std::time::Instant::now() < deadline {
        let lines: Vec<String> = {
            let Ok(q) = msgs.lock() else { return };
            if seen < q.len() { let l = q[seen..].to_vec(); seen = q.len(); l } else { Vec::new() }
        };
        for l in lines {
            let now = chrono_lite();
            println!("[{now}][{label}] tap: {l}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn chrono_lite() -> String {
    // 无 crate：用系统命令拿时间太重；直接计数无关紧要，借用 std 无时钟——改用 Instant 相对秒不可读。
    // 简案：调用 GetLocalTime via FFI（已链 kernel32/user32 区）。
    unsafe {
        #[repr(C)]
        struct ST { y: u16, mo: u16, dow: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16 }
        extern "system" { fn GetLocalTime(st: *mut ST); }
        let mut st = ST { y: 0, mo: 0, dow: 0, d: 0, h: 0, mi: 0, s: 0, ms: 0 };
        GetLocalTime(&mut st);
        format!("{:02}:{:02}:{:02}.{:03}", st.h, st.mi, st.s, st.ms)
    }
}

/// B 会话建立：pipe 服务端已在 main 建立；等 loaded（若 1.5s 内无则钩子注入全新 explorer），
/// 然后 advise → 等 ready（时钟链定位）。
/// 注意：ready 在 Advise 重放时即刻发出，而 tap 的 advise ack 固定 Sleep(200) 后才发——
/// ready 可能先于 advised 到达，必须聚合等待（wait_for 顺序消费会吞掉先到的 ready）。
fn ensure_session(msgs: &MsgQ) -> Result<(), String> {
    let want = format!(r#""ver":{}"#, TAP_VER);
    if wait_for(msgs, |l| l.contains(r#""t":"loaded""#) && l.contains(&want), 1_500).is_none() {
        println!("[probe] no resident tap; hook-injecting fresh...");
        let hhk = hook_inject(msgs)?;
        unsafe { UnhookWindowsHookEx(hhk); } // 初始化已把 DLL 钉住，钩子即可卸
    } else {
        println!("[probe] resident tap connected");
    }
    if !pipe_write(b"advise") { return Err("advise write failed".into()); }
    // 聚合等待（共享游标）：advised 与 ready 各自消费一次，顺序不限
    let mut got_adv = false;
    let mut got_ready = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !(got_adv && got_ready) {
        match take_matching(msgs,
            |l| l.contains(r#""t":"advised""#) || l.contains(r#""t":"ready""#), 2_000)
        {
            Some(l) => {
                println!("[probe] MSG {l}");
                if l.contains(r#""t":"advised""#) { got_adv = true; }
                if l.contains(r#""t":"ready""#) { got_ready = true; }
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!("session incomplete: advised={got_adv} ready={got_ready} (25s)"));
                }
            }
        }
    }
    Ok(())
}

fn b_props(msgs: &MsgQ) {
    if !pipe_write(b"props") { eprintln!("[probe] props write failed"); return; }
    match wait_for(msgs, |l| l.contains(r#""t":"props""#), 8_000) {
        Some(l) => println!("[probe] {l}"),
        None => eprintln!("[probe] no props reply"),
    }
}

fn b_set(msgs: &MsgQ, cmd: &str) -> bool {
    if !pipe_write(cmd.as_bytes()) { eprintln!("[probe] set write failed"); return false; }
    match wait_for(msgs, |l| l.contains(r#""t":"setres""#), 10_000) {
        Some(l) => { println!("[probe] {l}"); l.contains("\"hr\":0") }
        None => { eprintln!("[probe] no setres reply"); false }
    }
}

fn b_restore(msgs: &MsgQ) -> bool {
    if !pipe_write(b"restore") { eprintln!("[probe] restore write failed"); return false; }
    match wait_for(msgs, |l| l.contains(r#""t":"rstres""#), 10_000) {
        Some(l) => { println!("[probe] {l}"); l.contains("\"hr\":0") && !l.contains("\"err\"") }
        None => { eprintln!("[probe] no rstres reply"); false }
    }
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
        let want = format!(r#""ver":{}"#, TAP_VER);
        // poke 多次：任务栏线程瞬时忙时 SMTO 会静默放弃投递（B 阶段实测出现过连续失败），
        // 每次 poke 间隔检查 loaded 到达即止
        for poke in 1..=10 {
            let mut res = 0usize;
            let ok = SendMessageTimeoutW(tray, 0x0000, 0, 0, 2, 2000, &mut res);
            println!("[probe] poke {poke} send_ret={ok}");
            if wait_for(msgs, |l| l.contains(r#""t":"loaded""#) && l.contains(&want), 2_000).is_some() {
                println!("[probe] tap: loaded v{TAP_VER} hhk=0x{:x}", hhk as usize);
                return Ok(hhk);
            }
        }
        UnhookWindowsHookEx(hhk);
        Err("no matching-version `loaded` within poke loop".into())
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
        "btest" => {
            // B1：路线A、路线B 各一轮 settext→回读→截图→restore→回读→截图
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            println!("[probe] === baseline ===");
            shot("b0_base");
            b_props(&msgs);
            println!("[probe] === route A (SetProperty) ===");
            shot("b1_beforeA");
            let okA = b_set(&msgs, "settext B1TESTA");
            std::thread::sleep(std::time::Duration::from_millis(1500));
            shot("b2_afterA");
            let rA = b_restore(&msgs);
            std::thread::sleep(std::time::Duration::from_millis(800));
            shot("b3_restoredA");
            println!("[probe] === route B (winRT direct) ===");
            shot("b4_beforeB");
            let okB = b_set(&msgs, "settext2 B1TESTB");
            std::thread::sleep(std::time::Duration::from_millis(1500));
            shot("b5_afterB");
            let rB = b_restore(&msgs);
            std::thread::sleep(std::time::Duration::from_millis(800));
            shot("b6_restoredB");
            b_props(&msgs);
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            println!("[probe] BTEST DONE setA={okA} restoreA={rA} setB={okB} restoreB={rB}");
            // 正常退出：pipe 断开触发 tap 自动恢复（应为 no-op）+ Unadvise
        }
        "bkill" => {
            // B0：settext2 后硬杀自身（不走任何清理）——验证 tap 侧断线自动恢复
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let okB = b_set(&msgs, "settext2 B1KILLED");
            std::thread::sleep(std::time::Duration::from_millis(1200));
            shot("b7_killed_modified");
            println!("[probe] BKILL: modified={okB}; killing self NOW (no pipe close, no restore cmd)");
            std::process::abort(); // 模拟被杀：立即终止，不发送任何命令
        }
        "bstress" => {
            // 修改态驻留 secs 秒（外部并行跑全屏压力/主题切换），随后 restore（superseded 判定生效）
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            println!("[probe] BSTRESS: setting text and dwelling {secs}s (run bstress_fs.ps1 / btheme.ps1 now)");
            let okB = b_set(&msgs, "settext2 B1STRESS");
            shot("b8_stress_set");
            drain_msgs(&msgs, secs, "stress");
            shot("b9_stress_end");
            let rB = b_restore(&msgs);
            std::thread::sleep(std::time::Duration::from_millis(800));
            shot("bA_stress_restored");
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            println!("[probe] BSTRESS DONE set={okB} restore={rB}");
        }
        "brapid" => {
            // 判别「系统 VM 每秒重写文本」假说：先启动连拍（单进程 12 帧 @150ms），
            // 随后立即 set——窗口期内必有多帧落在 set 之后
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let _ = std::fs::remove_file("D:\\agents_tmp\\bburst_rapid_05.png");
            let burst = std::thread::spawn(|| {
                let _ = std::process::Command::new("powershell")
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass",
                           "-File", r"D:\agents_tmp\bburst.ps1", "rapid", "12", "150"])
                    .status();
            });
            std::thread::sleep(std::time::Duration::from_millis(350)); // 连拍第 2~3 帧启动后 set
            let ok = b_set(&msgs, "settext2 B1RAPID");
            let _ = burst.join();
            println!("[probe] BRAPID set={ok}");
        }
        "bhit" => {
            // HitTest 判别：屏幕时钟区的可见元素句柄 vs 跟踪句柄（tap 日志看结果）
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            if !pipe_write(b"hitclock") { eprintln!("[probe] hitclock write failed"); std::process::exit(1); }
            let _ = wait_for(&msgs, |l| l.contains(r#""t":"hittest""#), 8_000);
            println!("[probe] hittest done — see tap log HITTEST lines");
        }
        "bwatch" => {
            // 纯观察（不改文本）：记录 ready/lost（代次）事件——主题切换/DPI 等场景
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            drain_msgs(&msgs, secs, "watch");
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
        }
        "c0tree" | "c0ins" | "c0meas" | "c0rm" | "c0add" => {
            // C0 单步命令（tap 侧 c0* 协议；结果细节在 tap 日志）
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let ack = format!(r#""t":"{cmd}""#);
            if !pipe_write(cmd.as_bytes()) { eprintln!("[probe] {cmd} write failed"); std::process::exit(1); }
            match wait_for(&msgs, |l| l.contains(&ack), 15_000) {
                Some(l) => println!("[probe] {l}"),
                None => { eprintln!("[probe] no {cmd} ack"); std::process::exit(1); }
            }
        }
        "c1kill" => {
            // B0 面板路径：c1set 建面板后硬杀自身——验证 tap 断线自动摘面板+恢复原生
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let json = args.get(2).cloned().unwrap_or(
                r#"{"weather":"晴 26°C","festival":"教师节","term":"白露","lunar":"七月廿二"}"#.into());
            let mut line = String::from("c1set ");
            line.push_str(&json);
            if !pipe_write(line.as_bytes()) { eprintln!("[probe] c1set write failed"); std::process::exit(1); }
            match wait_for(&msgs, |l| l.contains(r#""t":"c1set""#), 15_000) {
                Some(l) => println!("[probe] {l}"),
                None => { eprintln!("[probe] no c1set ack"); std::process::exit(1); }
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
            shot("c1kill_modified");
            println!("[probe] C1KILL: killing self NOW (panel built, no cleanup)");
            std::process::abort();
        }
        "c1hold" => {
            // C1/C2 主测试会话：建立会话→下发数据→保持在线 secs 秒（面板在断开时才自动
            // 恢复=B0 语义，故场景测试期间本进程必须驻留）。期间其他验证用 PowerShell 并行。
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);
            let json = args.get(3).cloned().unwrap_or_default();
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            if !json.is_empty() {
                let mut line = String::from("c1set ");
                line.push_str(&json);
                if !pipe_write(line.as_bytes()) { eprintln!("[probe] c1set write failed"); std::process::exit(1); }
                match wait_for(&msgs, |l| l.contains(r#""t":"c1set""#), 15_000) {
                    Some(l) => println!("[probe] {l}"),
                    None => { eprintln!("[probe] no c1set ack"); std::process::exit(1); }
                }
            }
            println!("[probe] C1HOLD holding session for {secs}s (t0={})", chrono_lite());
            // 心跳版 drain：每 10s 发 ping（tap 侧 35s 静默即视同断线自动恢复）
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            let mut last_ping = std::time::Instant::now() - std::time::Duration::from_secs(10);
            let mut seen = 0usize;
            while std::time::Instant::now() < deadline {
                let lines: Vec<String> = {
                    let Ok(q) = msgs.lock() else { break };
                    if seen < q.len() { let l = q[seen..].to_vec(); seen = q.len(); l } else { Vec::new() }
                };
                for l in lines {
                    println!("[{}][hold] tap: {}", chrono_lite(), l);
                }
                if last_ping.elapsed() >= std::time::Duration::from_secs(5) {
                    pipe_write(b"ping");
                    last_ping = std::time::Instant::now();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            println!("[probe] C1HOLD done; exiting (disconnect triggers tap auto-restore)");
        }
        "c1set" => {
            // C1：下发五段数据+样式 JSON（原样透传给 tap，限界解析在 tap 侧）
            let json = args.get(2).cloned().unwrap_or_default();
            if json.is_empty() { eprintln!("usage: c1set '<json>'"); std::process::exit(1); }
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let mut line = String::from("c1set ");
            line.push_str(&json);
            if !pipe_write(line.as_bytes()) { eprintln!("[probe] c1set write failed"); std::process::exit(1); }
            match wait_for(&msgs, |l| l.contains(r#""t":"c1set""#), 15_000) {
                Some(l) => println!("[probe] {l}"),
                None => { eprintln!("[probe] no c1set ack"); std::process::exit(1); }
            }
        }
        "c1free" => {
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            if !pipe_write(b"c1free") { eprintln!("[probe] c1free write failed"); std::process::exit(1); }
            match wait_for(&msgs, |l| l.contains(r#""t":"c1free""#), 15_000) {
                Some(l) => println!("[probe] {l}"),
                None => { eprintln!("[probe] no c1free ack"); std::process::exit(1); }
            }
            std::thread::sleep(std::time::Duration::from_millis(600));
            shot("c1_after_free");
        }
        "c1tap" => {
            // C2：监听 tap 事件（左/右键投递）secs 秒，打印到达时刻（时延=相对启动秒）
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            println!("[probe] listening for tap events for {secs}s (t0={})", chrono_lite());
            drain_msgs(&msgs, secs, "tap");
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
        }
        "ctest" => {
            // C0 全序列：树/宽度链 dump → winRT 插入探针（连拍验证渲染）→ 量测 →
            // 引擎 AddChild 对照 → 删除 → 量测（可逆性）
            if let Err(e) = ensure_session(&msgs) { eprintln!("[probe] SESSION FAILED: {e}"); std::process::exit(1); }
            let step = |c: &str, ack: &str| -> bool {
                let want = format!(r#""t":"{ack}""#);
                if !pipe_write(c.as_bytes()) { eprintln!("[probe] {c} write failed"); return false; }
                match wait_for(&msgs, |l| l.contains(&want), 15_000) {
                    Some(l) => { println!("[probe] {l}"); true }
                    None => { eprintln!("[probe] no {ack} ack"); false }
                }
            };
            println!("[probe] === C0 baseline ===");
            shot("c0_base");
            step("c0tree", "c0tree");
            println!("[probe] === winRT insert probe (route B structural) ===");
            if step("c0ins", "c0ins") {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let _ = std::process::Command::new("powershell")
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass",
                           "-File", r"D:\agents_tmp\c_stage_20260913\cburst.ps1", "c0ins", "8", "300"])
                    .status();
                step("c0meas", "c0meas");
                step("c0rm", "c0rm");
                std::thread::sleep(std::time::Duration::from_millis(800));
                shot("c0_after_rm");
                step("c0meas", "c0meas");
            }
            println!("[probe] === engine AddChild discriminator ===");
            step("c0add", "c0add");
            std::thread::sleep(std::time::Duration::from_millis(300));
            shot("c0_after_add");
            println!("[probe] stats: {}", ask_stats(&msgs).unwrap_or_default());
            println!("[probe] CTEST DONE (see tap log C0TREE/C0INS/C0MEAS/C0ADD lines)");
        }
        "pipetest2" => {
            // 最小复现：server 读线程模式与 pipe_server 相同；client 连接后立即写 2 条，
            // 500ms 后再写 2 条——验证第二批是否送达（B 阶段 ack 丢失问题隔离）
            let local: &str = r"\\.\pipe\lical-localtest2";
            unsafe {
                let lh = CreateNamedPipeW(wide(local).as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | GENERIC_READ | GENERIC_WRITE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1, 65536, 65536, 0, std::ptr::null());
                if lh as isize == -1 { println!("create gle={}", GetLastError()); return; }
                let lhu = lh as usize;
                // reader（同 pipe_server 模式）
                std::thread::spawn(move || {
                    let h = lhu as H;
                    let _ = ConnectNamedPipe(h, std::ptr::null());
                    println!("[srv] connected");
                    let mut carry = String::new();
                    loop {
                        let mut buf = [0u8; 4096];
                        let mut n = 0u32;
                        let ok = ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null());
                        if ok == 0 { println!("[srv] read err gle={} n={}", GetLastError(), n); break; }
                        if n == 0 { println!("[srv] TRUE+0 wakeup"); continue; }
                        carry.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
                        while let Some(pos) = carry.find('\n') {
                            let line: String = carry.drain(..=pos).collect();
                            println!("[srv] LINE: {}", line.trim_end());
                        }
                    }
                });
                // client
                #[link(name = "kernel32")]
                extern "system" {
                    #[link_name = "CreateFileW"]
                    fn CreateFileW2(name: *const u16, access: u32, share: u32, sa: *const u8,
                        disp: u32, flags: u32, tmpl: H) -> H;
                    #[link_name = "Sleep"]
                    fn Sleep2(ms: u32);
                    #[link_name = "WriteFile"]
                    fn WriteFile2(h: H, buf: *const u8, len: u32, n: *mut u32, ov: *const u8) -> i32;
                }
                std::thread::spawn(move || {
                    Sleep2(300);
                    let h = CreateFileW2(wide(local).as_ptr(), GENERIC_READ | GENERIC_WRITE, 0,
                        std::ptr::null(), 3, 0, std::ptr::null_mut());
                    if h as isize == -1 { eprintln!("[cl] open failed gle={}", GetLastError()); return; }
                    let msgs: [&[u8]; 4] = [b"m1\n", b"m2\n", b"m3\n", b"m4\n"];
                    for (i, m) in msgs.iter().enumerate() {
                        if i == 2 { Sleep2(500); }
                        let mut n = 0u32;
                        let ok = WriteFile2(h, m.as_ptr(), m.len() as u32, &mut n, std::ptr::null());
                        println!("[cl] write {} ok={} n={}", i + 1, ok, n);
                    }
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
            eprintln!("usage: clockbar_probe <c1set json/c1free/c1tap secs/ctest/btest/bkill/bstress secs/bwatch secs/cycle n/dump/hookdump/hookcycle/pipetest/raw endpoint>");
            std::process::exit(1);
        }
    }
}
