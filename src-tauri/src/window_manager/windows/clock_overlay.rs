//! 任务栏时钟覆盖层（Phase 0 覆盖式接管）：不透明置顶窗口盖在系统时钟上，
//! 由前端 `ClockOverlayWindow` 渲染「时间 + 农历 + 天气」。
//!
//! 本阶段只接管「显示与悬停」：窗口不再鼠标穿透（`WS_EX_NOACTIVATE`，点击不抢
//! 前台焦点），悬停落在覆盖层上、系统时钟 XAML tooltip 从此不再触发；时钟区的
//! 点击仍由低级钩子整体吞掉（Phase 2 才交接输入），交互行为零变化。
//! 几何贴合复用 UIA 时钟矩形缓存；探测失败时保持隐藏——原生时钟兜底。
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetMonitorInfoW, GetPixel, MonitorFromWindow, ReleaseDC, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetAncestor, GetWindowLongPtrW, GetWindowRect, IsWindowVisible,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE,
    HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use super::get_window_hwnd;

/// 最近一次应用到覆盖层的时钟矩形（变化检测：矩形没变就不动窗口，避免周期重贴抖动）。
static LAST_APPLIED_RECT: Mutex<Option<RECT>> = Mutex::new(None);
/// 任务栏「静止位」矩形（贴底/贴边时的位置）；跟随位移以此为基准计算偏移。
static TRAY_HOME_RECT: Mutex<Option<RECT>> = Mutex::new(None);
/// 最近一次应用到的状态 (x, y, 宽, 高, below)：完全一致才跳过 SetWindowPos。
/// below_hwnd=-1 表示常规 topmost；z 序/尺寸变化必须连同位置一起比较（从
/// 「潜入盖住者下方」恢复 topmost 时位置可能完全相同；遮盖展开/收缩尺寸不同）。
static LAST_FOLLOW_POS: Mutex<Option<(i32, i32, i32, i32, isize)>> = Mutex::new(None);
/// 当前生效的 z 序模式（-1=常规 topmost，OFFSCREEN_MARK=屏外，其他=潜入到该
/// 窗口下方）。供 2s 重贴线程判断：常规模式才允许重申 topmost，潜入/屏外
/// 模式下重申 topmost 会把覆盖层顶回全屏窗口之上（实测打架）。
static CURRENT_BELOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(-1);
/// 「退出全屏自愈」进行中标记（防重复 spawn 治疗线程）。
static EXIT_HEALING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 几何应用串行锁（R8.1）：relocate 收缩路径与 update 可见性路径在不同线程
/// 并发应用几何时，可能以各自**刚读取的状态**交错 SetWindowPos——00:56:49.120
/// 实测竞态序列：WinEvent 线程按读取时仍在场的遮盖贴 212 宽，收缩线程尾随的
/// 常规跟随带 SWP_NOSIZE 落在 endorsed 左缘 → 窗口=「endorsed 左缘+遮盖宽度」
/// (3649..3861)，右缘冲出屏幕 21px（用户截图「时钟跑到任务栏最右侧」）；
/// 且变化检测元组记的是 NOSIZE 标志而非窗口实际尺寸，错误尺寸被判「无变化」
/// 无限驻留（实测卡 16.6s 直到下轮全屏才被遮盖全尺寸应用治愈）。锁序恒为
/// 本锁→GEOM（note_probe/轮询节奏只取 GEOM，无反向依赖，无死循环风险）。
static GEOM_APPLY_LOCK: Mutex<()> = Mutex::new(());
/// 覆盖层已完成首次贴合（attach）标记（R7.3）：attach 前窗口仍是 builder
/// 逻辑尺寸（170×52 逻辑 = 298×91 物理 @175%），visibility 兜底/WinEvent
/// 若在 attach 的 500ms 重试间隙应用几何（NOSIZE+SHOWWINDOW），窗口会以
/// 过宽尺寸闪现、右缘伸进「显示桌面」区（02:03:19 实测）。attach 前只
/// 维护状态机，不做窗口操作。
static ATTACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 覆盖层自身 HWND 缓存（R8.2 闭环校准用）：内部点（覆盖层右缘内 2px）的
/// 归属校验必须命中本窗口根——保证读到的「我们实际显示色」确实出自覆盖层，
/// 而不是同位置的其他内容。
static OVERLAY_HWND: Mutex<Option<isize>> = Mutex::new(None);

/// 覆盖层几何状态机阶段（P1+P2）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum GeomPhase {
    /// 常规：探测差异需经采纳门（候选持续 ≥8s）才更新认可矩形
    Normal,
    /// 被盖（全屏窗口压住任务栏）：几何冻结，探测样本记录原生位置用于预备遮盖
    Covered,
}

struct GeomState {
    phase: GeomPhase,
    /// 认可矩形：覆盖层内容文字的锚定位置（与钩子点击路由缓存解耦——
    /// 缓存如实跟踪全屏期布局 3660→3618，覆盖层内容绝不直接消费它）
    endorsed: Option<RECT>,
    /// 待采纳候选（与认可位不同的探测样本）
    candidate: Option<RECT>,
    /// 候选首次出现时刻
    candidate_since: Option<std::time::Instant>,
    /// 被盖期间观测到的原生时钟真实位置（N1 掩盖预备数据；跨轮保留作下轮预测）
    native_observed: Option<RECT>,
    /// 当前遮盖矩形（窗口被临时扩展为认可∪原生观测时的实际窗口矩形）
    mask: Option<RECT>,
    /// 收缩请求：UIA 读到原生==认可位即置位（R6 实时跟踪——扩展/收缩与
    /// 原生时钟的真实位置逐读数对齐，归位即缩，不做额外稳定等待）
    shrink_requested: bool,
    /// 退出观测窗截止时刻（R7）：收缩执行后 EXIT_WATCH_MS 内——轮询维持
    /// 150ms、Normal 相位的偏离读数继续喂遮盖数据。修 R6 的洞：首次收缩
    /// 清空 mask/native_observed 后轮询掉回 500ms/每 4 针才 UIA，偏离读数
    /// 只进 8s 候选路径（23:39 实测首个收缩后回摆仍持续 ~2.4s，遮盖再扩展
    /// 最坏要等 ~2.5s）。观测窗只延长跟踪，绝不重新变成延迟收缩的门槛。
    exit_watch_until: Option<std::time::Instant>,
    /// 最近一次覆盖探测 Visible 的时刻（供轮询加密判断：退出动画期相位
    /// 回摆时即使瞬时回到 Covered 也保持 150ms 节奏）
    last_probe_visible: Option<std::time::Instant>,
    /// 覆盖探测去抖：离开被盖态（Visible）的连续出现起点
    visible_since: Option<std::time::Instant>,
    /// 最近一次盖住者的窗口句柄（去抖等待期保持潜入 z 序用）
    last_cover: Option<isize>,
    /// 全屏位形记忆（R7.2）：跨退出轮保留的最后一次**非认可位**原生观测。
    /// 收缩时 native_observed 立即清空，但此字段保留——下一轮 Covered 探测
    /// 当轮即可按它展开遮盖，不等首针 UIA（帧取证 01:57 实测：暴露窗
    /// 191~323ms 期间原生时钟文字残块/托盘气泡可见，是真机残余感知主体；
    /// 全屏位形逐轮稳定 (3607,2076,3770,2160)，预测可靠）。endorsed 采纳后
    /// 若与记忆同值，union 会等于 endorsed 自然失效，无需专门清理。
    last_native_layout: Option<RECT>,
}

static GEOM: Mutex<GeomState> = Mutex::new(GeomState {
    phase: GeomPhase::Normal,
    endorsed: None,
    candidate: None,
    candidate_since: None,
    native_observed: None,
    mask: None,
    shrink_requested: false,
    exit_watch_until: None,
    last_probe_visible: None,
    visible_since: None,
    last_cover: None,
    last_native_layout: None,
});

/// 采纳门：与认可位不同的候选需**持续存在 ≥8s** 才采纳——
/// 退出全屏/进入全屏的过渡态布局（宽矩形、全屏期布局值）存活仅零点几到几秒，
/// 永远达不到门槛，结构性无法被采纳（PotPlayer 慢退出实测可骗过 500ms 门）；
/// 真实布局重排（图标增减/DPI 变更等持久变化）8s 后正常跟进，代价可忽略。
/// 注意：这是防抖参数，不是"连续观测 8s"的强保证（中间探测失败会累计时长）。
const PERSIST_ADOPT_MS: u128 = 8000;

/// 覆盖探测去抖：被盖→常规要求 Visible 持续存在 ≥300ms 才切换。
/// 实测：PotPlayer 退出全屏的窗口缩回动画使覆盖探测以 ~1.4s 周期在
/// Covered/Visible 间抖动，状态机随之扩张/收缩循环（用户见时钟宽度反复变化、
/// 喇叭图标反复被盖）。去抖后整个动画期保持被盖几何一次，动画结束一次性收缩。
/// 进入被盖不去抖（盖住就该立即遮）。
const COVER_DEBOUNCE_MS: u128 = 300;

/// 退出观测窗时长（R7）：收缩执行后维持快节奏跟踪的时长。23:39 实测退出
/// 回摆全程 ~5s、首个收缩后仍持续 ~2.4s；6s 覆盖「最后一个收缩之后仍可能
/// 出现的回摆尾巴」。窗口到期自然回落稳态节奏，无需显式清理。
const EXIT_WATCH_MS: u64 = 6000;

/// RECT 字段级比较（不依赖 derive）。
fn rect_eq(a: Option<RECT>, b: Option<RECT>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
        }
        (None, None) => true,
        _ => false,
    }
}

/// 两个矩形的并集（N1 掩盖矩形 = 认可位 ∪ 原生观测位）。
fn union_rect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

/// 几何应用诊断日志（标记文件门控，随动跟随动画期间会逐帧触发）。
fn geom_log(msg: &str) {
    if std::path::Path::new(r"D:\agents_tmp\clockrect_debug").exists() {
        crate::dbg_log(&format!("clockrect: {msg}"));
    }
}

/// 重探线程轮询间隔建议（毫秒）。加密（150ms）只在瞬态：
/// ① 退出 pending（遮盖在场、相位 Normal）——收缩/扩展时点与原生归位/离位
///    时点的量化差即一次轮询（R6 实时跟踪的基础）；
/// ② 被盖但遮盖未展开（进全屏头一秒，等首个 UIA 观测）——残块暴露窗；
/// ③ 退出回摆期相位瞬时回到 Covered，但 3s 内见过 Visible——回摆仍在进行，
///    掉回 2s 节奏会让下一次扩展/收缩晚一个量级（23:39 实测 42.5→44.1 的
///    UIA 空窗正是 R4 掉回慢节奏所致）；
/// ④ 退出观测窗内（R7，收缩后 EXIT_WATCH_MS）：首次收缩清空遮盖数据后
///    回摆可能仍在进行，此窗内维持 150ms 保证后续回摆读数逐针可见。
/// 稳态全屏播放（遮盖在场、相位 Covered、3s 内无 Visible）维持 500ms/2s，
/// 避免 UIA 重操作整场加密。
pub fn clock_overlay_reprobe_interval_ms() -> u64 {
    match GEOM.lock() {
        Ok(g) => {
            let now = std::time::Instant::now();
            let recent_visible = g
                .last_probe_visible
                .map(|t| t.elapsed().as_millis() < 3000)
                .unwrap_or(false);
            let in_exit_watch = g.exit_watch_until.map(|t| t > now).unwrap_or(false);
            match g.phase {
                GeomPhase::Normal if g.mask.is_some() || in_exit_watch => 150,
                GeomPhase::Covered if g.mask.is_none() => 150,
                GeomPhase::Covered if g.mask.is_some() && (recent_visible || in_exit_watch) => 150,
                _ => 500,
            }
        }
        Err(_) => 500,
    }
}

/// 探测结果喂给几何状态机（P1 布局解耦入口）。
///
/// 缓存矩形属于钩子点击路由（需真实当前布局，含全屏期布局）；覆盖层只消费
/// 认可矩形。注册表回退样本不是新观测，不得调用本函数。
/// 钩子回调路径会调用本函数：只做 Mutex 操作与罕见日志，保持轻量。
pub fn clock_overlay_note_probe(rect: RECT) {
    let Ok(mut g) = GEOM.lock() else {
        return;
    };
    if g.phase == GeomPhase::Covered {
        // 被盖期间的真实原生位置：记录为遮盖预备数据（N1），不影响认可矩形。
        // 跨相位翻转保留（R5）：PotPlayer 退出动画使相位以 ~1.4s 周期
        // Covered↔Visible 回摆，逐轮清空会把回摆证据丢掉、遮盖展开延迟整轮。
        g.native_observed = Some(rect);
        // R7.2 全屏位形记忆：非认可位读数即位形证据，跨轮保留供下轮
        // Covered 预测展开（收缩清 native_observed 不清此字段）。
        if !rect_eq(g.endorsed, Some(rect)) {
            g.last_native_layout = Some(rect);
        }
        // UIA 读到认可位：原生真实归位，立即请求收缩（R6 实时跟踪）。
        // 退出动画期原生位置逐读数回摆（23:39 实测 3618↔3660 四个来回），
        // 任何"稳定 N 次才缩"的守门都会让左缘在原生已归位后继续撑着——
        // 扩展/收缩与原生位置逐读数对齐才是与系统自身布局跳动同步的最小
        // 感知。fast-path 相位/z 兼职：盖住者已让位时提前夺回 topmost。
        if rect_eq(g.endorsed, Some(rect)) {
            if g.mask.is_some() && !g.shrink_requested {
                g.shrink_requested = true;
                crate::dbg_log("clockrect: shrink requested (native endorsed)");
            }
            if g.mask.is_some() {
                g.phase = GeomPhase::Normal;
                g.visible_since = None;
                crate::dbg_log("clockrect: phase->normal (uia native-back fast-path)");
            }
        } else {
            g.shrink_requested = false;
        }
        return;
    }
    if rect_eq(g.endorsed, Some(rect)) {
        // 与认可位一致：遮盖在场即立即请求收缩（R6 实时跟踪）
        if g.mask.is_some() && !g.shrink_requested {
            g.shrink_requested = true;
            crate::dbg_log("clockrect: shrink requested (native endorsed)");
        }
        if g.candidate.is_some() {
            g.candidate = None;
            g.candidate_since = None;
            crate::dbg_log("clockrect: candidate cleared (probe==endorsed)");
        }
        return;
    }
    // 与认可位不同。退出轮中（遮盖或原生观测任一在场，或处于退出观测窗）：
    // 读数即原生回摆的证据，喂遮盖数据并撤销收缩请求——Normal 相位的回摆
    // 读数不再丢失到候选路径（23:39 实测：40.8s 的 3618 读数落在相位翻转
    // 间隙，遮盖晚了 1.4s 才展开，期间原生残块露出）。R7 补观测窗条件：
    // 首次收缩清空 mask/native 后回摆读数仍能立即重建遮盖依据。
    let in_exit_watch = g
        .exit_watch_until
        .map(|t| t > std::time::Instant::now())
        .unwrap_or(false);
    if g.mask.is_some() || g.native_observed.is_some() || in_exit_watch {
        g.native_observed = Some(rect);
        g.shrink_requested = false;
        if !rect_eq(g.endorsed, Some(rect)) {
            g.last_native_layout = Some(rect);
        }
    }
    // 与认可位不同：走采纳门（候选需持续存在 ≥8s），绝不即时采纳——
    // 退出全屏的过渡期宽矩形/全屏期布局值就是这么混进去的（实测）；
    // PotPlayer 慢退出的过渡布局可存活数秒，500ms 门曾被骗过（实测）。
    let same_as_candidate = rect_eq(g.candidate, Some(rect));
    let elapsed = g.candidate_since.map(|t| t.elapsed().as_millis()).unwrap_or(0);
    if same_as_candidate && elapsed >= PERSIST_ADOPT_MS {
        g.endorsed = Some(rect);
        g.candidate = None;
        g.candidate_since = None;
        crate::dbg_log(&format!(
            "clockrect: endorsed adopt ({},{},{},{})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
    } else if !same_as_candidate {
        g.candidate = Some(rect);
        g.candidate_since = Some(std::time::Instant::now());
    }
    // 同候选但持久时长不足：继续等下一针
}

/// 构建任务栏时钟覆盖层窗口（独立、透明画布、置顶、无边框、隐藏待贴合）。
impl super::CalendarWindowManager {
    pub fn build_clock_overlay_window(app_handle: &AppHandle) -> Option<WebviewWindow> {
        let mut builder = tauri::WebviewWindowBuilder::new(
            app_handle,
            "clock_overlay",
            tauri::WebviewUrl::App("index.html?window=clock_overlay".into()),
        );
        // 固定 WebView2 用户数据目录（主窗口数据同目录下子目录），
        // 避免非主窗口默认写入 %TEMP% 导致每次启动堆积 ~68MB EBWebView。
        builder = builder.data_directory(std::path::PathBuf::from(
            r"D:\Program Files\li-calendar\webview-data\clock-overlay",
        ));
        // 覆盖层会经历「移出屏幕/被全屏窗口盖住」的隐藏方式，Chromium 默认对
        // 这类窗口做遮挡节流（暂停合成），恢复显示后内容要几百 ms~2s 才画出来
        // （实测：移回 800ms 后仍是透明透出原生时钟）。显式禁用遮挡计算与
        // 渲染器后台降级——窗口仅 158×84，常驻合成成本可忽略。
        builder = builder.additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows --disable-renderer-backgrounding",
        );
        let window = builder
            .title("")
            .inner_size(170.0, 52.0)
            .resizable(false)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            // 先隐藏，探测到时钟矩形并贴合后再无激活显示；探测失败保持隐藏（原生时钟兜底）
            .visible(false)
            .focused(false)
            .skip_taskbar(true)
            .shadow(false)
            .build()
            .ok()?;
        // 覆盖式接管：不再鼠标穿透——悬停落在覆盖层，系统时钟 tooltip 不再触发；
        // 点击仍被钩子整体吞掉（Phase 2 才交接）。NOACTIVATE 保证收到输入也不抢前台焦点，
        // TOOLWINDOW 避免 Alt+Tab 出现覆盖层条目。
        if let Some(hwnd) = get_window_hwnd(&window) {
            unsafe {
                let style = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
                let _ = SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    (style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0 as isize,
                );
            }
        }
        Some(window)
    }
}

/// 供低级钩子等模块获取指定标签窗口的 Win32 句柄。
/// （时钟点击判定使用覆盖窗口矩形：可见的自定义时钟在哪里，点击区就在哪里。）
pub fn window_hwnd_by_label(app_handle: &AppHandle, label: &str) -> Option<HWND> {
    app_handle.get_webview_window(label).and_then(|w| get_window_hwnd(&w))
}

/// Phase 0 启动入口：构建覆盖层（隐藏）并等待 UIA 时钟矩形就绪后贴合显示。
///
/// UIA 初始化在钩子消息线程上，时序可能晚于此处；每 500ms 重试、最多 10 秒。
/// 已存在窗口时（重复调用/心跳续跑）直接重贴，不重复建窗。
pub fn ensure_clock_overlay_attached(app_handle: &AppHandle) {
    // UIA 时钟矩形探测依赖 COM（与钩子消息线程同要求），本线程必须自行初始化
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let window = match app_handle.get_webview_window("clock_overlay") {
        Some(existing) => existing,
        None => {
            let Some(built) = super::CalendarWindowManager::build_clock_overlay_window(app_handle)
            else {
                crate::dbg_log("clock overlay: build_clock_overlay_window failed");
                return;
            };
            built
        }
    };
    for attempt in 0..20 {
        crate::windows_hook::refresh_clock_area_cache();
        let rect = crate::windows_hook::CLOCK_AREA_RECT_CACHE
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().copied());
        if let Some(rect) = rect {
            if apply_overlay_geometry(&window, &rect) {
                if let Some(hwnd) = get_window_hwnd(&window) {
                    show_overlay_above_taskbar(hwnd);
                    ATTACHED.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Ok(mut h) = OVERLAY_HWND.lock() {
                        *h = Some(hwnd.0 as isize);
                    }
                    crate::dbg_log(&format!(
                        "clock overlay: attached at attempt {attempt} rect=({},{})-({},{})",
                        rect.left, rect.top, rect.right, rect.bottom
                    ));
                    // R8：attach 即刻实采一次（前端此时的过渡色只是主题近似值），
                    // 同时惰性启动外观 worker 与 15s 可信色保鲜线程——保鲜必须
                    // 从会话开始就积累，否则首轮退出全屏时的底色仍是陈旧的
                    refresh_clock_overlay_appearance(app_handle, 0, "attach");
                    return;
                }
            }
        }
        if attempt == 3 || attempt == 10 {
            crate::dbg_log(&format!(
                "clock overlay: rect not ready at attempt {attempt}, retrying"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!("clock overlay: 10s 内未探测到时钟矩形，覆盖层保持隐藏（原生时钟兜底）");
    crate::dbg_log("clock overlay: gave up after 10s, staying hidden");
}

/// 刷新时钟矩形缓存并重贴（前端命令 / 开关重开路径用）。
pub fn relocate_clock_overlay(app_handle: &AppHandle) {
    crate::windows_hook::refresh_clock_area_cache();
    relocate_clock_overlay_endorsed(app_handle);
}

/// 直接按缓存矩形重贴（不再触发 UIA，供 2 秒周期重探线程复用其刷新结果）。
/// 按认可矩形重贴（P1：几何唯一权威）。仅在 Normal 阶段应用；遮盖展开期间
/// 保持遮盖，直到 UIA 读到原生==认可位（shrink_requested，note_probe 置位）
/// 才收缩——R6 实时跟踪：收缩时点=原生真实归位时点+一次读数量化，
/// 与系统自身布局跳动同步，无任何附加稳定等待。
/// R7 扩展即时化（与收缩对称）：不收缩的场合若原生观测偏离认可位且现有
/// 遮盖盖不住它，立即把遮盖更新为新并集——原实现此分支直接 return，新
/// 偏离要等下一轮 update 才生效，扩展比收缩慢一个轮询周期。
/// R7.1 Covered 相位即时展开：重探线程的调用序是 update_visibility（tick N
/// 探测）→ UIA 读数 → relocate——读数若显示原生已偏离而遮盖未展开，本函数
/// 原先直接 return，展开要等下一轮 tick（≤150ms），期间原生时钟左段残块
/// （3618..3660）暴露在认可位覆盖层左侧（23:50 真机实测每趟摆动都有
/// ~150-300ms 暴露窗）。读数到达即唤起可见性管理完成展开，暴露窗压到
/// 探测耗时级（~70ms，UIA 查询延迟为下限）。
/// 收缩动画进行中标记（R8.7）：动画期间 update/relocate 跳过几何应用，
/// 避免 150ms 探针针把滑入过程打断成跳变；终态由动画自身精确落位。
static SHRINK_ANIMATING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn relocate_clock_overlay_endorsed(app_handle: &AppHandle) {
    // R8.1：几何应用持串行锁执行；可见性管理自取同一把锁，必须锁外调用。
    // R8.7/R8.8：锁内返回收缩动画请求并已置位 SHRINK_ANIMATING 门（持锁内置位，
    // 消除「锁释放后置位」的竞态间隙，复审 §6.3）；滑入锁外执行（持锁睡眠会
    // 阻塞全部几何路径），且每步核对最新状态（复审 §6.2：原生回摆/快速再入时
    // 不得滑向旧终点）。
    let (need_update, anim) = relocate_geometry_locked(app_handle);
    if let Some((from_left, anim_endorsed)) = anim {
        let mut aborted = false;
        if let Some(window) = app_handle.get_webview_window("clock_overlay") {
            if let Some(hwnd) = get_window_hwnd(&window) {
                let right = anim_endorsed.right;
                let top = anim_endorsed.top;
                let h = anim_endorsed.bottom - anim_endorsed.top;
                let total = anim_endorsed.left - from_left;
                // 4 步 × ~25ms ≈ 100ms 滑入：中间矩形 [x, right] 恒 ⊇ 收缩前
                // 遮盖态，原生时钟（已读到 endorsed 位）全程零暴露；42px 瞬移
                // 是复验⑪录屏「收缩抖动」的感知来源，平滑滑入显著弱化。
                let steps = 4i32;
                for k in 1..=steps {
                    let covered_again = GEOM
                        .lock()
                        .ok()
                        .map(|g| g.phase == GeomPhase::Covered || g.mask.is_some())
                        .unwrap_or(false);
                    if covered_again {
                        aborted = true;
                        geom_log("clockrect: shrink anim aborted (covered again)");
                        break;
                    }
                    let x = from_left + total * k / steps;
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            x,
                            top,
                            right - x,
                            h,
                            SWP_NOACTIVATE | SWP_NOZORDER,
                        );
                    }
                    if k < steps {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
                if !aborted {
                    let _ = apply_overlay_geometry(&window, &anim_endorsed);
                    geom_log("clockrect: shrink animated (slide-in done)");
                }
            } else {
                aborted = true;
            }
        } else {
            aborted = true;
        }
        if aborted {
            // 中止后不落终态：重新被盖时遮盖流程下一针自行应用其几何；
            // 未被再盖的中止（窗口不可得等罕见路径）下一针 relocate 收敛。
            // 当前中间矩形 [x, right] 仍 ⊇ endorsed，原生不会因此暴露。
        }
        SHRINK_ANIMATING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    if need_update {
        update_clock_overlay_visibility(app_handle);
    }
}

/// relocate 几何本体（持 [`GEOM_APPLY_LOCK]`）。返回 (是否需随后调用可见性
/// 管理, 收缩动画需求 (动画起点左缘, 终态 endorsed))——锁内不可重入调用
/// update（std Mutex 不可重入，会自锁死）；动画在锁外执行（见上）。
fn relocate_geometry_locked(app_handle: &AppHandle) -> (bool, Option<(i32, RECT)>) {
    let _apply_guard = GEOM_APPLY_LOCK.lock();
    // 动画进行中：任何几何路径直接让位（滑入由动画自身收尾，终态落位后自愈）
    if SHRINK_ANIMATING.load(std::sync::atomic::Ordering::SeqCst) {
        return (false, None);
    }
    let (endorsed, phase, mask, shrink_requested, native) = match GEOM.lock() {
        Ok(g) => (g.endorsed, g.phase, g.mask, g.shrink_requested, g.native_observed),
        Err(_) => return (false, None),
    };
    if phase == GeomPhase::Covered {
        if mask.is_none() {
            if let (Some(e), Some(n)) = (endorsed, native) {
                if !rect_eq(Some(n), Some(e)) {
                    // 原生偏离证据已到手而遮盖未展开：立即展开（内含探测、
                    // 遮盖构建与成套应用；若无盖住者则走 Visible 分支路径）
                    return (true, None);
                }
            }
        }
        return (false, None);
    }
    if !shrink_requested {
        if let (Some(endorsed), Some(native)) = (endorsed, native) {
            if !rect_eq(Some(native), Some(endorsed)) {
                let u = union_rect(endorsed, native);
                if !rect_eq(mask, Some(u)) {
                    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
                        return (false, None);
                    };
                    if apply_mask_geometry(&window, u) {
                        crate::dbg_log(&format!(
                            "clockrect: mask expand (native diverged) union=({},{},{},{})",
                            u.left, u.top, u.right, u.bottom
                        ));
                    }
                    return (false, None);
                }
                if mask.is_some() {
                    // 并集未变（旧遮盖已盖住新观测）：维持现状
                    return (false, None);
                }
            }
        }
    }
    if mask.is_some() && !shrink_requested {
        return (false, None); // 遮盖保持期：原生仍在全屏位形，收缩会露出残块
    }
    let Some(endorsed) = endorsed else { return (false, None) };
    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
        return (false, None)
    };
    if mask.is_some() {
        crate::dbg_log("clockrect: shrink (native endorsed)");
    }
    // R8.7 收缩动画：遮盖收缩（左缘左移 ≥12px）时不再瞬移落位，交由锁外
    // 滑入动画（中间矩形恒 ⊇ endorsed，原生零暴露）；微小位移仍直接应用。
    let from_left = LAST_APPLIED_RECT
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.left));
    let anim = if mask.is_some()
        && from_left.map(|l| endorsed.left - l >= 12).unwrap_or(false)
    {
        // R8.8：持锁内置位——锁释放与置位之间的间隙曾有竞态窗口（复审 §6.3）
        SHRINK_ANIMATING.store(true, std::sync::atomic::Ordering::SeqCst);
        Some((from_left.unwrap_or(endorsed.left), endorsed))
    } else {
        if !apply_overlay_geometry(&window, &endorsed) {
            return (false, None);
        }
        None
    };
    if let Ok(mut g) = GEOM.lock() {
        g.mask = None;
        g.native_observed = None;
        g.shrink_requested = false;
        // 真实收缩（遮盖→无）才开启退出观测窗：常规无变化重贴不开窗
        if mask.is_some() {
            g.exit_watch_until = Some(
                std::time::Instant::now() + std::time::Duration::from_millis(EXIT_WATCH_MS),
            );
            // R8.3：收缩后采样只保留延迟针（400/1500/4000ms）。R8 引入的
            // delay=0 即时采样是「持续色差」毒源（复验⑧日志实锤）：退出
            // 动画期亚克力正从视频透出过渡到壁纸透出，即时采样屡次采到
            // 过渡色推送（#F0E4DB/#E8D9CE），快速连续测试时纠正针又被
            // 下一次全屏盖住拒绝——色差驻留到 trusted 兜底才恢复。这与
            // R8.1 撤销 z-reclaim/visible-switch/mask-held-tick 同理：
            // 过渡态不采样，等世界稳定后再采纳。
            refresh_clock_overlay_appearance(app_handle, 400, "post-shrink-400");
            refresh_clock_overlay_appearance(app_handle, 1500, "post-shrink-1500");
            refresh_clock_overlay_appearance(app_handle, 4000, "post-shrink-4000");
        }
    }
    // z 序维护按当前模式分流：常规模式重申 topmost（防任务栏重申后压到覆盖层
    // 之上）；潜入模式改为重申"潜入盖住者下方"（外部 z 序扰动后 2s 内自愈）；
    // 屏外模式不动 z 序。
    let below = CURRENT_BELOW.load(std::sync::atomic::Ordering::SeqCst);
    if let Some(hwnd) = get_window_hwnd(&window) {
        unsafe {
            match below {
                -1 => {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                mark if mark > 0 => {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND(mark as *mut core::ffi::c_void)),
                        endorsed.left,
                        endorsed.top,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                _ => {}
            }
        }
    }
    // 可见性统一走跟随管理：覆盖层曾因矩形未就绪保持隐藏（如启动时任务栏正
    // 收起、UIA 探测失败），矩形就绪后由这里恢复显示或按全屏/滑出态维持隐藏。
    // （由持锁外层 relocate_clock_overlay_endorsed 调用，此处只报告需求；
    // 动画收缩时 z 序维护已照常执行（NOMOVE|NOSIZE 与滑入不冲突），几何
    // 由锁外滑入收尾）
    (true, anim)
}

/// 任务栏当前状态：是否完全滑出屏幕 + 相对静止位的位移（滑入/滑出动画的实时偏移）。
struct TrayState {
    /// 整体滑出所在显示器边缘（自动隐藏收起完成态）
    fully_hidden: bool,
    /// 相对静止位的双轴位移（静止时 (0,0)；滑出动画中 y>0——底部任务栏）
    dx: i32,
    dy: i32,
    /// 静止位基准是否已学习（未学习时不可跟随，覆盖层应保持隐藏）
    ready: bool,
}

/// 读取任务栏状态并顺带维护「静止位」基准。
fn read_tray_state() -> Option<TrayState> {
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if !IsWindowVisible(tray).as_bool() {
            return Some(TrayState { fully_hidden: true, dx: 0, dy: 0, ready: true });
        }
        let mut rect = RECT::default();
        if GetWindowRect(tray, &mut rect).is_err() {
            return None;
        }
        let hmon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        // 完全滑出判定：窗口整体在某边缘之外（留 ≤4px 唤出热区）
        const TOL: i32 = 4;
        let fully_hidden = rect.top >= m.bottom - TOL
            || rect.bottom <= m.top + TOL
            || rect.left >= m.right - TOL
            || rect.right <= m.left + TOL;
        // 静止位基准维护。两条铁律（皆有实测教训）：
        // 1) 不能用"贴住任一显示器边缘"判定静止——任务栏左右恒贴满屏幕宽，
        //    滑动全程都会误判静止、每帧刷新基准，dy 恒为 0，覆盖层不跟随。
        // 2) 首次记录不能用"无条件接受"——自动隐藏用户启动应用时任务栏多为
        //    收起态，收起位被记成静止位后，唤出时 dy=-82 覆盖层顶飞到屏幕中央。
        // 正确判定：静止位 ⇔ 任务栏矩形**完全在显示器内**（收起态与滑动中间态
        // 都必然跨出边缘）。基准记录后只在回到基准 ±TOL 时刷新；位移超过屏幕
        // 尺寸视为基准过期（分辨率变更），丢弃重学。
        let fully_inside = rect.left >= m.left
            && rect.right <= m.right
            && rect.top >= m.top
            && rect.bottom <= m.bottom;
        let screen_span = (m.right - m.left).abs().max((m.bottom - m.top).abs());
        let (dx, dy, ready) = if let Ok(mut home) = TRAY_HOME_RECT.lock() {
            match *home {
                Some(h) => {
                    let ddx = rect.left - h.left;
                    let ddy = rect.top - h.top;
                    if ddx.abs() > screen_span || ddy.abs() > screen_span {
                        // 基准过期（分辨率/显示器变更）：丢弃，等完全入屏后重学
                        *home = None;
                        (0, 0, false)
                    } else if ddx.abs() <= TOL && ddy.abs() <= TOL {
                        // 回到静止位：刷新基准（吸收静止位的微小漂移）
                        *home = Some(rect);
                        (0, 0, true)
                    } else {
                        // 滑动中：保持基准，返回实时位移
                        (ddx, ddy, true)
                    }
                }
                None => {
                    if fully_inside {
                        *home = Some(rect);
                        (0, 0, true)
                    } else {
                        (0, 0, false)
                    }
                }
            }
        } else {
            (0, 0, false)
        };
        Some(TrayState { fully_hidden, dx, dy, ready })
    }
}

/// 任务栏左段（避开右端覆盖层与托盘）的可见性判定点（横向 10%/25%/40%，中带）。
const TASKBAR_PROBE_FRACIONS: [f64; 3] = [0.10, 0.25, 0.40];

/// 任务栏覆盖探测结果。
enum TrayCover {
    /// 探测点命中任务栏（含其子窗口）：任务栏可见
    Visible,
    /// 被其他窗口盖住，携带盖住者的根窗口（全屏视频/游戏）
    Covered(HWND),
    /// 无法判定（找不到任务栏/探测点全部落空），调用方退回矩形判定
    Unknown,
}

/// 任务栏是否被**其他窗口**盖住（全屏视频/游戏 topmost 压住任务栏）。
///
/// 语义替代「前台窗口矩形铺满=全屏」：PotPlayer 等播放器退出全屏的过渡期
/// 窗口矩形仍铺满整屏长达 1~2s（矩形判定持续误判全屏，轮询再快也没用），
/// 而此时任务栏已实际露出。本判定用 `WindowFromPoint` 命中测试直接问
/// 「任务栏上这点此刻归谁」——像素级真实状态，不受窗口矩形动画时序干扰。
/// 三点投票（任一点命中任务栏即判可见；全部命中外部窗口取第一个根窗口）。
fn taskbar_cover_probe() -> TrayCover {
    unsafe {
        let tray = match FindWindowW(w!("Shell_TrayWnd"), None) {
            Ok(tray) => tray,
            Err(_) => return TrayCover::Unknown,
        };
        let mut rect = RECT::default();
        if GetWindowRect(tray, &mut rect).is_err() {
            return TrayCover::Unknown;
        }
        let width = rect.right - rect.left;
        let mid_y = (rect.top + rect.bottom) / 2;
        let mut cover: Option<HWND> = None;
        for frac in TASKBAR_PROBE_FRACIONS {
            let pt = windows::Win32::Foundation::POINT {
                x: rect.left + (width as f64 * frac) as i32,
                y: mid_y,
            };
            let hit = WindowFromPoint(pt);
            if hit.0.is_null() {
                continue;
            }
            let root = GetAncestor(hit, GA_ROOT);
            if root == tray || hit == tray {
                return TrayCover::Visible;
            }
            if cover.is_none() {
                cover = Some(root);
            }
        }
        match cover {
            Some(hwnd) => TrayCover::Covered(hwnd),
            None => TrayCover::Unknown,
        }
    }
}

/// Phase 1 可见性与跟随管理：任务栏被盖（全屏视频/游戏）时**z 序潜入盖住者
/// 正下方**（窗口留在屏内），任务栏滑入/滑出动画期间跟随其位移移动。
///
/// 教训一：隐藏绝不能用 `SW_HIDE`——WebView2(Chromium) 对隐藏窗口触发后台
/// 节流，重新显示后内容恢复渲染要几百 ms~2s，期间透出原生时钟（实测）。
/// 教训二：移出屏幕同样不出帧——Chromium 对完全离屏窗口暂停合成（加
/// `--disable-backgrounding-occluded-windows` 等参数、前端 200ms 强制重绘
/// 均无效，实测），移回后仍有 100~400ms 空窗。
/// 最终方案：被盖时把覆盖层 z 序插到盖住者正下方——窗口全程在屏内、渲染
/// 管线存活，盖住者（全屏窗口必然 topmost，否则盖不住 topmost 任务栏）缩回
/// 的瞬间覆盖层已在原位、内容已在，零空窗。
/// 由 WinEvent 与 500ms 兜底线程调用。
pub fn update_clock_overlay_visibility(app_handle: &AppHandle) {
    // R7.3：attach 前窗口尺寸未按认可矩形设置，任何几何应用都会以
    // builder 逻辑尺寸（298×91 物理）呈现——直接跳过，只让状态机等
    // attach 后的首次 relocate 收敛。
    if !ATTACHED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // R8.1：与 relocate 收缩路径串行化（锁序：本锁→GEOM）。WinEvent 线程/
    // 500ms 轮询线程/重探线程三方并发应用几何的交叉竞态曾把窗口落成
    // 「endorsed 左缘+遮盖宽度」（右缘 3861 出屏，见 GEOM_APPLY_LOCK 注释）。
    // R8.7：收缩滑入动画期间让位（动画自身收尾，终态后由 wrapper 的 update 收敛）。
    if SHRINK_ANIMATING.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _apply_guard = GEOM_APPLY_LOCK.lock();
    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
        return;
    };
    let Some(hwnd) = get_window_hwnd(&window) else {
        return;
    };
    // 认可矩形：覆盖层几何唯一权威（P1 布局解耦）。缓存矩形属于钩子点击路由，
    // 会如实跟踪全屏期布局（实测 3660→3618），覆盖层绝不直接消费它。
    // 首次 endorsed 为空时从缓存惰性播种（attach 首探已写入正常布局）。
    let endorsed = {
        let cache = crate::windows_hook::CLOCK_AREA_RECT_CACHE
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().copied());
        let endorsed = match GEOM.lock() {
            Ok(mut g) => {
                if g.endorsed.is_none() {
                    g.endorsed = cache;
                }
                g.endorsed
            }
            Err(_) => return,
        };
        endorsed
    };
    let Some(endorsed) = endorsed else {
        return;
    };
    // 状态五元组：(x, y, 宽, 高, 插入到谁之后)。R8.1 起宽高恒传全尺寸
    // （弃 NOSIZE：交叉竞态留下的错误尺寸会因变化检测命中「无变化」而永久
    // 驻留；显式尺寸让任何后写者自愈），
    // -1 = 常规 topmost，OFFSCREEN_MARK = 屏外。位置一律从认可矩形/遮盖矩形取，
    // 缓存矩形只进状态机不进几何。
    let (target_x, target_y, target_w, target_h, below) = match taskbar_cover_probe() {
        TrayCover::Covered(cover) => {
            if let Ok(mut g) = GEOM.lock() {
                g.last_cover = Some(cover.0 as isize);
                g.visible_since = None;
                if g.phase != GeomPhase::Covered {
                    g.phase = GeomPhase::Covered;
                    g.candidate = None;
                    g.candidate_since = None;
                    // 原生观测/遮盖/收缩请求跨相位翻转保留（R5/R6）：退出动画
                    // 期相位以 ~1.4s 周期回摆，逐轮清空会反复丢失回摆证据
                    // （23:39 实测遮盖因此晚 1.4s 展开）。证据与请求由
                    // note_probe 逐读数维护，此处不做清理。
                    crate::dbg_log(&format!(
                        "clockrect: phase->covered (hold endorsed ({},{},{},{}))",
                        endorsed.left, endorsed.top, endorsed.right, endorsed.bottom
                    ));
                }
            }
            // N1 掩盖预备（评审 §3.3 修正）：遮盖目标 = 已展开 mask 优先，
            // 否则本轮新观测的原生位置与认可位取并集；R7.2 补全屏位形记忆
            // （跨轮保留的最后非认可位观测）作最终回退——进全屏头一秒
            // 首针 UIA 未到时即可展开，消除 191~323ms 残块暴露窗。保持遮盖
            // 必须位置+尺寸成套应用——只回左缘不缩宽会让窗口变成"认可左缘
            // +遮盖宽度"，右缘冲进「显示桌面」区（盖住相邻图标，用户实测）。
            let cur_mask = GEOM.lock().ok().and_then(|g| g.mask);
            let native = GEOM
                .lock()
                .ok()
                .and_then(|g| g.native_observed.or(g.last_native_layout));
            let mask_target = cur_mask.or_else(|| {
                native.and_then(|n| {
                    let u = union_rect(endorsed, n);
                    (!rect_eq(Some(u), Some(endorsed))).then_some(u)
                })
            });
            if cur_mask.is_none() {
                geom_log(&format!(
                    "covered-fallback: native={native:?} target={mask_target:?}"
                ));
            }
            if let Some(m) = mask_target {
                if let Ok(mut g) = GEOM.lock() {
                    if !rect_eq(g.mask, Some(m)) {
                        g.mask = Some(m);
                        geom_log(&format!(
                            "mask prepared union=({},{},{},{})",
                            m.left, m.top, m.right, m.bottom
                        ));
                    }
                }
                // 同步"最后应用矩形"：遮盖应用绕过 apply_overlay_geometry 的
                // 变更记录，不同步会让收缩被"无变化"跳过
                if let Ok(mut last) = LAST_APPLIED_RECT.lock() {
                    *last = Some(m);
                }
                (
                    mask_target.unwrap().left,
                    mask_target.unwrap().top,
                    mask_target.unwrap().right - mask_target.unwrap().left,
                    mask_target.unwrap().bottom - mask_target.unwrap().top,
                    cover.0 as isize,
                )
            } else {
                // R8.1：全尺寸（弃 NOSIZE，理由见 Visible 分支注释）
                (
                    endorsed.left,
                    endorsed.top,
                    endorsed.right - endorsed.left,
                    endorsed.bottom - endorsed.top,
                    cover.0 as isize,
                )
            }
        }
        TrayCover::Visible => {
            // 记录最近 Visible 时刻（R6）：回摆期相位瞬时回到 Covered 时，
            // 轮询加密按此时刻维持 3s，扩展/收缩不吃 2s 量化。
            if let Ok(mut g) = GEOM.lock() {
                g.last_probe_visible = Some(std::time::Instant::now());
            }
            // 覆盖探测去抖：被盖→常规要求 Visible 持续 ≥300ms。实测 PotPlayer
            // 退出全屏的窗口缩回动画使覆盖探测以 ~1.4s 周期在 Covered/Visible
            // 间抖动，无去抖则状态机反复扩张/收缩（用户见时钟宽度反复变化、
            // 喇叭图标反复被盖）。去抖等待期窗口保持被盖几何（遮盖已展开），
            // 零操作；动画结束一次性切换并收缩。
            if let Ok(mut g) = GEOM.lock() {
                if g.phase == GeomPhase::Covered {
                    let since = *g.visible_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed().as_millis() < COVER_DEBOUNCE_MS {
                        // F1（评审完善版）：等待期内探测时钟中心实际归属——
                        // 命中任务栏或本覆盖层 ⇒ 盖住者已让位，立即夺回 topmost
                        // （仅 z 序，几何保持遮盖位），把"原生时钟可见期"从整个
                        // 去抖期压缩到 WinEvent 延迟级；命中其他窗口（含仍在
                        // 显示的播放器画面）不动作，避免浮到视频上。
                        // 夺 z 后清除应用缓存，使随后的真·Covered 能重新潜入。
                        drop(g);
                        unsafe {
                            if let Ok(tray) = FindWindowW(w!("Shell_TrayWnd"), None) {
                                let center = windows::Win32::Foundation::POINT {
                                    x: (endorsed.left + endorsed.right) / 2,
                                    y: (endorsed.top + endorsed.bottom) / 2,
                                };
                                let hit = WindowFromPoint(center);
                                if !hit.0.is_null() {
                                    let root = GetAncestor(hit, GA_ROOT);
                                    let own_root = GetAncestor(hwnd, GA_ROOT);
                                    if root == tray || root == own_root {
                                        let _ = SetWindowPos(
                                            hwnd,
                                            Some(HWND_TOPMOST),
                                            0,
                                            0,
                                            0,
                                            0,
                                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                        );
                                        geom_log("z-reclaim early (clock center hit tray/self)");
                                        if let Ok(mut last) = LAST_FOLLOW_POS.lock() {
                                            *last = None;
                                        }
                                        // R8.6 恢复此处的即时采样（R8.1 曾撤销）：
                                        // 复验⑨录屏逐帧分析证实，任务栏材质在
                                        // 全屏进出瞬间整体切换 ±9~24 亮度并持续
                                        // 数秒——那不是「过渡毒色」而是表面真实
                                        // 颜色，不跟随才是色差来源（滞后 0.4~3s
                                        // 的分叉爆发被肉眼捕捉）。R8.4 左列主参考
                                        // +R8.2 闭环+渐变就位后，跟随已无副作用。
                                        refresh_clock_overlay_appearance(app_handle, 0, "z-reclaim");
                                    }
                                }
                            }
                        }
                        return;
                    }
                    g.phase = GeomPhase::Normal;
                    g.visible_since = None;
                    crate::dbg_log(&format!(
                        "clockrect: phase->normal (exit cover, endorsed ({},{},{},{}), mask held)",
                        endorsed.left, endorsed.top, endorsed.right, endorsed.bottom
                    ));
                    // R8.6 恢复退出确认 Visible 瞬间换色（R7.6 建立、R8.1 撤销）：
                    // 材质切换是表面真实颜色，早一针跟随早一针消除分叉。
                    refresh_clock_overlay_appearance(app_handle, 0, "visible-switch");
                }
            }
            // 遮盖保持期（R6 实时跟踪）：退出轮回摆期探测可能直接 Visible 而原生
            // 仍在全屏位形（探测只看任务栏左段，代表不了时钟区）——此时候盖
            // 未展开也必须立即展开，否则原生残块压在托盘区露出（23:39 实测
            // 40.575/41.722 两个暴露窗）。收缩不在此处：由 relocate 在
            // note_probe 置位收缩请求后执行。
            let mut mask_now = GEOM.lock().ok().and_then(|g| g.mask);
            if mask_now.is_none() {
                // 只信当轮真实观测（R7.1 语义，R7.4 恢复）：全屏位形记忆
                // （last_native_layout）绝不能在这里做 fallback——退出稳态/
                // 观测窗内收缩刚清空观测，用记忆重建会与下一针收缩形成
                // 「收缩→重建」死循环（02:17:31 诊断行实测同毫秒发生）。
                // 预测展开是 Covered 分支的职责（那边有盖住者压着，多盖
                // 42px 底色在安全侧且色差 <2 不可见）。
                let native = GEOM.lock().ok().and_then(|g| g.native_observed);
                if let Some(n) = native {
                    if !rect_eq(Some(n), Some(endorsed)) {
                        let u = union_rect(endorsed, n);
                        if let Ok(mut g) = GEOM.lock() {
                            g.mask = Some(u);
                        }
                        geom_log(&format!(
                            "mask prepared union=({},{},{},{}) (visible-branch)",
                            u.left, u.top, u.right, u.bottom
                        ));
                        if let Ok(mut last) = LAST_APPLIED_RECT.lock() {
                            *last = Some(u);
                        }
                        mask_now = Some(u);
                    }
                }
            }
            if let Some(m) = mask_now {
                // R8.6 恢复「遮盖在场每针换色」（R7.6 建立、R8.1 撤销）：录屏
                // 逐帧分析证实材质切换是表面真实颜色，回摆撑盖期逐针跟随，
                // 分叉不再有 0.4~3s 滞后窗。
                refresh_clock_overlay_appearance(app_handle, 0, "mask-held-tick");
                // 全尺寸成套应用（含首次展开针），不做 NOSIZE——首次展开若只
                // 移位，158 宽窗口盖不全 3618-3769 残块
                (m.left, m.top, m.right - m.left, m.bottom - m.top, -1)
            } else {
                // R8.6：常规可见态每针轻采（d≥2 才推送，自带节流）——任务栏
                // 材质不止在全屏进出时切换（任意最大化窗口开/关都会切），
                // 15s 保鲜跟不上，平涂滞后即「细微色差保持」。
                // 全尺寸成套应用（弃 NOSIZE，理由见上）。
                refresh_clock_overlay_appearance(app_handle, 0, "visible-tick");
                let (ew, eh) = (endorsed.right - endorsed.left, endorsed.bottom - endorsed.top);
                match read_tray_state() {
                    // 任务栏完全滑出或基准未学习：无盖住者可潜入，只能移出屏幕
                    // （此路径仅自动隐藏任务栏用户触发；全屏场景走 Covered 分支）
                    Some(state) if state.fully_hidden || !state.ready => {
                        (offscreen_x(endorsed.right), endorsed.top, ew, eh, OFFSCREEN_MARK)
                    }
                    Some(state) => {
                        (endorsed.left + state.dx, endorsed.top + state.dy, ew, eh, -1)
                    }
                    None => (offscreen_x(endorsed.right), endorsed.top, ew, eh, OFFSCREEN_MARK),
                }
            }
        }
        // 探测失败：退回矩形全屏判定；全屏则移出屏幕，否则常规显示
        // （R8.1：全尺寸，弃 NOSIZE）
        TrayCover::Unknown => {
            let (ew, eh) = (endorsed.right - endorsed.left, endorsed.bottom - endorsed.top);
            if crate::windows_hook::is_foreground_fullscreen() {
                (offscreen_x(endorsed.right), endorsed.top, ew, eh, OFFSCREEN_MARK)
            } else {
                (endorsed.left, endorsed.top, ew, eh, -1)
            }
        }
    };
    // 「退出潜入恢复常规」瞬间安排两针延迟重探（150ms/600ms）：探测经
    // clock_overlay_note_probe 确认原生归位（请求收缩）或确认布局稳定，
    // UIA 在独立治疗线程执行，绝不进 WinEvent 回调线程。
    let prev_below = CURRENT_BELOW.load(std::sync::atomic::Ordering::SeqCst);
    CURRENT_BELOW.store(below, std::sync::atomic::Ordering::SeqCst);
    if prev_below > 0 && below == -1 && !EXIT_HEALING.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        let heal_app = app_handle.clone();
        std::thread::Builder::new()
            .name("clock-overlay-exit-heal".into())
            .spawn(move || {
                for delay in [150u64, 450] {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    crate::windows_hook::refresh_clock_area_cache();
                    crate::window_manager::relocate_clock_overlay_endorsed(&heal_app);
                }
                EXIT_HEALING.store(false, std::sync::atomic::Ordering::SeqCst);
            })
            .ok();
    }
    // 变化检测：状态与当前完全一致则零窗口操作
    if let Ok(mut last) = LAST_FOLLOW_POS.lock() {
        if *last == Some((target_x, target_y, target_w, target_h, below)) {
            return;
        }
        *last = Some((target_x, target_y, target_w, target_h, below));
    }
    unsafe {
        let insert_after = if below == OFFSCREEN_MARK || below == -1 {
            // 屏外隐藏不需要 topmost 重排（出屏即不可见）；常规显示重申 topmost
            if below == -1 {
                Some(HWND_TOPMOST)
            } else {
                None
            }
        } else {
            // 潜入盖住者正下方（同为 topmost 组内，直接指定插入位置）
            Some(HWND(below as *mut core::ffi::c_void))
        };
        // 遮盖预备/保持需要带尺寸应用；常规跟随保持尺寸（NOSIZE）
        let (cx, cy, flags) = if target_w > 0 && target_h > 0 {
            (target_w, target_h, SWP_NOACTIVATE | SWP_SHOWWINDOW)
        } else {
            (0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW)
        };
        let _ = SetWindowPos(hwnd, insert_after, target_x, target_y, cx, cy, flags);
        // 诊断：每次实际应用的几何变更（含操作后实际矩形，用于核对）
        let mut after = RECT::default();
        let applied = GetWindowRect(hwnd, &mut after).is_ok();
        geom_log(&format!(
            "apply pos=({target_x},{target_y}) size=({cx},{cy}) below={below} after=({},{},{},{}) ok={applied}",
            after.left, after.top, after.right, after.bottom
        ));
    }
}

/// 「移出屏幕」隐藏态的 z 序标记（配合 offscreen_x 位置判断）。
const OFFSCREEN_MARK: isize = -2;

/// 屏外隐藏位的 x：任务栏右缘之外 320px（覆盖层宽 ~210，确保整体出屏；
/// 用屏幕坐标而非显示器矩形，规避多屏负坐标环境下的"屏外"误判）。
fn offscreen_x(tray_right: i32) -> i32 {
    tray_right + 320
}

/// 按时钟矩形贴合覆盖层（尺寸 + 位置）；矩形与上次一致时跳过重设。
/// R7：位置+尺寸改单次 SetWindowPos 原子应用——原 set_size/set_position
/// 两连调用会呈现中间态：收缩时先缩宽、左缘未动，右缘短暂停在旧左缘+新宽
/// （3618+158=3776，冲进托盘图标区），随后才跳到 3818（评审 E）。窗口是
/// 每监视器 DPI 感知的，物理坐标直传与 Tauri Physical 语义一致。
fn apply_overlay_geometry(window: &WebviewWindow, rect: &RECT) -> bool {
    let w = (rect.right - rect.left) as i32;
    let h = (rect.bottom - rect.top) as i32;
    if w <= 0 || h <= 0 {
        return false;
    }
    if let Ok(mut last) = LAST_APPLIED_RECT.lock() {
        if let Some(prev) = *last {
            if prev.left == rect.left
                && prev.top == rect.top
                && prev.right == rect.right
                && prev.bottom == rect.bottom
            {
                return true;
            }
        }
        *last = Some(*rect);
    }
    match get_window_hwnd(window) {
        Some(hwnd) => unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
            .is_ok()
        },
        None => {
            // HWND 不可得时的兜底（正常路径不会走到）
            let _ = window.set_size(tauri::PhysicalSize { width: w as u32, height: h as u32 });
            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                x: rect.left,
                y: rect.top,
            }));
            true
        }
    }
}

/// 遮盖几何应用（R7 扩展/更新用）：单次 SetWindowPos 成套应用位置+尺寸，
/// z 序按当前模式分流（与 update_clock_overlay_visibility 的应用路径同语义）。
/// 同步 GEOM.mask、LAST_APPLIED_RECT 与 LAST_FOLLOW_POS——不同步会让收缩被
/// 「无变化」跳过、下一轮 update 重复应用。
fn apply_mask_geometry(window: &WebviewWindow, m: RECT) -> bool {
    let w = m.right - m.left;
    let h = m.bottom - m.top;
    if w <= 0 || h <= 0 {
        return false;
    }
    let Some(hwnd) = get_window_hwnd(window) else {
        return false;
    };
    if let Ok(mut g) = GEOM.lock() {
        g.mask = Some(m);
    }
    if let Ok(mut last) = LAST_APPLIED_RECT.lock() {
        *last = Some(m);
    }
    let below = CURRENT_BELOW.load(std::sync::atomic::Ordering::SeqCst);
    unsafe {
        let insert_after = if below == OFFSCREEN_MARK {
            None
        } else if below == -1 {
            Some(HWND_TOPMOST)
        } else {
            Some(HWND(below as *mut core::ffi::c_void))
        };
        let _ = SetWindowPos(hwnd, insert_after, m.left, m.top, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let mut after = RECT::default();
        let ok = GetWindowRect(hwnd, &mut after).is_ok();
        geom_log(&format!(
            "apply pos=({},{}) size=({w},{h}) below={below} after=({},{},{},{}) ok={ok}",
            m.left, m.top, after.left, after.top, after.right, after.bottom
        ));
    }
    if let Ok(mut last) = LAST_FOLLOW_POS.lock() {
        *last = Some((m.left, m.top, w, h, below));
    }
    true
}

/// 无激活显示并把覆盖层压到任务栏之上（同为 topmost 组内，后声明者在上）。
fn show_overlay_above_taskbar(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// 读取任务栏主题模式（`SystemUsesLightTheme`=1 为浅色——任务栏跟随**系统**模式，
/// 不读 `AppsUseLightTheme`，两者可分别设置）；读取失败按浅色处理。
fn system_uses_light_theme() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return true;
        }
        let mut value: u32 = 1;
        let mut len = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            w!("SystemUsesLightTheme"),
            None,
            None,
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut len),
        )
        .is_ok();
        let _ = RegCloseKey(key);
        ok && value == 1
    }
}

/// 采样点归属判定：命中任务栏自身（或其子窗口根）才算任务栏。
unsafe fn sample_point_owned(pt: (i32, i32), tray: HWND) -> bool {
    let hit = WindowFromPoint(windows::Win32::Foundation::POINT { x: pt.0, y: pt.1 });
    if hit.0.is_null() {
        return false;
    }
    hit == tray || GetAncestor(hit, GA_ROOT) == tray
}

/// 从任务栏实采底色：采样**「显示桌面」细条**（当前实际应用矩形右缘之外）。
///
/// 该区域没有任何图标/按钮像素，且紧邻时钟——底色与时钟局部最贴近。
/// 旧版采任务栏横向全宽中带，点位大量落在开始按钮/图标上，均值被污染产生色差。
/// 时钟矩形本身被覆盖层盖住（采到的是自己），也必须避开。
///
/// R8 采样锚点稳定化（评审 §3.1）：旧锚点用钩子缓存 CLOCK_AREA_RECT_CACHE，
/// 它会如实跟踪全屏期布局（3618..3769）——退出期采样列 3772..3782 落进遮盖
/// (3618..3818) **内部**，被自己的窗口归属校验拒绝：退出最需要换色的时刻采样
/// 结构性不可用、一直用旧色。新锚点 = 当前实际应用矩形（遮盖在场=遮盖矩形，
/// 常规=认可矩形）右缘，采样点恒在覆盖层窗口右侧之外；endorsed/cache 仅作
/// 首次应用前的回退。
///
/// R8 采样完整性（评审 §3.2）：归属校验扩为读前**逐点**验证全部采样位（旧版
/// 只查每列中线，气泡可只盖上/下行）→ 读后复验三列中线（检查与读取之间
/// 盖住者再现的 TOCTOU 收窄）→ 块内离散度守卫（纯色任务栏区散度近 0，
/// 半覆盖的气泡/残影会拉高散度——宁可不采保留旧色，不推送污染值）。
/// 失败原因经 geom_log 记入诊断。三道闸都不代表像素真值的充分证明，
/// 但把已知竞态（#00FFFD/#67EDE8/#C8F2EF 三采样污染实测）全部挡住。
/// R8.4 采样结果：主色 + 两个参考区读数 + 覆盖层实际显示色。
struct TaskbarSample {
    /// 推送用主色：左侧净列（纯任务栏表面）优先，条带回退
    primary: (u8, u8, u8),
    /// 条带（「显示桌面」细条）读数——诊断对照；R8.4 起仅作回退
    strip: Option<(u8, u8, u8)>,
    /// 左侧净列读数——诊断
    left: Option<(u8, u8, u8)>,
    /// 覆盖层实际显示色（闭环校准用）
    inside: Option<(u8, u8, u8)>,
}

/// R8.2 闭环校准：内部色 = 覆盖层右缘内 2px 的屏幕像素（三点中位）——即
/// **我们推送色的实际显示效果**。开环链路（外部采样 vs 推送值）自洽时日志
/// 全是 unchanged，但用户仍见持续色差（复验⑦）——偏差位于「推送色→屏幕
/// 呈现」之间，开环结构性观测不到。内外部之差就是用户肉眼所见色差，由
/// run_appearance_sample 反馈到推送值。内部点归属必须命中覆盖层自身
/// （滑动跟随中/被盖时拒绝→None→退化纯开环）。
///
/// R8.4 主参考迁移（修「退出后先无色差→很快有色差→保持」）：主参考从
/// 「显示桌面」细条改覆盖层**左侧净列**（托盘图标间隙的纯任务栏表面）。
/// 复验⑨实测：退出全屏后条带可稳定偏离主色 9~14 RGB 数秒（#F0E4DB vs
/// #E7D8CD，条带是特殊交互元素，有独立的高亮/材质态），而我们把它平涂
/// 给整个覆盖层 → 「本来无色差（trusted 色当时正确）→ 采样采纳条带分叉色
/// → 与周围任务栏色差并保持」。左侧净列与覆盖层下方是同一块连续表面，
/// 无条带的特殊状态；图标污染列用逐列离散度过滤（纯色列散度 ≤6 才采）。
/// 条带降级为回退（左侧无净列时）+ 诊断对照（日志双读数，分叉可直读）。
fn sample_taskbar_pixel() -> Option<TaskbarSample> {
    unsafe {
        // 采样锚点：当前实际应用矩形（遮盖在场=遮盖矩形，常规=认可矩形）
        let applied = LAST_APPLIED_RECT
            .lock()
            .ok()
            .and_then(|g| *g)
            .or_else(|| GEOM.lock().ok().and_then(|g| g.endorsed))
            .or_else(|| {
                crate::windows_hook::CLOCK_AREA_RECT_CACHE
                    .read()
                    .ok()
                    .and_then(|guard| guard.as_ref().copied())
            })?;
        let anchor_right = applied.right;
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let mut tray_rect = RECT::default();
        GetWindowRect(tray, &mut tray_rect).ok()?;
        // 时钟所在显示器右缘（「显示桌面」条右边界）
        let hmon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return None;
        }
        let screen_right = mi.rcMonitor.right;
        // 条内取 3 个纵向位置（上/中/下错开抗单点噪点）
        let y_mid = (tray_rect.top + tray_rect.bottom) / 2;
        let y_top = tray_rect.top + (tray_rect.bottom - tray_rect.top) / 4;
        let y_bot = tray_rect.bottom - (tray_rect.bottom - tray_rect.top) / 4;

        // ---- 条带参考（回退 + 诊断）：3 列 × 3 行，读前逐点归属校验 ----
        let mut strip_pts: Vec<(i32, i32)> = Vec::with_capacity(9);
        for dx in [3i32, 8, 13] {
            let sx = anchor_right + dx;
            if sx >= screen_right - 1 {
                break;
            }
            for y in [y_top, y_mid, y_bot] {
                strip_pts.push((sx, y));
            }
        }
        let strip_pre_ok =
            strip_pts.len() >= 3 && strip_pts.iter().all(|p| sample_point_owned(*p, tray));
        if !strip_pre_ok {
            geom_log("appearance sample: strip pre-check owned=false");
        }

        let dc = GetDC(None);
        let strip = if strip_pre_ok {
            let mut colors: Vec<(u8, u8, u8)> = Vec::with_capacity(strip_pts.len());
            for p in &strip_pts {
                let color = GetPixel(dc, p.0, p.1).0;
                if color != 0xFFFF_FFFF {
                    colors.push((
                        (color & 0xFF) as u8,
                        ((color >> 8) & 0xFF) as u8,
                        ((color >> 16) & 0xFF) as u8,
                    ));
                }
            }
            // 闸②：读后复验三列中线（收窄检查与读取之间的竞态窗）
            let post_ok = [3i32, 8, 13]
                .iter()
                .all(|dx| sample_point_owned((anchor_right + dx, y_mid), tray));
            // 闸③：块内离散度守卫——半覆盖的气泡/残影会拉高散度
            let spread = colors
                .iter()
                .fold((255u8, 255u8, 255u8, 0u8, 0u8, 0u8), |a, c| {
                    (
                        a.0.min(c.0),
                        a.1.min(c.1),
                        a.2.min(c.2),
                        a.3.max(c.0),
                        a.4.max(c.1),
                        a.5.max(c.2),
                    )
                });
            let spread_v = (spread.3 - spread.0)
                .max(spread.4 - spread.1)
                .max(spread.5 - spread.2) as i32;
            if !post_ok {
                geom_log("appearance sample: strip post-check owned=false, discard");
                None
            } else if colors.len() < 3 {
                geom_log("appearance sample: strip GetPixel all failed");
                None
            } else if spread_v > 48 {
                geom_log(&format!("appearance sample: strip dispersion {spread_v} > 48, discard"));
                None
            } else {
                let n = colors.len() as u32;
                let sum = colors.iter().fold((0u32, 0u32, 0u32), |a, c| {
                    (a.0 + c.0 as u32, a.1 + c.1 as u32, a.2 + c.2 as u32)
                });
                Some((
                    (sum.0 / n) as u8,
                    (sum.1 / n) as u8,
                    (sum.2 / n) as u8,
                ))
            }
        } else {
            None
        };

        // ---- 左侧净列（R8.4 主参考；R8.8 屏幕坐标固定列）：右缘向左
        // 175~200px 共 6 个**固定屏幕列**（遮盖 3607..3819 与常规 3649..3819
        // 两态下位置不变，使渐变映射屏幕锚定——窗口移动/收缩只裁切不重映射；
        // 候选窗曾取 190/195/200px 处全被图标污染（03:24 实测 left=none），
        // 修正为 175~200px 覆盖实测净列 3639/3644 一带；CSS 映射按 -190px
        // 名义位置，滑差 ≤10px ≈0.1 RGB 可忽略）。归属（托盘）+ 三点离散度
        // ≤6 过滤污染；遮盖期这些列位于覆盖层窗口内部→归属拒绝→退化为
        // 平涂（安全侧）。
        let mut left: Option<(u8, u8, u8)> = None;
        for k in 0..6i32 {
            let sx = anchor_right - 175 - k * 5;
            if sx <= tray_rect.left + 2 {
                break;
            }
            let cols = [(sx, y_top), (sx, y_mid), (sx, y_bot)];
            if !cols.iter().all(|p| sample_point_owned(*p, tray)) {
                continue;
            }
            let mut rows: Vec<(u8, u8, u8)> = Vec::with_capacity(3);
            for p in cols {
                let color = GetPixel(dc, p.0, p.1).0;
                if color != 0xFFFF_FFFF {
                    rows.push((
                        (color & 0xFF) as u8,
                        ((color >> 8) & 0xFF) as u8,
                        ((color >> 16) & 0xFF) as u8,
                    ));
                }
            }
            if rows.len() < 3 {
                continue;
            }
            let sp = rows.iter().fold((255u8, 255u8, 255u8, 0u8, 0u8, 0u8), |a, c| {
                (
                    a.0.min(c.0),
                    a.1.min(c.1),
                    a.2.min(c.2),
                    a.3.max(c.0),
                    a.4.max(c.1),
                    a.5.max(c.2),
                )
            });
            let spread_v = (sp.3 - sp.0).max(sp.4 - sp.1).max(sp.5 - sp.2) as i32;
            if spread_v > 6 {
                continue; // 图标/悬停高亮污染列，换更左侧
            }
            left = Some((
                ((rows[0].0 as u32 + rows[1].0 as u32 + rows[2].0 as u32) / 3) as u8,
                ((rows[0].1 as u32 + rows[1].1 as u32 + rows[2].1 as u32) / 3) as u8,
                ((rows[0].2 as u32 + rows[1].2 as u32 + rows[2].2 as u32) / 3) as u8,
            ));
            break;
        }

        // ---- 内部点（闭环）：覆盖层右缘内 2px（padding 纯背景区），三点中位 ----
        let mut inside: Option<(u8, u8, u8)> = None;
        let own_hwnd = OVERLAY_HWND.lock().ok().and_then(|h| *h);
        let inside_owned = |p: (i32, i32)| -> bool {
            let hit = WindowFromPoint(windows::Win32::Foundation::POINT { x: p.0, y: p.1 });
            !hit.0.is_null()
                && own_hwnd
                    .map(|h| hit.0 as isize == h || GetAncestor(hit, GA_ROOT).0 as isize == h)
                    .unwrap_or(false)
        };
        let inside_pts = [
            (anchor_right - 2, y_top),
            (anchor_right - 2, y_mid),
            (anchor_right - 2, y_bot),
        ];
        if inside_pts.iter().all(|p| inside_owned(*p)) {
            let mut rows: Vec<(u8, u8, u8)> = Vec::with_capacity(3);
            for p in inside_pts {
                let color = GetPixel(dc, p.0, p.1).0;
                if color != 0xFFFF_FFFF {
                    rows.push((
                        (color & 0xFF) as u8,
                        ((color >> 8) & 0xFF) as u8,
                        ((color >> 16) & 0xFF) as u8,
                    ));
                }
            }
            if rows.len() == 3 {
                // 逐通道中位（三点排序取中，滤单点噪声）
                let mut r = [rows[0].0, rows[1].0, rows[2].0];
                let mut g = [rows[0].1, rows[1].1, rows[2].1];
                let mut b = [rows[0].2, rows[1].2, rows[2].2];
                r.sort_unstable();
                g.sort_unstable();
                b.sort_unstable();
                inside = Some((r[1], g[1], b[1]));
            }
        } else if own_hwnd.is_some() {
            geom_log("appearance sample: inside points not owned, closed-loop off this round");
        }
        ReleaseDC(None, dc);

        let Some(primary) = left.or(strip) else {
            geom_log("appearance sample: no clean surface (left & strip unavailable), skip");
            return None;
        };
        Some(TaskbarSample { primary, strip, left, inside })
    }
}

/// 覆盖层外观：任务栏底色（实采优先，主题注册表兜底）+ 按亮度选择的对比前景色。
/// 返回 `(右端背景 hex, 前景 hex, 左端背景 hex)`。绝不返回透明——透底叠字是
/// 覆盖式方案的头号风险。R8.5：左端=左侧净列（无净列时=右端，等价平涂）。
pub fn clock_overlay_appearance_colors() -> (String, String, String) {
    let (bg, bg_left, source) = match sample_taskbar_pixel() {
        // 命令路径（前端初始加载）取双端参考色；闭环校准仅在 worker 推送路径
        Some(s) => {
            let right = s.strip.unwrap_or(s.primary);
            let left = s.left.unwrap_or(right);
            // R8.2：同步推送基准——前端将显示此色，闭环以 LAST_PUSHED_BG 为
            // 「当前显示色」参照，不同步会把命令设置的色误算成偏差
            if let Ok(mut last) = LAST_PUSHED_BG.lock() {
                *last = Some((left, right));
            }
            (right, left, "sampled")
        }
        None => {
            let light = system_uses_light_theme();
            crate::dbg_log(&format!(
                "clock overlay appearance: pixel sampling failed, fallback theme light={light}"
            ));
            let c = if light { (0xF3, 0xF3, 0xF3) } else { (0x20, 0x20, 0x20) };
            (c, c, "theme")
        }
    };
    let luminance = 0.2126 * bg.0 as f64 + 0.7152 * bg.1 as f64 + 0.0722 * bg.2 as f64;
    let fg = if luminance > 128.0 { "#1a1a1a" } else { "#ffffff" };
    crate::dbg_log(&format!(
        "clock overlay appearance bg=#{:02X}{:02X}{:02X} bgLeft=#{:02X}{:02X}{:02X} ({source}) fg={fg}",
        bg.0, bg.1, bg.2, bg_left.0, bg_left.1, bg_left.2
    ));
    (
        format!("#{:02X}{:02X}{:02X}", bg.0, bg.1, bg.2),
        fg.to_string(),
        format!("#{:02X}{:02X}{:02X}", bg_left.0, bg_left.1, bg_left.2),
    )
}

/// 最近一次推送给前端的底色（RGB）——变化检测，未变不重复推送。
/// 最近一次推送给前端的底色 `(左端, 右端)`（R8.5 渐变双端点）——变化检测，
/// 任一端变化 ≥2 才推送。左端缺净列时与右端同值（等价平涂）。
static LAST_PUSHED_BG: Mutex<Option<((u8, u8, u8), (u8, u8, u8))>> = Mutex::new(None);

/// 外观推送序号（R8，评审 §3.4）：每次推送前自增，事件 payload 与命令响应
/// 都携带——前端按 seq 单调守卫应用，晚到的旧值（初始 invoke 响应 vs 事件
/// 竞态）不再覆盖新色。
static APPEARANCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 当前推送序号（供命令响应携带，前端单调守卫用）。
pub fn clock_overlay_appearance_seq() -> u64 {
    APPEARANCE_SEQ.load(std::sync::atomic::Ordering::SeqCst)
}

/// 待采样请求槽（R8 单工作线程，评审 §3.3）：请求只往里塞 `(到期时刻, 来源)`，
/// 永久 worker 串行消费——多请求合并、采样与推送天然有序。旧实现每次调用
/// spawn 一个线程，旧线程可在新线程之后 emit 旧色而前端无版本照单全收；
/// 延迟请求（400ms 兜底）也不再被后到的即时请求吞掉（多入口并存，各自到期）。
static APPEARANCE_DUE: Mutex<Vec<(std::time::Instant, &'static str)>> = Mutex::new(Vec::new());

/// worker / 可信色保鲜线程的懒启动标记。
static APPEARANCE_WORKERS_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 底色动态跟随请求入口（R7.5 建立，R8 改为合并调度）。`delay_ms` 为 0 表示
/// 立即，`src` 仅供诊断日志区分调用方。
///
/// 时机全景（R8.6 收敛后）：
/// - **attach 即刻**：前端过渡色（主题近似值）换真色，并点火 worker/保鲜线程；
/// - **逐针跟随（visible-tick / mask-held-tick / z-reclaim / visible-switch）**：
///   任务栏可见的每一探针针都轻采一次——材质切换（全屏进出、最大化窗口开/
///   关）是表面真实颜色 ±9~24，跟随滞后 0.4~3s 即肉眼色差爆发（复验⑨录屏
///   逐帧实证）；d≥2 死区自带节流，稳态零推送；
/// - **收缩后延迟三连采（400 / 1500 / 4000ms）**：收缩路径的兜底针；
/// - **正常态保鲜（15s / 退出观测窗内 3s）**：逐针跟随的兜底网。
///
/// 历史（R8.6 修正认知）：R8.1/R8.3 曾以「过渡毒色翻转」为由撤销逐针采样——
/// 复验⑨录屏逐帧分析证明那些颜色是任务栏表面的真实材质状态（不透后面窗口、
/// 只透壁纸，受控实验排除垫底污染），翻转即表面本身在变；撤销跟随制造了
/// 0.4~3s 的分叉滞后窗=复验⑦~⑩持续可见的「细微色差」。基础设施就位
/// （左列主参考/闭环/渐变/死区）后恢复跟随。
///
/// R8.1 重要教训（撤销 R8/R7.6 的退出窗口期活跃采样 z-reclaim /
/// visible-switch / mask-held-tick）：退出过渡期任务栏亚克力的透出内容正从
/// 视频切回壁纸，**真实任务栏本色本身在变**，活跃采样采到的是过渡色且与
/// 稳态色来回翻转推送（00:56 真机实测 seq=2~11 连续翻转）——这本身就是
/// 用户可见的残余色差。退出期信任保鲜维持的稳态色（=进全屏前的本色，通常
/// 与收敛后的稳态一致），只在世界稳定后采样采纳。
pub fn refresh_clock_overlay_appearance(app_handle: &AppHandle, delay_ms: u64, src: &'static str) {
    let due = std::time::Instant::now() + std::time::Duration::from_millis(delay_ms);
    let spawn = {
        let Ok(mut queue) = APPEARANCE_DUE.lock() else {
            return;
        };
        let first = !APPEARANCE_WORKERS_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst);
        // 队列极小（常态 0~1 条，退出窗峰值 ~4 条），满了丢新请求保旧——
        // 丢的只是多的一次重试，下个时机层会再来
        if queue.len() < 8 {
            queue.push((due, src));
        }
        first
    };
    if spawn {
        spawn_appearance_workers(app_handle.clone());
    }
}

fn spawn_appearance_workers(app: AppHandle) {
    // 采样/推送 worker：串行消费 APPEARANCE_DUE（最早到期者先采）。
    // 30ms 粒度轮询换来零锁争用与实现简单；采样本身是微秒级 hit-test+GetPixel。
    let worker_app = app.clone();
    let _ = std::thread::Builder::new()
        .name("clock-appearance-worker".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let claimed = {
                let Ok(mut queue) = APPEARANCE_DUE.lock() else {
                    continue;
                };
                let now = std::time::Instant::now();
                let idx = queue
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.0 <= now)
                    .min_by_key(|(_, t)| t.0)
                    .map(|(i, _)| i);
                idx.map(|i| queue.remove(i))
            };
            if let Some((_, src)) = claimed {
                run_appearance_sample(&worker_app, src);
            }
        })
        .ok();
    // 正常态可信色保鲜（R8）：稳态可见期每 15s 轻采一次。全屏期/回摆期跳过
    // （那边由时机层覆盖，且盖住者会顶掉归属校验）；这里的收益在正常态——
    // 旧实现只在退出轮采样，正常态底色缓变时覆盖层色渐旧无人纠正。
    // R8.3：退出观测窗内（收缩后 6s）提速到 3s——连续测试中最后一轮若仍
    // 留有过渡色，最迟 3s 纠正（此前实测最坏 6.5s+ 才被 15s 兜底拉回）。
    let _ = std::thread::Builder::new()
        .name("clock-appearance-trusted".into())
        .spawn(move || loop {
            let in_watch = GEOM
                .lock()
                .ok()
                .and_then(|g| g.exit_watch_until)
                .map(|t| t > std::time::Instant::now())
                .unwrap_or(false);
            std::thread::sleep(std::time::Duration::from_secs(if in_watch { 3 } else { 15 }));
            if !ATTACHED.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            if CURRENT_BELOW.load(std::sync::atomic::Ordering::SeqCst) != -1 {
                continue;
            }
            let steady = GEOM
                .lock()
                .ok()
                .map(|g| g.phase == GeomPhase::Normal && g.mask.is_none())
                .unwrap_or(false);
            if !steady {
                continue;
            }
            refresh_clock_overlay_appearance(&app, 0, "trusted-15s");
        })
        .ok();
}

/// R8.8 外观更新候选（纯函数）：候选=本轮双端采样，变化量=与上次推送的
/// 最大通道差。抽出纯函数并单测锁定语义——防止再次出现「采样值不参与
/// 计算」（R8.5 回归：cand_left=prev_left+corr 使左端点/渐变斜率冻结）或
/// 「inside 缺失误判收敛」（R8.5 回归：corr=0→d=0）一类缺陷。
/// `inside` 不进入本函数：渲染偏差实测为 0，它只作为独立诊断量记录
/// （见 run_appearance_sample 的 insideErr）。
fn appearance_update(
    prev: ((u8, u8, u8), (u8, u8, u8)),
    left: (u8, u8, u8),
    right: (u8, u8, u8),
) -> (((u8, u8, u8), (u8, u8, u8)), i32) {
    let cand = (left, right);
    let ch_d = |a: (u8, u8, u8), b: (u8, u8, u8)| {
        (a.0 as i32 - b.0 as i32)
            .abs()
            .max((a.1 as i32 - b.1 as i32).abs())
            .max((a.2 as i32 - b.2 as i32).abs())
    };
    let d = ch_d(cand.0, prev.0).max(ch_d(cand.1, prev.1));
    (cand, d)
}

/// 执行一次采样与推送（worker 串行调用，无并发）。失败保持最近可信色，
/// 原因已在 sample_taskbar_pixel 内记诊断日志。
///
/// R8.5 渐变双端点：覆盖层下方的任务栏表面是**横向渐变**（壁纸透出，实测
/// 左端 E9D9CC → 右端 E7D8CD，亮态下更陡），平涂单色必然与某一侧边缘差
/// 1~4 RGB——复验⑩「还是有」的细微持续色差。改为推送双端点 (left,right)，
/// 前端 `linear-gradient` 按**屏幕坐标锚定**复现渐变（窗口移动/收缩只裁切
/// 不重映射）。
/// R8.8 双端直接更新（复审 §2/§3/§5 确定性缺陷修复）：候选=本轮双端采样
/// （`appearance_update`），渲染偏差实测为 0 故不做积分式校正。旧闭环公式
/// `cand=prev+(采样-内部)` 的三重缺陷：左端采样不参与（渐变斜率冻结，复审
/// §2）、内部点缺失时冻结更新并误报收敛（复审 §3，R8.5 回归）、上一次推送
/// 未呈现时误差重复累计（复审 §5）。内部读数保留为**诊断量**：insideErr
/// 非零即渲染/呈现偏差告警，不再进入控制回路。
fn run_appearance_sample(app: &AppHandle, src: &'static str) {
    let Some(s) = sample_taskbar_pixel() else {
        geom_log(&format!("appearance run src={src}: no sample (kept trusted color)"));
        return;
    };
    // 双端点：右端=条带（覆盖层右邻），左端=左侧净列（覆盖层左邻）；
    // 任一缺失时取另一端（等价平涂）
    let right = s.strip.unwrap_or(s.primary);
    let left = s.left.unwrap_or(right);
    let src_note = match (s.left, s.strip) {
        (Some(l), Some(st)) => {
            let d_ls = (l.0 as i32 - st.0 as i32)
                .abs()
                .max((l.1 as i32 - st.1 as i32).abs().max((l.2 as i32 - st.2 as i32).abs()));
            format!("left=#{:02X}{:02X}{:02X} strip=#{:02X}{:02X}{:02X} (d={d_ls})", l.0, l.1, l.2, st.0, st.1, st.2)
        }
        (Some(l), None) => format!("left=#{:02X}{:02X}{:02X} strip=none", l.0, l.1, l.2),
        (None, Some(st)) => format!("left=none strip=#{:02X}{:02X}{:02X}", st.0, st.1, st.2),
        (None, None) => "left=none strip=none".into(),
    };
    let Ok(mut last) = LAST_PUSHED_BG.lock() else {
        return;
    };
    let Some((prev_left, prev_right)) = *last else {
        // 首次推送（无参照显示色）：直接推双端采样值
        *last = Some((left, right));
        drop(last);
        push_appearance(app, src, left, right, &src_note);
        return;
    };
    // R8.8 双端直接更新：候选=本轮采样（见上方函数文档；appearance_update
    // 纯函数可单测）。内部读数降级为诊断量。
    let ((cand_left, cand_right), d) = appearance_update((prev_left, prev_right), left, right);
    let inside_note = match s.inside {
        Some(i) => format!(
            "inside=#{:02X}{:02X}{:02X} (err={:+},{:+},{:+})",
            i.0,
            i.1,
            i.2,
            i.0 as i32 - cand_right.0 as i32,
            i.1 as i32 - cand_right.1 as i32,
            i.2 as i32 - cand_right.2 as i32
        ),
        None => "inside=none (open-loop)".into(),
    };
    if d < 2 {
        geom_log(&format!(
            "appearance run src={src}: converged right=#{:02X}{:02X}{:02X} {src_note} {inside_note} pushed=({:02X}{:02X}{:02X}|{:02X}{:02X}{:02X}) d={d}",
            right.0,
            right.1,
            right.2,
            prev_left.0, prev_left.1, prev_left.2,
            prev_right.0, prev_right.1, prev_right.2
        ));
        return;
    }
    *last = Some((cand_left, cand_right));
    drop(last);
    push_appearance(app, src, cand_left, cand_right, &src_note);
}

/// 推送外观到前端（seq 自增 + 事件 emit）。R8.5：bg_left 为渐变左端，
/// bg 为渐变右端。
fn push_appearance(
    app: &AppHandle,
    src: &'static str,
    bg_left: (u8, u8, u8),
    bg: (u8, u8, u8),
    src_note: &str,
) {
    let luminance = 0.2126 * bg.0 as f64 + 0.7152 * bg.1 as f64 + 0.0722 * bg.2 as f64;
    let fg = if luminance > 128.0 { "#1a1a1a" } else { "#ffffff" };
    let seq = APPEARANCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    crate::dbg_log(&format!(
        "clockrect: appearance push seq={seq} src={src} bg=#{:02X}{:02X}{:02X} bgLeft=#{:02X}{:02X}{:02X} {src_note} (gradient)",
        bg.0, bg.1, bg.2, bg_left.0, bg_left.1, bg_left.2
    ));
    let _ = app.emit(
        "clock-appearance",
        serde_json::json!({
            "bg": format!("#{:02X}{:02X}{:02X}", bg.0, bg.1, bg.2),
            "bgLeft": format!("#{:02X}{:02X}{:02X}", bg_left.0, bg_left.1, bg_left.2),
            "fg": fg,
            "seq": seq,
        }),
    );
}

#[cfg(test)]
mod tests {
    // R8.8：合成输入单测，锁定外观更新语义（复审 §10 实验 A 的可判定部分）。
    // 回归背景：R8.5 版实现 cand_left=prev_left+corr（左端采样不参与，渐变
    // 斜率冻结）、inside=None 时 corr=0 导致更新冻结并误报收敛。
    use super::appearance_update;
    type Rgb = (u8, u8, u8);

    #[test]
    fn 左端变化必须进入候选_即使右端与内部读数都不变() {
        // 复审 §2 反例：旧推送 (233,231)，新采样左 243/右 231——旧公式 corr=0
        // 会输出 d=0 丢弃；直接更新必须采纳左端新值。
        let prev = ((233u8, 220u8, 210u8), (231u8, 216u8, 205u8));
        let (cand, d) = appearance_update(prev, (243, 230, 210), (231, 216, 205));
        assert_eq!(cand.0, (243, 230, 210));
        assert_eq!(cand.1, (231, 216, 205));
        assert!(d >= 2);
    }

    #[test]
    fn 候选恒等于本轮采样_与历史值和内部读数无关() {
        // 直接更新语义：候选只由本轮采样决定（渲染偏差实测为 0，不做积分校正）
        let prev = ((200u8, 200u8, 200u8), (200u8, 200u8, 200u8));
        let (cand, _) = appearance_update(prev, (240, 228, 219), (241, 228, 218));
        assert_eq!(cand, ((240, 228, 219), (241, 228, 218)));
    }

    #[test]
    fn 采样与推送一致时变化量低于推送死区() {
        let c = ((231u8, 216u8, 205u8), (233u8, 217u8, 204u8));
        let (_, d) = appearance_update(c, c.0, c.1);
        assert_eq!(d, 0);
    }

    #[test]
    fn 渐变斜率变化_两端独立跟随() {
        // 斜率变大：左端更亮、右端不变——两端必须各自跟随
        let prev = ((233u8, 217u8, 204u8), (231u8, 216u8, 205u8));
        let (cand, d) = appearance_update(prev, (241, 225, 212), (231, 216, 205));
        assert_eq!(cand.0, (241, 225, 212));
        assert_eq!(cand.1, (231, 216, 205));
        assert_eq!(d, 8);
    }
}
