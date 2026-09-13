//! watch.rs——explorer PID 监视 + 会话生命周期持有（方案 §4.1，E1 重注入落点）。
//!
//! 循环语义：
//! - explorer 不在场 → 会话视为失效（tap 随宿主消亡），等待；
//! - PID 变化 / 会话未建立 → `ensure_session`（必要时钩子注入）。新鲜 explorer 沉降期
//!   ~4.5min（B 阶段实测：过早 init 报 0x80070490/advise 零重放），失败按 15s 重试、
//!   不缩短节奏；成功后 spawn 数据线程；
//! - 会话在场 → 每 5s 心跳 ping（tap 侧 35s 静默兜底之上保持活跃），写失败=会话失效；
//! - tap 事件（tap/tree_rebuilt/track_fail/lost）全量落日志。

use super::data;
use super::ffi;
use super::session;
use std::sync::atomic::Ordering;

pub fn explorer_pid() -> Option<u32> {
    unsafe {
        let hwnd = ffi::GetShellWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        ffi::GetWindowThreadProcessId(hwnd, &mut pid);
        (pid != 0).then_some(pid)
    }
}

pub fn spawn_watch() {
    std::thread::Builder::new()
        .name("clockbar-watch".into())
        .spawn(|| {
            watch_loop();
        })
        .ok();
}

fn watch_loop() {
    super::dbg_log("watch: lifecycle thread started");
    session::reset_cursor();
    let mut session_active = false;
    let mut last_pid: Option<u32> = None;
    let mut last_ping = std::time::Instant::now();
    let mut seen = 0usize;
    let mut last_props = std::time::Instant::now() - std::time::Duration::from_secs(540);
    let mut last_props_text: Option<String> = None;
    let mut last_props_minute: Option<u16> = None;
    let mut props_text_minute: u16 = 0;
    loop {
        if super::STOP.load(Ordering::Relaxed) {
            super::dbg_log("watch: stop flag — exiting");
            return;
        }
        // tap 事件全量落日志（快照后锁外打印，不消费全局游标，与 wait_for 判定互不干扰）
        {
            let q = session::msg_queue();
            let new_lines: Vec<String> = match q.lock() {
                Ok(v) => v.iter().skip(seen).cloned().collect(),
                Err(_) => Vec::new(),
            };
            seen += new_lines.len();
            for l in &new_lines {
                super::dbg_log(&format!("tap: {l}"));
            }
        }
        let pid = explorer_pid();
        if pid.is_none() {
            if session_active || last_pid.is_some() {
                super::dbg_log("watch: explorer gone (GetShellWindow none)");
            }
            session_active = false;
            last_pid = None;
            sleep_interruptible(2000);
            continue;
        }
        if last_pid != pid {
            super::dbg_log(&format!(
                "watch: explorer pid {} (was {})",
                pid.unwrap(),
                last_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
            ));
            // E1：explorer 重启=重注入。沉降期 ~4.5min，先等 3s（方案 §4.1）再走
            // ensure_session 失败重试节奏。
            session_active = false;
            last_pid = pid;
            sleep_interruptible(3000);
        }
        if !session_active {
            // E1 教训（双门）：
            // ① 年龄门——explorer 出生 <45s 不注入（~4s 内注入的两次端口永久不就绪，
            //    ≥17s 注入的全部成功；tap v49 沉降期长预算兜底）；
            // ② 驻留报告——DLL 驻留而会话未建时 hook_inject 返回错误并按 15s 重试，
            //    tap Install 成功即自连管道，本循环自然收敛。
            let age = session::explorer_age_secs(pid.unwrap());
            if age < 45 {
                if age != u64::MAX {
                    super::dbg_log(&format!(
                        "watch: explorer age {age}s <45s — defer inject (15s)"
                    ));
                }
                sleep_interruptible(15_000);
                continue;
            }
            match session::ensure_session() {
                Ok(()) => {
                    session_active = true;
                    last_ping = std::time::Instant::now();
                    super::dbg_log("watch: session established");
                    data::spawn_data_thread();
                }
                Err(e) => {
                    // 沉降期失败在此重试；15s 节奏勿缩短（方案 §3 A/B 实测）
                    super::dbg_log(&format!("watch: session failed: {e} — retry in 15s"));
                    sleep_interruptible(15_000);
                }
            }
        } else {
            // 心跳：5s 间隔（tap 侧 35s 静默兜底之上保持活跃）
            if last_ping.elapsed() >= std::time::Duration::from_secs(5) {
                if !super::pipe::pipe_write(b"ping") {
                    super::dbg_log("watch: ping write failed — session lost, tap auto-restores");
                    session_active = false;
                } else {
                    last_ping = std::time::Instant::now();
                }
            }
            // VM 停写巡检（D 阶段遗留观察项，E5）：每 10min 只读 props 回 Time.Text，
            // 连续两次读数相同且系统分钟已前进 ⇒ VM 停写 ALERT（判别工具=dprops 口径）。
            if session_active && last_props.elapsed() >= std::time::Duration::from_secs(600) {
                last_props = std::time::Instant::now();
                if super::pipe::pipe_write(b"props") {
                    if let Some(l) = session::wait_for(
                        |l| l.contains(r#""t":"props""#) && l.contains(r#""text""#),
                        3_000,
                    ) {
                        if let Some(text) = extract_json_str(&l, "text") {
                            let minute_now = super::ffi::now_local_time().minute;
                            if Some(&text) == last_props_text.as_ref()
                                && Some(minute_now) != last_props_minute
                                && minute_now != props_text_minute
                            {
                                super::dbg_log(&format!(
                                    "watch: VM STOPWRITE ALERT — Time.Text stuck at '{text}' across checks (system minute now {minute_now})"
                                ));
                            } else {
                                super::dbg_log(&format!(
                                    "watch: props patrol Time.Text='{text}' (ok)"
                                ));
                            }
                            last_props_text = Some(text.clone());
                            last_props_minute = Some(minute_now);
                            props_text_minute = extract_minute(&text).unwrap_or(minute_now);
                        }
                    }
                }
            }
        }
        sleep_interruptible(1000);
    }
}

fn sleep_interruptible(ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if super::STOP.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 从限界 JSON 取 "key":"value"（同 tap JGetStr 口径：无转义）。
fn extract_json_str(j: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = j.find(&pat)? + pat.len();
    let rest = &j[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// "H:mm[:ss]" → 分钟数（跨天 23:59→00:00 视为前进）。
fn extract_minute(t: &str) -> Option<u16> {
    let hm = t.split(':').collect::<Vec<_>>();
    if hm.len() < 2 {
        return None;
    }
    Some(hm[1].parse().ok()?)
}
