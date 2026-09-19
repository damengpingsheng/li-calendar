//! 注入式任务栏时钟——会话管理器（方案 v2 §4.1，E0 生产迁移）。
//!
//! 探针 crate（`src-tauri/clockbar/`，A~D 阶段验收工件）数据链/会话逻辑的生产化移植，
//! 协议面与 tap v48 完全兼容（tap 侧零改动）。与探针形态的差异：
//! 1. pipe 服务端带 DACL（限 SYSTEM/Administrators/当前用户）+ 客户端身份校验（对端必须 explorer.exe）；
//! 2. TAP DLL 路径优先取部署目录（exe 同目录 `lical_clock_tap50.dll`），回退工程 bin 路径（dev）；
//! 3. 天气城市缓存路径改为安装目录 `clockbar_city.txt`（探针用 D:\agents_tmp 会话目录）；
//! 4. 会话生命周期由 watch 线程持有（explorer PID 监视→建会话→心跳→重注入），随注入开关启停；
//! 5. 天气失败降级纪律不变：一律 `--` 占位，绝不影响其余段与 explorer。
//!
//! FFI 采用手写声明（与探针逐字同源，A~D 阶段已实测），不引入 windows crate 管线特性。

#[cfg(windows)]
pub mod data;
#[cfg(windows)]
pub mod ffi;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod watch;
#[cfg(windows)]
pub mod weather;

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Mutex;

/// 注入开关（用户决策 #4：默认开启）。关闭时仅原生时钟，且互斥门放行旧覆盖层。
static INJECTION_ENABLED: AtomicBool = AtomicBool::new(true);

/// 注入会话已启动守卫（防 watch 线程重复 spawn）。
static STARTED: AtomicBool = AtomicBool::new(false);

/// 会话线程停止令牌（shutdown 时置位，watch/data 线程检测后退出）。
pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// 数据链重初始化标志（v57 休眠唤醒修复）：tap 侧 AUTO 恢复（心跳兜底或断管重连）
/// 会摘面板并清 panelWanted，而 watch 心跳在新管道实例上正常=watch 无感；data 线程
/// 检测到本标志后清 last_sent（强制重发当前行重建面板）并立即刷新天气。
pub(crate) static DATA_REINIT: AtomicBool = AtomicBool::new(false);

pub(crate) fn dbg_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"D:\agents_tmp\clockbar_host.log")
    {
        let ts = crate::timestamp_string();
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 注入开关当前状态（供旧覆盖层互斥门查询）。
pub fn injection_enabled() -> bool {
    INJECTION_ENABLED.load(Ordering::SeqCst)
}

/// TAP DLL 路径：优先部署目录（exe 同目录），回退工程 bin 路径（dev 场景）。
#[cfg(windows)]
pub fn tap_dll_path() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("lical_clock_tap60.dll");
            if p.exists() {
                return Some(p);
            }
        }
    }
    let dev = std::path::PathBuf::from(
        r"D:\project\li-calendar\src-tauri\clockbar\bin\lical_clock_tap60.dll",
    );
    dev.exists().then_some(dev)
}

/// 注入式时钟样式（S 阶段）。会话外全局持有：设置命令写入，数据线程每 tick 读取
/// 并组进 c1set（行变化才发送 → 样式变更 ≤1s 生效，无需重启 explorer/会话）。
#[cfg(windows)]
static STYLE: Mutex<Option<crate::app_runtime::config::ClockbarStyleConfig>> = Mutex::new(None);

/// 应用样式（设置页命令/启动时从 liConfig 载入）。
#[cfg(windows)]
pub fn apply_style(cfg: crate::app_runtime::config::ClockbarStyleConfig) {
    dbg_log("clockbar: style updated");
    if let Ok(mut g) = STYLE.lock() {
        *g = Some(cfg);
    }
}

/// 当前样式快照（无配置=None → 数据线程不下发 style 扩展=现行为）。
#[cfg(windows)]
pub fn current_style() -> Option<crate::app_runtime::config::ClockbarStyleConfig> {
    STYLE.lock().ok().and_then(|g| g.clone())
}

/// 应用注入开关（E0/E6）。开启=启动会话管理器；关闭=优雅拆除会话（tap 断线自动恢复原生时钟）。
pub fn apply_injection(enabled: bool) {
    INJECTION_ENABLED.store(enabled, Ordering::SeqCst);
    if enabled {
        start();
    } else {
        shutdown();
    }
}

/// 启动会话管理器（pipe 服务端 + watch 生命周期线程）。幂等。
pub fn start() {
    if !STARTED.swap(true, Ordering::SeqCst) {
        STOP.store(false, Ordering::SeqCst);
        dbg_log("clockbar: session manager starting");
        pipe::start_server();
        watch::spawn_watch();
    }
}

/// 优雅拆除：置 STOP → 显式恢复序列（c1free 摘面板 + unadvise）→ 断开管道。
/// tap 侧断线即走 AUTO 恢复（B0 语义），双保险。
pub fn shutdown() {
    if STARTED.swap(false, Ordering::SeqCst) {
        dbg_log("clockbar: session manager shutting down");
        STOP.store(true, Ordering::SeqCst);
        session::detach_best_effort();
        pipe::disconnect_server();
        dbg_log("clockbar: shutdown done");
    }
}
