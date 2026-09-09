//! 任务栏时钟覆盖层（Phase 0 覆盖式接管）：不透明置顶窗口盖在系统时钟上，
//! 由前端 `ClockOverlayWindow` 渲染「时间 + 农历 + 天气」。
//!
//! 本阶段只接管「显示与悬停」：窗口不再鼠标穿透（`WS_EX_NOACTIVATE`，点击不抢
//! 前台焦点），悬停落在覆盖层上、系统时钟 XAML tooltip 从此不再触发；时钟区的
//! 点击仍由低级钩子整体吞掉（Phase 2 才交接输入），交互行为零变化。
//! 几何贴合复用 UIA 时钟矩形缓存；探测失败时保持隐藏——原生时钟兜底。
use std::sync::Mutex;
use tauri::{AppHandle, Manager, WebviewWindow};
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
                    crate::dbg_log(&format!(
                        "clock overlay: attached at attempt {attempt} rect=({},{})-({},{})",
                        rect.left, rect.top, rect.right, rect.bottom
                    ));
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
pub fn relocate_clock_overlay_endorsed(app_handle: &AppHandle) {
    let (endorsed, phase, mask, shrink_requested, native) = match GEOM.lock() {
        Ok(g) => (g.endorsed, g.phase, g.mask, g.shrink_requested, g.native_observed),
        Err(_) => return,
    };
    if phase == GeomPhase::Covered {
        if mask.is_none() {
            if let (Some(e), Some(n)) = (endorsed, native) {
                if !rect_eq(Some(n), Some(e)) {
                    // 原生偏离证据已到手而遮盖未展开：立即展开（内含探测、
                    // 遮盖构建与成套应用；若无盖住者则走 Visible 分支路径）
                    update_clock_overlay_visibility(app_handle);
                }
            }
        }
        return;
    }
    if !shrink_requested {
        if let (Some(endorsed), Some(native)) = (endorsed, native) {
            if !rect_eq(Some(native), Some(endorsed)) {
                let u = union_rect(endorsed, native);
                if !rect_eq(mask, Some(u)) {
                    let Some(window) = app_handle.get_webview_window("clock_overlay") else {
                        return;
                    };
                    if apply_mask_geometry(&window, u) {
                        crate::dbg_log(&format!(
                            "clockrect: mask expand (native diverged) union=({},{},{},{})",
                            u.left, u.top, u.right, u.bottom
                        ));
                    }
                    return;
                }
                if mask.is_some() {
                    // 并集未变（旧遮盖已盖住新观测）：维持现状
                    return;
                }
            }
        }
    }
    if mask.is_some() && !shrink_requested {
        return; // 遮盖保持期：原生仍在全屏位形，收缩会露出残块
    }
    let Some(endorsed) = endorsed else { return };
    let Some(window) = app_handle.get_webview_window("clock_overlay") else { return };
    if mask.is_some() {
        crate::dbg_log("clockrect: shrink (native endorsed)");
    }
    if !apply_overlay_geometry(&window, &endorsed) {
        return;
    }
    if let Ok(mut g) = GEOM.lock() {
        g.mask = None;
        g.native_observed = None;
        g.shrink_requested = false;
        // 真实收缩（遮盖→无）才开启退出观测窗：常规无变化重贴不开窗
        if mask.is_some() {
            g.exit_watch_until = Some(
                std::time::Instant::now() + std::time::Duration::from_millis(EXIT_WATCH_MS),
            );
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
    update_clock_overlay_visibility(app_handle);
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
    // 状态五元组：(x, y, 宽, 高, 插入到谁之后)。宽高为 0 = 保持尺寸（NOSIZE），
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
            // 否则本轮新观测的原生位置与认可位取并集。保持遮盖必须位置+尺寸
            // 成套应用——只回左缘不缩宽会让窗口变成"认可左缘+遮盖宽度"，
            // 右缘冲进「显示桌面」区（盖住相邻图标，用户实测）。
            let cur_mask = GEOM.lock().ok().and_then(|g| g.mask);
            let native = GEOM.lock().ok().and_then(|g| g.native_observed);
            let mask_target = cur_mask.or_else(|| {
                native.and_then(|n| {
                    let u = union_rect(endorsed, n);
                    (!rect_eq(Some(u), Some(endorsed))).then_some(u)
                })
            });
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
                (endorsed.left, endorsed.top, 0, 0, cover.0 as isize)
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
                }
            }
            // 遮盖保持期（R6 实时跟踪）：退出轮回摆期探测可能直接 Visible 而原生
            // 仍在全屏位形（探测只看任务栏左段，代表不了时钟区）——此时候盖
            // 未展开也必须立即展开，否则原生残块压在托盘区露出（23:39 实测
            // 40.575/41.722 两个暴露窗）。收缩不在此处：由 relocate 在
            // note_probe 置位收缩请求后执行。
            let mut mask_now = GEOM.lock().ok().and_then(|g| g.mask);
            if mask_now.is_none() {
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
                // 全尺寸成套应用（含首次展开针），不做 NOSIZE——首次展开若只
                // 移位，158 宽窗口盖不全 3618-3769 残块
                (m.left, m.top, m.right - m.left, m.bottom - m.top, -1)
            } else {
                match read_tray_state() {
                    // 任务栏完全滑出或基准未学习：无盖住者可潜入，只能移出屏幕
                    // （此路径仅自动隐藏任务栏用户触发；全屏场景走 Covered 分支）
                    Some(state) if state.fully_hidden || !state.ready => {
                        (offscreen_x(endorsed.right), endorsed.top, 0, 0, OFFSCREEN_MARK)
                    }
                    Some(state) => {
                        (endorsed.left + state.dx, endorsed.top + state.dy, 0, 0, -1)
                    }
                    None => (offscreen_x(endorsed.right), endorsed.top, 0, 0, OFFSCREEN_MARK),
                }
            }
        }
        // 探测失败：退回矩形全屏判定；全屏则移出屏幕，否则常规显示
        TrayCover::Unknown => {
            if crate::windows_hook::is_foreground_fullscreen() {
                (offscreen_x(endorsed.right), endorsed.top, 0, 0, OFFSCREEN_MARK)
            } else {
                (endorsed.left, endorsed.top, 0, 0, -1)
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

/// 从任务栏实采底色：采样**时钟矩形右侧、屏幕右缘之前的细条**（「显示桌面」区）。
///
/// 该区域没有任何图标/按钮像素，且紧邻时钟——底色与时钟局部最贴近。
/// 旧版采任务栏横向全宽中带，点位大量落在开始按钮/图标上，均值被污染产生色差。
/// 时钟矩形本身被覆盖层盖住（采到的是自己），也必须避开。
fn sample_taskbar_pixel() -> Option<(u8, u8, u8)> {
    unsafe {
        // 覆盖层贴合的时钟矩形：右缘之右即「显示桌面」细条
        let clock = crate::windows_hook::CLOCK_AREA_RECT_CACHE
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().copied())?;
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
        // 条内取 3 个纵向位置、每列 3 点（上/中/下错开，抗单点噪点）
        let y_mid = (tray_rect.top + tray_rect.bottom) / 2;
        let y_top = tray_rect.top + (tray_rect.bottom - tray_rect.top) / 4;
        let y_bot = tray_rect.bottom - (tray_rect.bottom - tray_rect.top) / 4;
        let dc = GetDC(None);
        let mut sum = (0u32, 0u32, 0u32);
        let mut count = 0u32;
        for dx in [3i32, 8, 13] {
            let x = clock.right + dx;
            if x >= screen_right - 1 {
                break;
            }
            for y in [y_top, y_mid, y_bot] {
                let color = GetPixel(dc, x, y).0;
                if color == 0xFFFF_FFFF {
                    continue;
                }
                sum.0 += (color & 0xFF) as u32;
                sum.1 += ((color >> 8) & 0xFF) as u32;
                sum.2 += ((color >> 16) & 0xFF) as u32;
                count += 1;
            }
        }
        ReleaseDC(None, dc);
        if count < 3 {
            return None;
        }
        Some((
            (sum.0 / count) as u8,
            (sum.1 / count) as u8,
            (sum.2 / count) as u8,
        ))
    }
}

/// 覆盖层外观：任务栏底色（实采优先，主题注册表兜底）+ 按亮度选择的对比前景色。
/// 返回 `(背景 hex, 前景 hex)`。绝不返回透明——透底叠字是覆盖式方案的头号风险。
pub fn clock_overlay_appearance_colors() -> (String, String) {
    let (bg, source) = match sample_taskbar_pixel() {
        Some(sampled) => (sampled, "sampled"),
        None => {
            let light = system_uses_light_theme();
            crate::dbg_log(&format!(
                "clock overlay appearance: pixel sampling failed, fallback theme light={light}"
            ));
            (
                if light { (0xF3, 0xF3, 0xF3) } else { (0x20, 0x20, 0x20) },
                "theme",
            )
        }
    };
    let luminance = 0.2126 * bg.0 as f64 + 0.7152 * bg.1 as f64 + 0.0722 * bg.2 as f64;
    let fg = if luminance > 128.0 { "#1a1a1a" } else { "#ffffff" };
    crate::dbg_log(&format!(
        "clock overlay appearance bg=#{:02X}{:02X}{:02X} ({source}) fg={fg}",
        bg.0, bg.1, bg.2
    ));
    (format!("#{:02X}{:02X}{:02X}", bg.0, bg.1, bg.2), fg.to_string())
}
