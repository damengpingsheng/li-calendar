# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 深夜（R7 后） · 用途：新会话窗口快速接续
> **本轮（R1~R7）完整总结见 `总结-退出全屏左缘收缩排障R1-R6.md`（先读它）**
> R7 的驱动评审：**REVIEW-退出全屏左缘收缩-R1-R6新一轮分析.md**
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 完成；④左缘收缩感知经 R4→R5→R6→R7→R7.1 迭代后，**真机帧取证（01:57）定位残余感主体=进全屏时等 UIA 首读的 191~323ms 暴露窗（原生文字残块+托盘气泡可见）**；**R7.4（当前部署 64416）**=全屏位形记忆预测展开（R7.2，Covered 当轮展开不等首针 UIA）+ATTACHED 门（R7.3，修启动 298×91 过宽闪现——疑似「右缘偶发异常」元凶）+根除收缩→重建死循环（R7.4，诊断行实测 150ms 连环闪现后回归 Visible 只信当轮观测）；ffplay 部署后 45s 观察零循环零多余收缩，**待真机复验③**；主线下一步 Phase 2 输入接管。

## 2. R4→R5→R6→R7 归因链（详见总结文档 §3）

- 真机 23:39 完整 UIA 读数序列（R4 期证据）：退出后原生时钟在 3618↔3660 间回摆 4 趟约 5s（PotPlayer 退出驱动布局反复切换）。**注意：回摆是否独立于覆盖层存在尚未对照排除（评审 §3.A，A 实验待做）。**
- R4 病因：回摆证据被相位翻转清空致残块暴露 + mask 在场即 150ms 加密致整场开销。
- R5 病因：settle 门 1.2s 与用户诉求（收缩开始时刻贴住归位时刻）正反。
- R6 实时跟踪：扩展/收缩逐读数对齐；真机复验「大幅改善仍可察觉」→ 暂停。
- **R7（评审驱动）**：评审列出 A~F 六个候选，其中 C/D/E 静态代码核对全部证实并修复——
  - **C 跟踪中断**：收缩清空 mask/native 后轮询掉回 500ms/每4针 UIA、Normal 偏离读数只进 8s 候选路径 → 新增**退出观测窗** `exit_watch_until`（真实收缩后 6s 内维持 150ms + 偏离读数继续喂遮盖数据）；
  - **D 扩展滞后**：relocate 在遮盖保持期直接 return → 新增**即时扩展路径**（native 偏离且旧遮盖盖不住时当轮 `apply_mask_geometry` 应用）；
  - **E 非原子应用**：`set_size`+`set_position` 两连调用有中间态（右缘瞬停 3776）→ `apply_overlay_geometry` 改**单次 SetWindowPos**（SWP_NOZORDER）。

## 3. 部署与回归状态

- ✅ R7 已实施并**本地提交**（squashed 分支，未推送）、已部署（特征串 `mask expand (native diverged)` 校验通过）、进程正常运行
- ✅ ffplay 回归（23:23 时间线）：29.785 一次展开 → 37.997 读到归位即请求收缩 → 38.002 原子落位 → 观测窗 150ms 节奏持续 ~6s → 45.2/47.3 逐级回落稳态——与设计完全一致
- ⚠️ **待用户真机复验**：PotPlayer 退出操作；`clockrect_debug` 标记**当前已开启**（验完删除）
- WebView2 清理已改为**限定本应用相关进程**（按命令行过滤，评审 §7 指正后修正）

## 4. 下一步（新会话按序执行）

1. **真机复验③（R7.4）**：用户操作 PotPlayer 进/出全屏；预期①进全屏头一秒残块暴露消除（covered-fallback 诊断行 + 当轮 apply 全尺寸）②退出后无连环闪现（无 shrink→rebuild 连环）③此前帧取证工具可复用：`D:\agents_tmp\capture_with_potplayer.ps1`（40s 逐帧录制+PNG 存档）。
2. **观察项（未归因，无碍验收）**：a) endorsed 在 3660↔3649 间随时钟内容摆动（采纳门在多数派+少数派穿插下会锁旧值，当前两值覆盖关系安全）；b) 23:53~00:05 曾出现无 src 的 R6 格式日志行（疑旧副本进程，未复现）；c) y=2071 垂直漂移（自动隐藏任务栏场景）。
3. **A 基线对照**（评审实验，判定回摆归因）：`D:\agents_tmp\clock_baseline_probe.ps1 -Tag on/off` 各跑一轮对比回摆频率。
4. 若复验③通过：④项收尾（删除 clockrect_debug 标记、删心跳自动化），进主线 **Phase 2 输入接管**（PLAN §3，先 `OVERLAY_TAKEOVER` 开关验证后翻默认）。
5. 若仍可感知：帧取证+日志对齐定位（勿无证据堆机制）。

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`，构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`）；**构建产物在 `src-tauri\target\release\li-calendar.exe`**（`cd src-tauri` 后构建，路径易错）
- 部署后重启前清理 WebView2 **限定本应用进程**（PowerShell 按命令行含 li-calendar 过滤）
- 诊断：`D:\agents_tmp\menu_dbg.log`（毫秒时间戳）；几何全量日志门控 = 存在 `D:\agents_tmp\clockrect_debug` 文件（**当前已开启**，验完删除）；探测日志行格式 `uia rect=(...) src=<hwnd|uia-autoid|uia-class> ms=<耗时>`
- **日志分析陷阱**：按时间过滤必须 `^\[HH:MM:` 前缀锚定（裸字符串比较会误捞历史行）
- **端到端量化 ≈220ms 而非 150ms**：本机探测恒 `src=uia-class`（Win11 26200 无经典时钟窗口，HWND 快速路径不命中），每针 ~65-75ms + 150ms sleep
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 宽158；全屏期 (3618,2076,3769,2160)；显示器 3840×2160 @175%
- PotPlayer 路径：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- ffplay：`D:\environment\ffmpeg\ffmpeg-9.0.1-full_build\bin\ffplay.exe -fs -autoexit D:\agents_tmp\test_video8.mp4`；基线对照脚本 `D:\agents_tmp\clock_baseline_probe.ps1`
- 排障方法论：每轮修复必须有日志/截图实测证据；真机现象与自动化回归是两套判据；结论措辞留余地（勿称「物理极限」——评审 §4）；**用户的感知描述要精确到「哪个动作的时刻」再归因**；心跳自动化在任务完成后要删除

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10 |
| REVIEW-退出全屏左缘收缩-R1-R6新一轮分析.md | R7 驱动评审（C/D/E 证实修复；A/F 待实验） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/R7 观测窗+即时扩展+原子应用） |
| src-tauri/src/windows_hook/{mouse_hook,clock_window}.rs | 钩子/探测（轮询节奏、src/ms 日志） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端渲染 |
