# 方案：任务栏时钟覆盖式接管（完整版）

> 本文档是「覆盖式接管」的**总方案与实施全集**：架构决策、已实施内容、当前几何状态机、排障史索引。
> 当前 bug 排障状态与续作指引见 **STATE-当前进度.md**。
> **⚠ 2026-09-12 晚：需求澄清与路线重估**——见下方 §1.1 与 `对照与取证-ElevenClock稳定性与优效日历机制-2026-09-12.md`。原始需求为**横向长条布局**（本文档所载「两行布局」为注册表时代能力约束下的误译）；同机对照证实 ElevenClock 稳定、优效日历为注入 Explorer 改造原生时钟 XAML 路线。**下一代架构首选注入路线，本文档的覆盖式架构定位为第二代遗产**（鼠标钩子/菜单/数据链保留，几何状态机/采样链待退役）。

---

## 1. 背景与目标

### 1.1 需求澄清（2026-09-12，追溯补记）

**原始意图（用户 2026-09-12 澄清）**：任务栏时钟区应为**横向长条**——从左到右依次为天气、节日、节气、农历、时间。
**误译链**：注册表方案只能改格式串（改不了布局）→ 需求被压缩为「塞进原生时钟的两行文字」→ 覆盖式以「复刻原生时钟形态」为前提固化两行布局（见 D1 与 §4.1）→ R1~R9.4 全部迭代均在「时钟=原生宽度」的隐性假设下进行，无任何环节回头核对布局形态。**此后任何时钟呈现层设计（注入 XAML 面板 / GDI 覆盖 / 其他）必须以长条为默认形态核对该需求。**

早期方案（注册表写系统时间格式，让**原生时钟**代显农历）让原生时钟既做渲染者又做输入接收者，派生出整串补丁：低级钩子吞键路由、tooltip 两套压制（老系统隐藏 `tooltips_class32` / 新 Win11 两段式 `SendInput` 移光标）、菜单悬停守护、`NATIVE_DISMISS_PENDING` 重弹机制……每次 Win11 更新都要重新对抗系统行为。

目标架构：**自绘覆盖窗口盖在原生时钟上，独占输入与显示**；原生时钟保持存活作为安全网（任何定位失败/探测超时/系统改版，兜底表现就是原生时钟照常显示）。

参考先例：ElevenClock（架构对标）、Windhawk taskbar-clock-customization.cpp（新 Win11 时钟区 XAML 资料）、TrafficMonitor（反面教材：子窗口嵌入路线已证伪）。

## 2. 架构决策（第二代历史记录，2026-09-12 晚起已失效）

> ⚠ 本表是 2026-09 覆盖式架构定型时的决策快照。第三代注入路线（`方案-注入式任务栏时钟第三代-2026-09-12.md` v2）批准实施后，本表 D1~D5 除「保留项」（见该方案 §7）外全部退役；此表仅供历史追溯，**不再是现行架构约束**。

| # | 决策 | 理由 |
|---|---|---|
| D1 | 复用 Tauri webview 覆盖窗口（`clock_overlay` + `ClockOverlayWindow.tsx`） | 农历（`lunar-typescript`）、天气（`src/http/weather.ts`）、秒级定时器、主题自适应全现成；原生 D2D 绘制等于全部重写 |
| D2 | 原生时钟保持存活，不注册表隐藏 | 兜底安全网；它的矩形是定位锚点 |
| D3 | 输入最终由覆盖窗口原生接收（`WS_EX_NOACTIVATE\|WS_EX_TOOLWINDOW`，不抢焦点） | tooltip 因指针到不了原生时钟而天然消失 |
| D4 | 定位复用现有 UIA 探测体系（`clock_window.rs`：探测/校验/缓存/持久化/重探） | 实战验证过的自愈逻辑直接继承 |
| D5 | **几何权威分离（第二阶段新增）**：覆盖层几何只消费「认可矩形 endorsed」，与钩子点击路由缓存彻底解耦 | 缓存如实跟踪全屏期布局（3660→3618），覆盖层消费它就会错位（实测） |

## 3. 分阶段实施状态总览

| Phase | 内容 | 状态 |
|---|---|---|
| 0 | 覆盖层挂载/贴合/底色实采/前端配色 | ✅ 完成并真机验收（1227996） |
| 1 | 定位健壮化：滑动跟随、采样色差、全屏显隐事件驱动、探测去抖 | ✅ 完成并真机验收（4eb15a6~8a599f0） |
| 2 | **输入接管**：前端 mousedown/contextmenu 事件 → `clock_overlay_action` 命令；钩子时钟区分支 `OVERLAY_TAKEOVER` 开关化（先部署验证、后翻默认值）；右键抬起铁律 webview 实现（preventDefault + mouseup 投递）；菜单锚点用后端 GetWindowRect | ⬜ 未开始 |
| 3 | 退役注册表文本方案：`useWindowsTrayClockBootstrap` 停用、一次性迁移 `disable_custom_clock`、设置语义迁移 | ⬜ 未开始 |
| 4 | 清死代码（钩子时钟区吞键/tooltip 压制/悬停守护）+ 改写 AGENTS.md | ⬜ 未开始 |

## 4. 当前实现全貌（Phase 0+1 完成态）

### 4.1 窗口与渲染

- **窗口**：`window_manager/windows/clock_overlay.rs::build_clock_overlay_window`——Tauri 透明窗口，附加参数 `--disable-features=CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows --disable-renderer-backgrounding`（对抗 WebView2 遮挡节流），`WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW`（收输入不抢焦点、不进 Alt+Tab）；数据目录固定 `webview-data\clock-overlay`。
- **渲染**：`ClockOverlayWindow.tsx` 两行文本（时间+星期 / 农历+天气），秒级 setInterval，200ms 1px 像素保活重绘（抗合成暂停），右对齐锚定（遮盖向左扩展时文字视觉位置不变）。
- **配色**：后端 `clock_overlay_appearance` 命令——任务栏「显示桌面」细条 9 点均值实采底色（`GetPixel`，紧邻时钟无图标污染），失败退 `SystemUsesLightTheme` 主题兜底；前端挂载与主题变化时拉取。

### 4.2 挂载与定位

- **启动**：`hook_runtime.rs::start_taskbar_runtime` → 独立线程 `ensure_clock_overlay_attached`（COM 初始化 → `build_clock_overlay_window` 隐藏构建 → 500ms×20 重试 `refresh_clock_area_cache` + 贴合 + 显示；失败保持隐藏，原生时钟兜底）。
- **矩形来源**：`clock_window.rs::get_clock_rect_via_uia`（HWND 路径优先，UIA 兜底）+ `validate_or_scale_clock_rect`（任务栏带校验+DPI 修正）+ 注册表 `ClockRect` 持久化。
- **重探**：500ms 轻量可见性检查 / 2s UIA 重探重贴（同一循环分频）。

### 4.3 显隐与几何状态机（核心，当前 bug 主战场）

`update_clock_overlay_visibility`（WinEvent 事件驱动 + 500ms 兜底）：

- **覆盖探测**：`taskbar_cover_probe`——任务栏左段 10%/25%/40% 三点 `WindowFromPoint`，命中任务栏=Visible，全部命中外部窗口=Covered（携带盖住者根 HWND），全落空=Unknown（退回 `is_foreground_fullscreen` 矩形判定）。
- **z 序潜入**：Covered 时把覆盖层插到盖住者正下方（`SetWindowPos(hwnd, insert_after=盖住者)`）——窗口全程在屏内、WebView2 渲染管线存活（SW_HIDE/移屏外均触发 Chromium 合成暂停，实测无效，故用 z 序）。
- **遮盖（N1）**：被盖期间 `note_probe` 记录原生时钟真实位置 → 与认可位取并集 → 窗口扩展为并集（不透明背景遮住原生残块，文字右对齐锚定右缘视觉不变）；退出后遮盖保持，探测**连续两次** == endorsed 才收缩回认可矩形。
- **采纳门**：Normal 态差异探测须**持续 ≥8s** 才采纳进 endorsed（防过渡态矩形混入；PotPlayer 慢退出曾骗过 500ms 门）。
- **覆盖去抖**：Covered→Normal 要求 Visible 持续 ≥300ms（PotPlayer 退出动画使探测 ~1.4s 周期抖动，曾致扩张/收缩循环）；去抖等待期分类化提前夺 z（时钟中心 `WindowFromPoint` 命中任务栏/自己 → 立即 `HWND_TOPMOST`，命中其他窗口不动作）。
- **任务栏滑动跟随**：`read_tray_state` 维护「静止位基准」（完全入显示器内才算静止——任务栏恒贴满屏宽的教训），跟随 dy 位移；全屏/滑出时移出屏幕（OFFSCREEN）。
- **几何入口唯一**：所有位置/尺寸只从 endorsed/遮盖矩形取，成套应用（位置+尺寸，防"认可左缘+遮盖宽度"错配）。

### 4.4 钩子（Phase 2 待接管部分）

WH_MOUSE_LL 低级钩子（`mouse_hook.rs`）目前仍负责：时钟区点击吞键与路由（左键切日历/右键菜单双模式）、WinEvent（前台切换+任务栏位移+前台窗口位移）驱动覆盖层显隐、桌面日历 WIN+D 恢复、2s 重探线程。

## 5. 排障史索引（每条都有实测数据，详见 DEBUG-全屏退出时钟瞬态错位.md）

| 问题 | 根因 | 修复 | commit |
|---|---|---|---|
| 覆盖层不出现 | 配置 `taskbarWidgetEnabled=false` | 开关置 true | — |
| UIA 探测失败 | 贴合线程漏 COM 初始化 | CoInitializeEx | — |
| 底色采样失效 | 任务栏底色横向渐变，众数策略失效 | 改通道均值+SystemUsesLightTheme | — |
| 色差 | 全宽中带采样被图标污染 | 改采「显示桌面」细条 | f83fac2 |
| 滑入滑出不同步 | 2s 轮询滞后 | WinEvent 事件驱动 | 4b2ef22 |
| 滑动不跟随 | 静止位误判+收起态被记基准 | 完全入屏才算静止；基准未学不现身 | 20005af |
| 全屏显隐慢 | 前台不变收不到事件 | LOCATIONCHANGE 放行前台窗口 | 1f85c6a |
| 时钟退出全屏延迟出现（1~2s） | SW_HIDE → WebView2 节流 | 尝试移屏外（仍节流）→ **z 序潜入** | 2924a26 |
| 退出瞬间宽矩形闪现 | Normal 态即时采纳过渡态 | 采纳门（持续≥8s） | 6281a64, ba74a28 |
| 滑出后不跟随（E 方案真机失败） | 退出后缓存残留全屏期布局值仍被消费 | **P1+P2 几何权威状态机**（endorsed 解耦） | f3235e9 |
| 退出过渡宽度抖动 | 覆盖探测 ~1.4s 周期抖动 | 覆盖探测去抖（Visible≥300ms） | 8a599f0 |
| 原生时钟残块露出 | 原生时钟自己滞后（XAML 布局），几何管线证清白 | **N1 受控遮盖**（并集预备+确认收缩） | c34c23f |
| 当前残余（z 序可见期/遮盖横跳） | 去抖等待期零窗口操作+遮盖状态一致性缺陷 | **R1/R2/R3 已实施待真机复验** | 见 STATE 文档 |

## 6. 验证工具

- `D:\agents_tmp\zslip_test.ps1`——ffplay 全屏进出 + 时钟中心命中翻转 + 100/400ms 截图（主力回归）；
- `D:\agents_tmp\burst_test.ps1` / `fullscreen_latency.ps1` / `follow_test.ps1` / `offscreen_test.ps1`——专项测量；
- `D:\agents_tmp\test_video8.mp4`——8s 测试视频；
- 诊断：`menu_dbg.log`（带毫秒时间戳）；`clockrect_debug` 标记文件开启几何全量日志；`clockrect: <source>` 埋点记录缓存写入来源。

## 7. 明确不做的事

不注册表隐藏原生时钟、不做像素级任务栏混合、不做子窗口嵌入、不重写原生绘制。
