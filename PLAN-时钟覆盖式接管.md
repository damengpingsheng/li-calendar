# 方案：任务栏时钟覆盖式接管

> 状态：已评审待实施 · 2026-09-07
> 路线：**覆盖式**——自绘透明置顶窗口盖在原生时钟上并接收全部输入，原生时钟保持存活作为安全网。
> 取代：现行「注册表写系统时间格式，让原生时钟代显农历」方案（`registry_clock.rs` 那条链）。

---

## 1. 背景与目标

现行方案让**原生时钟**既做渲染者又做输入接收者，由此派生出一整串补丁：低级钩子吞键路由、tooltip 两套压制（老系统隐藏 `tooltips_class32` / 新 Win11 两段式 `SendInput` 移光标）、菜单悬停守护、`NATIVE_DISMISS_PENDING` 重弹机制、右键抬起配对吞放……每次 Win11 更新都要重新对抗系统行为。

目标架构把这些整类消灭：

- 覆盖窗口**独占输入**：左键/右键原生落在自己窗口上，tooltip 因指针永远到不了原生时钟元素而天然不弹；
- 原生时钟**保持存活**：任何定位失败/探测超时/系统改版，兜底表现就是原生时钟照常显示——用户永远面对一个能用的时钟；
- 钩子从「时钟区交互中枢」降级为「桌面日历守护」（WIN+D 恢复仍需要它），时钟区分支整体退役。

参考先例：ElevenClock（架构对标：独立顶层窗口 + 覆盖 + 探测重试，不 hook 不注入）、Windhawk taskbar-clock-customization.cpp（新 Win11 时钟区 XAML 结构的公开资料）、TrafficMonitor（反面教材：子窗口嵌入在新 Win11 上反复碎掉，印证独立顶层窗口是对的选择）。

## 2. 架构决策（四条，实施中不可动摇）

| # | 决策 | 理由 |
|---|---|---|
| D1 | **复用现有 Tauri webview 覆盖窗口**（`clock_overlay` + `ClockOverlayWindow.tsx`），不做 D2D/DWrite 原生绘制 | 农历（前端 `lunar-typescript`）、天气（`src/http/weather.ts`，30 分钟刷新）、秒级定时器、深浅主题监听全部现成；webview-data 目录也已固定。原生绘制等于全部重写 |
| D2 | **原生时钟保持存活，不注册表隐藏** | 兜底安全网 + 它的矩形就是定位锚点（`get_clock_rect_via_uia` 找的就是它）。注册表隐藏要重启 explorer 才生效，代价不成比例 |
| D3 | **输入全部由覆盖窗口原生接收**：移除 `set_ignore_cursor_events(true)`，改加 `WS_EX_NOACTIVATE \| WS_EX_TOOLWINDOW` | 收到点击但不抢前台焦点；钩子对时钟区彻底放手 |
| D4 | **定位复用现有 UIA 探测体系**：`clock_window.rs` 的探测/校验/缓存/注册表持久化/2 秒重探线程全部保留 | 矩形缓存是历史重灾区，但那套自愈逻辑（`refresh_clock_rect_if_in_taskbar` + 2s 轮询 + `ClockRect` 持久化）已经过实战，直接继承 |

## 3. 现状盘点（已存在、本方案直接启用的资产）

| 资产 | 位置 | 状态 |
|---|---|---|
| 覆盖窗口构建 | `src-tauri/src/window_manager/windows/clock_overlay.rs` → `build_clock_overlay_window` | **悬空**：全仓库无调用点；当前为鼠标穿透设计 |
| 贴合函数 | 同文件 `relocate_clock_overlay`（按 `CLOCK_AREA_RECT_CACHE` set_size/set_position） | 已实现，未接线 |
| 时钟矩形探测 | `windows_hook/clock_window.rs`：`get_clock_rect_via_uia`（`find_clock_window` + UIA 兜底）、`validate_or_scale_clock_rect`（任务栏带校验 + DPI 修正）、持久化 `HKCU\Software\liCalendar\ClockRect` | 在用（供钩子判定） |
| 2 秒重探线程 | `windows_hook/mouse_hook.rs` → `start_hook_message_thread`（L163） | 在用 |
| 前端时钟视图 | `src/windows/ClockOverlayWindow.tsx`：两行文本（时间+星期 / 农历+天气）、秒级 setInterval、`prefers-color-scheme` 监听、天气 30 分钟 | 已实现；当前**无背景色**（透明） |
| 左键行为 | `CalendarWindowManager::toggle_clock_calendar()`（`hook_runtime.rs::handle_left_click` 调用） | 在用，直接复用 |
| 右键菜单双模式 | `clock_context_menu.rs`：默认 `track_native_clock_menu`（L433）；标记文件 `D:\agents_tmp\ccm_tauri_menu` 切 Tauri 窗口菜单（L219）；另有 `ccm_disabled` 实验开关（`hook_runtime.rs:75`） | 在用，菜单动作回流逻辑复用 |
| 注册表文本方案（退役对象） | `registry_clock.rs`：`set_custom_clock_text`(L274) → `update_taskbar_clock_display`(L336) → `update_system_time_format`(L369)；前端 `useWindowsTrayClockBootstrap.ts` 自动应用、`useClockManager.ts::handleApplyClock` 手动应用 | 在用 |
| 退出清理 | `lib.rs::request_app_exit`（L94-109）：`disable_custom_clock` → 卸钩 → sleep 300ms → `std::process::exit(0)` | 在用，结构保留 |

## 4. 唯一的新难点：透明缝隙（透底叠字）

原生时钟还在底下渲染，覆盖窗口若保持透明背景，原生文字会从透明像素透出来。处理：

- 覆盖窗口绘制**不透明背景**：
  1. 读 `HKCU\...\Themes\Personalize` 的 `AppsUseLightTheme` 判深浅，取基色（深 ≈ `#202020` 系 / 浅 ≈ `#f3f3f3` 系）；
  2. 用 GDI `GetPixel` 从任务栏实际像素采一次底色做修正（应对强调色/Mica 变体）——启动时与主题变化广播时各做一次；
  3. 后端经命令/事件把颜色传给前端（前端现有 `prefers-color-scheme` 监听保留作初始值，后端值覆盖它）。
- 采样失败时退化为按主题取固定深/浅色，**不允许回退透明**。
- 尺寸策略：宽 = max(时钟矩形宽, 文本所需宽)，**右缘锚定时钟矩形右缘向左扩展**（左侧是托盘图标区，右侧没空间）；高 = 时钟矩形高。
- 已知限制（记入设置页文案，不解决）：TranslucentTB 类全透明任务栏用户会看到一块实色。

## 5. 分阶段实施

### Phase 0 · 竖起窗口（最小闭环）

1. **接入启动**：`app_runtime/windows/hook_runtime.rs::start_taskbar_runtime`（L14）里，钩子线程启动后追加：`build_clock_overlay_window` → `refresh_clock_area_cache()` → `relocate_clock_overlay`。注意该函数受 `taskbar_widget_enabled` 开关门控，覆盖窗口同样受控（见 Phase 3 第 4 点语义迁移）。
2. **窗口属性**（`clock_overlay.rs`）：
   - 删 `window.set_ignore_cursor_events(true)`；
   - Win32 侧对 hwnd 加 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`（`SetWindowLongPtrW`），显示走 `SW_SHOWNOACTIVATE`，保持 `always_on_top`；
   - 层级注意：`HWND_TOPMOST` 需高于任务栏（任务栏本身是 topmost，覆盖层要显式再 `SetWindowPos` 压过它）。
3. **不透明底 + 布局**（`ClockOverlayWindow.tsx`）：根容器加背景色（第 4 节来源）；保留两行布局与秒级定时器；容器 `height: 100dvh`、右对齐 padding，字体随窗口高度自适应（不写死物理像素）。
4. **尺寸自适应**：`relocate_clock_overlay` 按探测矩形 set_size/set_position，已有实现基本够用；宽度的 max(矩形, 文本) 策略此阶段先用矩形宽，文本溢出问题实测后再调。

**验收**：启动后时钟区显示我们的两行时钟，原生文字不透出、不叠字，秒级走字，深浅主题下颜色和谐。

### Phase 1 · 定位健壮化

1. 2 秒重探线程保留为兜底；`relocate_clock_overlay` 只在矩形变化超过阈值（如 ≥2px）时才移动窗口，避免周期性微抖。
2. 钩子消息线程窗口过程加 `TaskbarCreated` 广播监听：explorer 重启 → 强制重探 + 重贴 + 重取任务栏底色。`WM_DISPLAYCHANGE`/`WM_DPICHANGED` 由 2 秒轮询自然覆盖，v1 不单独建事件。
3. **全屏隐藏**：重探线程顺带检测前台窗口是否全屏（窗口矩形 == 显示器矩形），是则隐藏覆盖层，退出全屏自动恢复（对齐 ElevenClock 行为）。
4. **任务栏自动隐藏**：检测 `Shell_TrayWnd` 不可见/移出屏幕 → 隐藏覆盖层，恢复则重贴。

### Phase 2 · 交互接管（顺序不能反）

1. **前端事件**（`ClockOverlayWindow.tsx`）：
   - `onContextMenu` → `preventDefault()`（**铁律：右键抬起才触发**——浏览器 `contextmenu` 默认在按下时触发，必须拦掉）；
   - `mousedown` 记录 `button === 2` 起点 → `mouseup`（button 2）时 `invoke('clock_overlay_action', { kind: 'right' })`；
   - 左键 `click` → `invoke('clock_overlay_action', { kind: 'left' })`。
   - `WS_EX_NOACTIVATE` 下 WebView2 仍正常收到鼠标消息（点击不激活窗口，事件照发），无需额外处理。
2. **新命令** `clock_overlay_action`（`commands/windows.rs`，参照 `clock_menu_action` L304 的写法）：
   - `left` → `window_manager.toggle_clock_calendar()`（与现钩子左键分支完全同源，`hook_runtime.rs::handle_left_click` L114 的逻辑迁来）；
   - `right` → 复用双模式菜单分流（迁自 `start_hook_listener` L83-106）：
     - 默认 `track_native_clock_menu`：**owner 改为覆盖窗口 hwnd**；**菜单锚点由后端取覆盖窗口矩形的右下角**（`GetWindowRect` + `TPM_RIGHTALIGN | TPM_BOTTOMALIGN`），**不信任前端传来的 screenX/screenY**（DPI 虚拟化风险）；现有的 y 钳制到工作区底缘、ALT 解锁 + `SetForegroundWindow` 重试逻辑原样保留；
     - `D:\agents_tmp\ccm_tauri_menu` / `ccm_disabled` 开关保留。
3. **钩子放手（开关化）**：`mouse_hook_proc`（`mouse_hook.rs:253`）时钟区分支加总开关 `OVERLAY_TAKEOVER`（static AtomicBool）：
   - 为真时该分支直接 `CallNextHookEx` 放行；
   - **先加开关并部署验证覆盖窗口点击全部正常，再把默认值翻为 true**——顺序反了会出现时钟点不动的空窗期；
   - 开关同时应使 `is_mouse_in_clock_area` 相关的钩子内自愈逻辑跳过（矩形探测线程保留，覆盖层还要用）。
4. **本阶段不删任何旧代码**：tooltip 压制（`hide_taskbar_tooltips`、`move_cursor_to_dismiss_tooltip`）、`spawn_menu_hover_guard`、`NATIVE_DISMISS_PENDING` 等先变 dead code，Phase 4 统一清。

### Phase 3 · 退役注册表文本方案

1. `src/hooks/settings/useWindowsTrayClockBootstrap.ts` 停止自动应用自定义文本（各窗口挂载时的自动链下线）。
2. **一次性迁移**：启动时若 `CUSTOM_CLOCK_ENABLED` 为真 → 调 `disable_custom_clock()`（L193，用 `LOCALE_NOUSEROVERRIDE` 取系统默认格式回写，幂等）→ 老用户升级即自动恢复系统时钟原状。
3. `request_app_exit` 里的 `disable_custom_clock` 调用**保留**（幂等兜底，清理老版本残留）。
4. **设置语义迁移**：`taskbar_widget_enabled` 从「注册表文本 + 钩子」开关变为「覆盖层」开关——`set_taskbar_widget_enabled(false)`（`mouse_hook.rs:42`）改为隐藏/销毁覆盖窗口 + `disable_custom_clock`（过渡期双清）；设置页 `WindowsTrayForm` 的自定义文本表单移除或改为覆盖层样式配置。
5. `registry_clock.rs` 的 `update_system_time_format`/`set_custom_clock_text` 写路径从命令层摘除；`ClockRect` 注册表持久化属于覆盖层定位，**保留**。

### Phase 4 · 清理死代码（真机验证通过后）

删除：钩子时钟区吞键/菜单路由/`NATIVE_DISMISS_PENDING`、`spawn_menu_hover_guard`、tooltip 两套压制、两段式移光标。**钩子本体保留**——桌面日历 WIN+D 恢复（`start_hook_listener` 里 `is_desktop_in_foreground` → `ensure_desktop_widget_on_desktop` 分支）仍依赖它。

同步改写 `AGENTS.md`「右键菜单与桌面窗口架构」章节：约束 4（tooltip 压制）、约束 5 的大部分内容作废，替换为覆盖层新约束（不透明底、右缘锚定、右键抬起铁律的 webview 实现方式、`OVERLAY_TAKEOVER` 开关语义）。

## 6. 验证清单（对齐 AGENTS.md 惯例）

| # | 场景 | 预期 |
|---|---|---|
| 1 | 启动后不做任何操作 | 覆盖层显示、秒级走字、农历正确、原生文字不透出 |
| 2 | 悬停覆盖层 | 无系统 tooltip、无闪烁、无光标跳动 |
| 3 | 左键 / 右键 | 日历切换 / 菜单弹出；菜单内上/下半 = 设置/退出；菜单外点击关闭并放行 |
| 4 | 时钟区内再右键 | 菜单关闭并立即重弹（webview 语境下应天然成立，需验证） |
| 5 | 退出三件套 | ①启动后直接右键退出正常；②退出后立即左键点桌面图标正常；③进程无残留、原生时钟恢复系统默认格式 |
| 6 | explorer 重启（`TaskbarCreated`） | 覆盖层自动回贴、底色重采 |
| 7 | 改分辨率 / DPI 缩放 / 多显示器 | 重贴正确，无叠字 |
| 8 | 任务栏自动隐藏 / 全屏视频 | 覆盖层隐藏，恢复后重现 |
| 9 | `OVERLAY_TAKEOVER` 翻转前后 | 两种模式下时钟都可点、无空窗 |
| 10 | Win11 26200 真机全流程 | 日志（`D:\agents_tmp\menu_dbg.log`）能定位每次点击；合成注入不能覆盖全部真实时序，最终以真实点击为准 |
| 11 | 构建部署 | `cargo build --release --features tauri/custom-protocol`；产物校验（时间戳/特征串）后按部署流程覆盖 `D:\Program Files\li-calendar\liCalendar.exe` 并确认进程存活 |

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| **透底叠字**（最大风险） | Phase 0 不透明底是硬前提；采样失败退固定深/浅色，绝不回退透明 |
| `TrackPopupMenu` 在「点击发生在自己窗口」的新语境下前台获取表现未知 | 双模式开关保留，出问题即切 Tauri 窗口菜单（`ccm_tauri_menu`）顶上 |
| 钩子放开时机出现时钟点不动 | `OVERLAY_TAKEOVER` 开关化：先部署验证输入接管，后翻默认值 |
| 实色块 vs 半透明任务栏（TranslucentTB 等） | 已知限制，设置页文案说明 |
| WebView2 常驻内存 | 单窗口 + 固定数据目录；实测不可接受再立项原生绘制（不在本方案内） |
| Win11 更新改任务栏结构 | 原生时钟兜底在线，覆盖层定位失败最多回到原生时钟，不再是系统级破坏（对比注册表方案写坏格式的历史风险） |

## 8. 明确不做的事

- 不注册表隐藏原生时钟（`ShowSystrayDateTimeValueName`），不重启 explorer；
- 不做像素级任务栏混合（Mica/亚克力仿真）；
- 不做子窗口嵌入任务栏（TrafficMonitor 路线，已证伪）;
- 不在本方案内重写原生绘制（D2D/DWrite）。

---

## 9. 实施进度（心跳续跑锚点 · 随进度更新）

- [x] Phase 0 代码全部完成：`clock_overlay.rs` 重写（隐藏构建 + `WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW` + 500ms×20 贴合重试 + 任务栏像素采样/主题兜底）、`window_manager/windows.rs` 再导出、`hook_runtime.rs` 启动接线、`mouse_hook.rs`（2s 重探重贴 + 开关显隐）、`commands/windows.rs` `clock_overlay_appearance` 命令、`lib.rs` 双处注册、`ClockOverlayWindow.tsx` 配色应用 + contextmenu 拦截
- [x] 前端构建：`pnpm build` 通过（dist 已更新，index-DqZCb3s2.js）
- [x] Rust 构建：`cargo build --release --features tauri/custom-protocol` 通过（含 3 轮修复：再导出链补全、windows 0.62 API 签名、贴合线程补 COM 初始化）
- [x] 部署到 `D:\Program Files\li-calendar\liCalendar.exe`（时间戳衔接 + 特征串校验通过）
- [x] 程序化验证：进程存活；覆盖窗口矩形 (3612,2076)-(3819,2160) 与注册表 ClockRect **逐像素一致**；任务栏底色实采 `#E9DACB` 成功（与 PowerShell 独立实测一致）；截图确认**无透底叠字、配色融合、秒级走字**
- [x] Phase 0 完成记录（2026-09-07 22:15）：

### Phase 0 完成记录

**过程中发现并修复的问题**
1. **用户配置 `taskbarWidgetEnabled=false` 导致整套任务栏运行时（含钩子）未启动**——首次部署后覆盖层不出现即此因；已把该配置改为 `true`（⚠️ 这同时改变了用户设置项，见下方"交付注意"）。
2. **贴合线程漏 COM 初始化**：UIA 探测要求线程 `CoInitializeEx`（钩子线程有、新线程没有），补上后贴合一次成功（attempt 0）。
3. **底色采样众数策略失效**：任务栏底色沿横向有壁纸透出的渐变（实测 5 点 5 值），"精确 RGB 众数≥2" 永不成立 → 改为**通道均值**；兜底注册表键由 `AppsUseLightTheme`（应用模式）改为 `SystemUsesLightTheme`（任务栏跟随**系统**模式）。修复后实采 `#E9DACB` 与任务栏融合（截图验证）。

**交付注意**
- ⚠️ 验证过程中把用户持久化配置 `taskbarWidgetEnabled` 从 `false` 改成了 `true`（`%APPDATA%\pro.softsoft.li-calendar\liConfig.json`），否则任务栏运行时整体不启动。用户如需关闭，走设置页开关（新代码下开关会同时显隐覆盖层）。
- 交互仍走钩子老路径（Phase 0 只接管显示与悬停）：左键切日历、右键菜单行为应与改动前一致，待用户真机点击验收。
- 遗留优化（非阻塞）：采样点遇图标仍可能拉偏均值（可加纵向多点/去极值）；`clock_overlay.rs` 内部分函数与再导出有 unused 警告（Phase 2/4 清理时一并处理）。

**下一阶段**：Phase 1（定位健壮化）→ Phase 2（输入接管，`OVERLAY_TAKEOVER` 开关化）→ Phase 3（退役注册表文本方案）→ Phase 4（清死代码 + 改写 AGENTS.md）。

- [x] **Phase 1 完成**（2026-09-07 22:30，已部署验证）：
  - 全屏前台隐藏：`is_foreground_fullscreen` 从 `windows_hook` 导出，2s 循环接入 `update_clock_overlay_visibility`（全屏/任务栏不可见 → `SW_HIDE`，恢复 → `SW_SHOWNOACTIVATE`，全程无激活）；
  - 任务栏自动隐藏检测：`taskbar_visible()`——`Shell_TrayWnd` 可见性 + 相对所在显示器四边滑出判定（TOL=4px，自动隐藏态窗口仍"可见"但整体滑出屏幕）；
  - 重贴阈值：Phase 0 的 `LAST_APPLIED_RECT` 精确匹配已覆盖（矩形未变零窗口操作，只重申 topmost）；
  - **偏离记录**：PLAN 原定的 `TaskbarCreated` 监听**未实现**——explorer 重启后 2s 重探线程天然完成重探+重贴+topmost 重申（≤2s 自愈），专设消息窗口收广播的复杂度不值；若日后用户对重启恢复的 2s 延迟敏感再补。
  - **部署中顺带实测**：重启后时钟矩形自发变化（207→158 宽，任务栏重排），重探线程 attempt 0 贴合新矩形，验证了变化跟踪链路。
  - **Phase 1 追加（用户反馈驱动）**：初版可见性挂在 2s 轮询上，用户实测"隐藏/出现比任务栏慢"（最坏 2s 滞后）。改为 **WinEvent 事件驱动**：`SetWinEventHook` 监听 `EVENT_SYSTEM_FOREGROUND`（全屏进出）+ `EVENT_OBJECT_LOCATIONCHANGE`（过滤 `Shell_TrayWnd`，自动隐藏滑入/滑出），挂在与鼠标钩子同泵线程；回调只做轻量判定+ShowWindow，2s 轮询降级为兜底。卸载对称（`uninstall_global_mouse_hook` 与 `WindowsHookManager::uninstall_hook` 双路径 `UnhookWinEvent`）。
  - 事件路径验证：程序化弹铺满屏窗体 400ms 后截图——覆盖层已隐藏（纯轮询在此窗口内来不及，判定为事件驱动生效）；关闭后恢复。自动隐藏滑入/滑出路径待用户真机验。
