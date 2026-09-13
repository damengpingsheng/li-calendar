//! 手写 Win32 FFI（与探针 crate 逐字同源，A~D 阶段实测形态）。
//! 不用 windows crate：探针侧七条血泪的验证载体就是这套声明，保持一致降低迁移风险。
#![allow(non_snake_case)]

use std::ffi::c_void;

pub type H = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    pub fn LoadLibraryW(name: *const u16) -> H;
    pub fn GetProcAddress(module: H, name: *const u8) -> H;
    pub fn GetLastError() -> u32;
    pub fn GetModuleFileNameW(module: H, buf: *mut u16, len: u32) -> u32;
    pub fn CreateNamedPipeW(
        name: *const u16,
        openmode: u32,
        pipemode: u32,
        maxinstances: u32,
        outbuf: u32,
        inbuf: u32,
        deftimeout: u32,
        sa: *const SECURITY_ATTRIBUTES,
    ) -> H;
    pub fn ConnectNamedPipe(h: H, ov: *const c_void) -> i32;
    pub fn DisconnectNamedPipe(h: H) -> i32;
    pub fn PeekNamedPipe(
        h: H,
        buf: *mut u8,
        bufsize: u32,
        read: *mut u32,
        avail: *mut u32,
        left: *mut u32,
    ) -> i32;
    pub fn ReadFile(h: H, buf: *mut u8, len: u32, n: *mut u32, ov: *const c_void) -> i32;
    pub fn WriteFile(h: H, buf: *const u8, len: u32, n: *mut u32, ov: *const c_void) -> i32;
    pub fn CloseHandle(h: H) -> i32;
    pub fn GetNamedPipeClientProcessId(h: H, pid: *mut u32) -> i32;
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> H;
    pub fn QueryFullProcessImageNameW(h: H, flags: u32, buf: *mut u16, len: *mut u32) -> i32;
    pub fn GetProcessTimes(
        h: H,
        created: *mut i64,
        exited: *mut i64,
        kernel: *mut i64,
        user: *mut i64,
    ) -> i32;
    pub fn GetCurrentProcess() -> H;
}

#[link(name = "advapi32")]
extern "system" {
    pub fn OpenProcessToken(process: H, access: u32, token: *mut H) -> i32;
    pub fn GetTokenInformation(
        token: H,
        class: i32,
        info: *mut u8,
        len: u32,
        retlen: *mut u32,
    ) -> i32;
    pub fn ConvertSidToStringSidW(sid: *const c_void, strp: *mut *mut u16) -> i32;
    pub fn LocalFree(h: H) -> H;
    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        sd: *mut *mut c_void,
        sdlen: *mut u32,
    ) -> i32;
}

#[link(name = "user32")]
extern "system" {
    pub fn GetShellWindow() -> H;
    pub fn GetWindowThreadProcessId(hwnd: H, pid: *mut u32) -> u32;
    pub fn FindWindowW(cls: *const u16, name: *const u16) -> H;
    pub fn SetWindowsHookExW(id: i32, proc: H, module: H, tid: u32) -> H;
    pub fn UnhookWindowsHookEx(hhk: H) -> i32;
    pub fn SendMessageTimeoutW(
        hwnd: H,
        msg: u32,
        wp: usize,
        lp: isize,
        flags: u32,
        timeout: u32,
        res: *mut usize,
    ) -> isize;
}

#[repr(C)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: u32,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: i32,
}

#[repr(C)]
pub struct LOCAL_TIME {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

extern "system" {
    pub fn GetLocalTime(st: *mut LOCAL_TIME);
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn now_local_time() -> LOCAL_TIME {
    let mut st = LOCAL_TIME {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    st
}

pub fn hex(hr: i32) -> String {
    format!("0x{:08X}", hr as u32)
}
