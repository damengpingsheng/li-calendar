# 对照与取证：成熟时钟方案同机验证与路线重估（2026-09-12 晚）

> 背景：R9.4 暂停后，`REVIEW-R9.4持续消失与ResourceTimeout归因复核` 质疑「环境级（显卡 TDR）无解」定案（0x1cc 并非显示驱动专用代码、看门狗观测目标错位等），另一 agent 对话提出「先做成熟方案同机对照，再决定是否重写」。用户批准执行对照。
> **三句话结论**：
> ① ElevenClock 同机同压力下完全稳定（12 次全屏进出零消失，~1s 恢复）——「环境级无解」被推翻，持续消失属我们实现层（WebView2 呈现 + 恢复链缺陷）；
> ② 优效日历取证实锤：**注入 Explorer、就地改造原生时钟 XAML**（Windhawk 同路线），无独立时钟窗口——「几乎完美」来自结构性消除问题组；
> ③ 用户澄清**原始需求即横向长条**（天气|节日|节气|农历|时间从左到右），文档中的两行布局是注册表时代能力约束下的误译——**注入路线升级为首选**。

---

## 1. ElevenClock 同机对照实验

### 1.1 环境与方法

- ElevenClock 4.4.1.1（winget 安装），Qt6 渲染，时钟窗口落位 (3490,2077)-(3840,2158)，覆盖在原生时钟之上（也是覆盖式，但原生渲染路径）；
- 我们的 liCalendar 已停止，避免覆盖层干扰；ElevenClock 自动套用了遗留的注册表时间格式（显示带秒两行，与原生格式一致，肉眼难分——用 `WindowFromPoint` 鉴别归属 `elevenclock` 进程确认所见确为它）；
- 方法：既有 `frame_capture.ps1`（DPI 感知，时钟区域 260×106 逐帧差分，~33fps）+ 驱动 PotPlayer 进出全屏；
- **方法论教训**：前两轮作废——Enter 发给了最小化/未激活的 PotPlayer（`AppActivate` 不可靠），帧数据全为时钟走字。第三版起改 `SetForegroundWindow` + 每轮切换后**用窗口矩形==显示器矩形验证全屏确实发生**，6/6 轮通过才算数。

### 1.2 结果

| 轮次 | 条件 | 全屏进出 | 结果 |
|---|---|---|---|
| 静态视频轮（`frames_elevenclock3.csv`） | 视频暂停在末帧 | 6 次（矩形验证） | 每次退出后 ~1s 内恢复秒级走字；最后一轮退出后连续 40s+ 稳定；零消失、零冻结 |
| 播放中轮（`frames_elevenclock4.csv`） | 视频播放中 | 6 次（矩形验证） | 同上；最后一轮退出后 40s+ 稳定走字（maxPrev ~500/s 的秒针级差分） |

数据形态干净可读：全屏段 avgBase=26000（视频画面），间隙段回落 ~200-700（时钟走字）。**12 次进出无一例持续消失**——对照我们的应用在同场景下分钟级不恢复。

诚实边界：测试时段系统日志**无新的 0x1cc/WER 事件**——TDR 级驱动高压未在对照中复现，本对照为中等压力；但我们的应用恰是这类常规进出全屏时开始持续消失的，对照仍具判别力。

用户补充观察（2026-09-12）：ElevenClock 也存在切换后色差，但**消退比我们快**。若将来走独立窗口路线，应参考其源码（GPL，Qt）的采样节奏与触发时机。

### 1.3 结论

复核文档的工作假设证实：**问题在我们的实现，不是环境**。原生渲染的独立窗口时钟（Qt/GDI 路径）同机稳定；持续消失 = WebView2 呈现层 + 恢复链缺陷的组合。R9.4 总结中「WebView2 呈现架构内可做之事已全部做完、剩余为环境级」的定案**不再成立**。

## 2. 优效日历机制取证

### 2.1 静态证据（安装目录 `D:\Program Files\优效日历\`）

| 组件 | 作用 |
|---|---|
| `win11_hook.dll / win11_hook_x64.dll` | Win11 路径：注入 explorer，改造任务栏时钟 XAML |
| `SystemClockHook.dll / _x64.dll` + `SystemClockHookHost.exe / _x64.exe` | Win10 经典路径（`SetWindowsHookEx`+`TrayClock` 子类化）+ 注入宿主/IPC |
| `YXCalendar.exe` | 主程序（DuiLib 原生 DirectUI，非 Electron；附带 CLI 本地 API，见其目录内 AGENTS.md） |

### 2.2 运行时证据

- 启动后 `explorer.exe` 模块列表出现 `win11_hook_x64.dll`——**注入实锤**；注入通道为 `InitializeXamlDiagnosticsEx`（XAML Diagnostics 官方调试接口加载 DLL 进目标进程 XAML 运行时，Windhawk 同款通道）；
- **时钟区域 `WindowFromPoint` 命中 explorer 自己的 `TrayNotifyWnd`——没有任何独立时钟窗口**；
- PDB 残留符号名直接暴露实现骨架：
  - `TrackClockTextBlock(…, TextBlock const&, bool)`——在任务栏 XAML 树中找到并持续跟踪原生时钟 TextBlock（Win11 任务栏为 WinUI/XAML）；
  - `ApplyStyleToTrackedTextBlocks(ClockStyleSnapshot const&)`——把字体/格式/颜色快照应用到原生 TextBlock；
  - `RunOnUiElementDispatcher(UIElement, function)`——在任务栏自己的 XAML UI 线程 dispatcher 上执行；
  - `YXCalendarTaskbarTap`（IClassFactory + IOleObjectSite 的 WinRT COM 对象）——**点击路由**：时钟点击经 COM 站点转给主程序弹窗；
  - `RestoreSuppressedClockToolTips` / `RestoreClockHoverTracking`——tooltip/悬停在原生元素层面处理；
  - `RequestWin11HookRefresh` / `RequestWin11HookUnload`——graceful 刷新/卸载（实测：强杀进程后 DLL 仍驻留 explorer，直到 explorer 重启——卸载依赖正常退出路径）。

### 2.3 结论

优效日历 = **Windhawk taskbar-clock-customization 同路线的商用实现**（PLAN 文档早已将其列为参考先例）。它「几乎完美」的原因：背景是任务栏材质本身（不存在采样/色差）、z 序是任务栏自己的（不存在被埋/全屏潜入）、位置是原生布局（不存在跟踪回摆）、渲染走 explorer 合成管线（不存在 WebView2 停止呈现）——**不是把假时钟做得更像，而是把真时钟改成自己的**。且证明该路线在本机 Win11 26200 上可用。

## 3. 长条需求澄清（2026-09-12，重要）

**用户澄清原文要点**：最开始提出农历天气需求时，想要的就是**横向长条布局**（从左到右：天气、节日、节气、农历、时间），而非上下两行。

**文档考古**：README（「可显示星期等信息」）与 PLAN D1/§4.1（「两行文本（时间+星期 / 农历+天气）」）均无长条记录。误译链：注册表方案只能改格式串（改不了布局）→ 需求被压缩成「塞进两行文字」→ 覆盖式以「复刻原生时钟形态」为前提固化两行 → R1~R9.4 全部在时钟宽度参照系下迭代。**没有任何环节回头核对过布局形态需求。**

### 3.1 路线决策矩阵（含长条需求后）

| 路线 | 满足长条需求的方式 | 代价 |
|---|---|---|
| 注册表（第一代） | 做不到（只能改文字） | — |
| 覆盖式 WebView/GDI（第二代） | 注册表占位 hack：写宽格式让原生时钟撑宽、覆盖层画长条 | 宽度靠字体度量间接控制；崩溃残留宽条；tooltip 露馅；全屏遮盖机制在 ~500px 上重演；第一代+第二代脆弱点叠加 |
| **注入原生时钟（Windhawk/优效路线）** | XAML 树插入真面板（StackPanel+多 TextBlock/图标），布局引擎自动让位、托盘图标自动左移 | explorer 崩溃责任；Windows 大版本更新需适配 XAML 树结构；杀软误报风险；注入/卸载生命周期管理 |

注入路线实现分两档：①只改原生 TextBlock 文字（宽度自动增长，Windhawk mod 层面）；②插入自定义 XAML 面板（五段独立样式+独立点击事件，完全体——优效 `YXCalendarTaskbarTap` 证明点击路由可行）。数据刷新走主进程→DLL 的命名管道 IPC（优效同款）。

### 3.2 架构史观（防止重复犯错）

完整脉络：注册表改原生时钟（第一代，死穴=输入仍靠原生时钟）→ 覆盖式 WebView（第二代，死穴=盖文字/追位置/仿背景整组问题+WebView2 呈现不可靠）→ **注入改造原生时钟（第三代候选）**。注意：第三代并非回到第一代——第一代只能改文本且输入无解，第三代拥有 XAML 元素级的样式/布局/输入控制。每次换架构都源于上一代补丁堆到不可维护；这次先做对照验证再动手，勿再带未验证假设跳进重写。

## 4. 下一步（待用户确认后执行）

1. **注入路线 PoC**（首选方向），验收项：
   - 注入 explorer（XAML Diagnostics 或 Windhawk 式加载）→ 枚举出时钟 TextBlock；
   - 改一次文字/颜色（最小验证）→ 插入自定义五段面板（宽度自适应、托盘图标避让）→ 各段独立点击事件 → graceful unload（explorer 不重启情况下干净退出）→ explorer 重启后自动恢复注入；
   - 参考资料源：Windhawk `taskbar-clock-customization` mod（开源）、优效符号骨架（§2.2）；
2. **备选**（注入被否决时，如不愿接受 explorer 注入风险面）：GDI 覆盖层 + 注册表宽度占位；
3. **无论走哪条**，先修两个确定性缺陷（看门狗观测目标改前端呈现心跳、`LAST_FOLLOW_POS` 改成功后提交）——恢复链可靠性是任何路线的共同前提；
4. 存量代码处置（注入路线下）：鼠标钩子/右键菜单/桌面日历/农历天气数据链**全部保留**；几何状态机/z 自愈/采样链大部分退役。

## 5. 环境注记

- ElevenClock 4.4.1.1 仍安装在本机（进程未运行；`winget uninstall SomePythonThings.ElevenClock` 可移除）；
- 优效日历强杀后 `win11_hook_x64.dll` 仍驻留 explorer（至 explorer/系统重启），期间无害（宿主已死无 IPC）；
- 本机遗留注册表自定义时间格式（liCalendar 被强杀未走恢复路径所致），下次 liCalendar 正常退出会恢复；
- 证据文件：`D:\agents_tmp\frames_elevenclock3.csv`、`frames_elevenclock4.csv`、`eleven_stress3.ps1`、`eleven_stress4.ps1`（含全屏矩形验证方法论）。
