//! pipe.rs——named pipe 服务端（E0 生产化：DACL + 客户端身份校验）。
//! 协议与探针一致：行 JSON、单实例、双端 PeekNamedPipe 轮询（B 阶段血泪 #6/#7 全套防御）。

use super::ffi;
use super::session;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

pub const PIPE_NAME: &str = r"\\.\pipe\lical-clockbar-b62"; // 与 tap v62 版本化一致

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const SDDL_REVISION_1: u32 = 1;
const TOKEN_QUERY: u32 = 0x0008;
const TokenUser: i32 = 1;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

type MsgQ = Arc<Mutex<Vec<String>>>;

/// 已连接实例句柄（读线程登记；写侧经 pipe_write 使用）。
pub(crate) static G_PIPE: Mutex<Option<usize>> = Mutex::new(None);

/// 发送一行到 tap（自动补 \n）。返回 false=管道已断（tap 将走 AUTO 恢复）。
pub fn pipe_write(bytes: &[u8]) -> bool {
    unsafe {
        if let Ok(g) = G_PIPE.lock() {
            if let Some(hu) = *g {
                let h = hu as ffi::H;
                let mut data = bytes.to_vec();
                data.push(b'\n');
                let mut n = 0u32;
                return ffi::WriteFile(h, data.as_ptr(), data.len() as u32, &mut n, std::ptr::null())
                    != 0;
            }
        }
    }
    false
}

/// 构造限定当前用户的 SDDL DACL：SYSTEM、Administrators、当前进程令牌用户三 ACE，
/// 保护位 P（无继承）。失败返回 None（调用方退化为默认 SA——单用户桌面会话下风险可控，
/// 客户端身份校验仍在连接侧兜底）。
fn current_user_sddl() -> Option<String> {
    unsafe {
        let mut token: ffi::H = std::ptr::null_mut();
        if ffi::OpenProcessToken(ffi::GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        // 首查取所需长度
        let mut retlen = 0u32;
        let _ = ffi::GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut retlen);
        if retlen == 0 || retlen > 4096 {
            ffi::CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; retlen as usize];
        if ffi::GetTokenInformation(token, TokenUser, buf.as_mut_ptr(), retlen, &mut retlen) == 0 {
            ffi::CloseHandle(token);
            return None;
        }
        ffi::CloseHandle(token);
        // TOKEN_USER = { SID_AND_ATTRIBUTES { Sid: PSID, Attributes } }：首字段即 Sid 指针
        let sid_ptr = *(buf.as_ptr() as *const *const std::ffi::c_void);
        let mut strp: *mut u16 = std::ptr::null_mut();
        if ffi::ConvertSidToStringSidW(sid_ptr, &mut strp) == 0 || strp.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *strp.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(strp, len));
        ffi::LocalFree(strp as ffi::H);
        Some(format!("D:P(A;;GRGW;;;SY)(A;;GRGW;;;BA)(A;;GRGW;;;{sid})"))
    }
}

/// 客户端身份校验（E0 落地）：对端进程映像必须为 explorer.exe。
fn client_is_explorer(h: ffi::H) -> bool {
    unsafe {
        let mut pid = 0u32;
        if ffi::GetNamedPipeClientProcessId(h, &mut pid) == 0 || pid == 0 {
            return false;
        }
        let proc = ffi::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if proc.is_null() {
            return false;
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = ffi::QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len);
        ffi::CloseHandle(proc);
        if ok == 0 {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let base = path.rsplit(['\\', '/']).next().unwrap_or("");
        base.eq_ignore_ascii_case("explorer.exe")
    }
}

/// 启动 pipe 服务端线程（进程生命周期内单例；循环接受 tap 重连）。
pub fn start_server() {
    let msgs = session::msg_queue();
    std::thread::Builder::new()
        .name("clockbar-pipe".into())
        .spawn(move || {
            server_loop(msgs);
        })
        .ok();
}

fn server_loop(msgs: MsgQ) {
    unsafe {
        // DACL（E0）：SYSTEM/Administrators/当前用户三 ACE，保护位 P。
        // SD 内存覆盖 CreateNamedPipeW+ConnectNamedPipe 全程（sa/sd 存活于本函数栈）。
        let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut have_dacl = false;
        if let Some(sddl) = current_user_sddl() {
            if ffi::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                ffi::wide(&sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) != 0
                && !sd.is_null()
            {
                have_dacl = true;
            } else {
                super::dbg_log("pipe: build DACL failed — fallback default SA");
            }
        } else {
            super::dbg_log("pipe: resolve user SID failed — fallback default SA");
        }
        let sa = ffi::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<ffi::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let sa_ptr: *const ffi::SECURITY_ATTRIBUTES =
            if have_dacl { &sa as *const _ } else { std::ptr::null() };

        let name = ffi::wide(PIPE_NAME);
        let h = ffi::CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | GENERIC_READ | GENERIC_WRITE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            sa_ptr,
        );
        if h as isize == -1 {
            super::dbg_log(&format!(
                "pipe: CreateNamedPipeW failed gle={} (驻留旧会话/其他进程抢占？)",
                ffi::GetLastError()
            ));
            return;
        }
        super::dbg_log(&format!("pipe: server listening dacl={have_dacl}"));
        let hu = h as usize;
        while !super::STOP.load(Ordering::Relaxed) {
            let _ = ffi::ConnectNamedPipe(h, std::ptr::null());
            if super::STOP.load(Ordering::Relaxed) {
                break;
            }
            // E0 客户端身份校验：非 explorer 的连接直接掐断
            if !client_is_explorer(h) {
                super::dbg_log("pipe: client rejected (not explorer.exe)");
                ffi::DisconnectNamedPipe(h);
                continue;
            }
            if let Ok(mut g) = G_PIPE.lock() {
                *g = Some(hu);
            }
            super::dbg_log("pipe: tap connected");
            read_loop(h, &msgs);
            if let Ok(mut g) = G_PIPE.lock() {
                *g = None;
            }
            ffi::DisconnectNamedPipe(h);
            super::dbg_log("pipe: tap disconnected");
        }
        ffi::CloseHandle(h);
    }
}

fn read_loop(h: ffi::H, msgs: &MsgQ) {
    unsafe {
        let mut carry = String::new();
        // 【B 阶段血泪 #7】阻塞 ReadFile 的 TRUE+0 虚唤醒会吞数据 → Peek 轮询。
        loop {
            if super::STOP.load(Ordering::Relaxed) {
                return;
            }
            let mut avail: u32 = 0;
            let pok = ffi::PeekNamedPipe(
                h,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            );
            if pok == 0 {
                return; // 真断开（tap 侧走 AUTO 恢复）
            }
            if avail == 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            let mut buf = [0u8; 4096];
            let mut n = 0u32;
            let ok = ffi::ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null());
            if ok == 0 {
                return;
            }
            if n == 0 {
                continue; // 虚唤醒防御保留
            }
            carry.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
            while let Some(pos) = carry.find('\n') {
                let line: String = carry.drain(..=pos).collect();
                let line = line.trim_end_matches(['\n', '\r', '\0']).to_string();
                if !line.is_empty() {
                    session::push_line(msgs, &line);
                }
            }
            if carry.len() > 8192 {
                carry.clear();
            }
        }
    }
}

/// 主动断开当前连接（shutdown 序列用）：tap 侧 Peek 失败 → AUTO 恢复。
pub fn disconnect_server() {
    unsafe {
        if let Ok(mut g) = G_PIPE.lock() {
            if let Some(hu) = g.take() {
                ffi::DisconnectNamedPipe(hu as ffi::H);
            }
        }
    }
}
