# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 深夜（R6 后） · 用途：新会话窗口快速接续
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**（先读它）
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）
> 两份外部评审：`REVIEW-全屏退出时钟瞬态错位-第三阶段分析.md` / `REVIEW-...-第四阶段分析.md`（均已采纳大部分意见）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 完成；R1-R3（c2eeed1）真机复验①②③通过；R4（14ebbce）被用户澄清证伪（慢的是收缩开始时刻非动作本身）；R5（f4a9b6e）settle 门方向也反（归位后仍撑 1.2s）；**R6「实时跟踪」（e2794ca）已实施、已部署、ffplay 回归通过（归位读数后 1ms 收缩），未真机复验**。

## 2. R4→R5→R6 归因链（重要，防走回头路）

- 真机 23:39 完整 UIA 读数序列（关键证据）：退出后**原生时钟自己在 3618↔3660 间回摆 4 趟约 5s**（PotPlayer 退出过程驱动任务栏布局反复切换；40.2 归位→40.8 回全屏位→41.3 归位→41.9 回→42.35 归位→44.1/44.45 又回→44.69 最终归位）。
- R4 病因：第一次收缩 42.35 本身及时，但 44.1 原生又回全屏位被迫再扩展 → 用户见"左缘撑到很晚才缩"；且回摆证据被相位翻转清空/覆盖（41.336 的 3660 读数覆盖 40.806 的 3618），遮盖多次晚展开、残块暴露（40.575/41.722/43.864）。
- R5 病因：settle 门 1.2s 让"原生归位后左缘仍撑 1.2s+"，与用户诉求（归位即缩）正反。
- **R6 实时跟踪**（e2794ca，clock_overlay.rs）：扩展/收缩逐读数对齐原生真实位置——UIA 读到认可位即置 `shrink_requested` 并立即收缩（无稳定等待，时点=归位时点+一次读数量化 ≤150ms）；非认可位读数即撤销请求并喂 `native_observed`；回摆证据跨相位翻转保留（R5 遗产）；轮询加密新增「相位回 Covered 但 3s 内见过 Visible 保持 150ms」（修 R4 实测 42.5→44.1 UIA 空窗）；稳态全屏（Covered+mask+3s 无 Visible）仍 500ms/2s。
- 感知模型：回摆期扩展窗与系统自身布局跳动对齐（原生在全屏位时任务栏本来就是全屏排列、喇叭区被系统收起），每次跳变都是"与系统一起动"，无任何"我们额外撑着"的时间。

## 3. 部署与回归状态

- ✅ R6 构建通过、已部署（特征串 `shrink (native endorsed)` 校验在产物中）、进程在跑
- ✅ ffplay 回归（01:20 时间线）：20.37 covered → 21.07 mask prepared（唯一一次）→ 28.792 收缩请求+fast-path → **28.793 收缩落位（1ms）** → 稳态
- ✅ R6 已本地提交（e2794ca）
- ❌ **未真机复验**（请用户 PotPlayer 复验④：预期左缘只在原生真偏离时扩展，原生每次归位左缘立刻开始缩；回摆期会有几次与系统布局跳动同步的小幅收放，属系统自身行为）

## 4. 下一步（新会话按序执行）

1. 用户 PotPlayer 真机复验④；若仍不满意，抓屏幕截图对齐日志分析（PotPlayer 勿自动化，可请用户操作时后台跑截屏脚本记录时钟区 (3600,2076)-(3840,2160) 逐 200ms 帧）
2. **遗留独立疑点**：真机 22:49:10 任务栏滑出/滑回一次后覆盖层落位 y=2071（认可 2076，垂直漂 5px），read_tray_state 跟随 dy 路径，与本轮无关
3. 闭环后：删 `D:\agents_tmp\clockrect_debug` 标记；进 Phase 2 输入接管（PLAN §3，安全顺序：先 `OVERLAY_TAKEOVER` 开关验证后翻默认）

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`，构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`）；**构建产物在 `src-tauri\target\release\li-calendar.exe`**（`cd src-tauri` 后构建，路径易错）
- 部署后要 `taskkill //IM msedgewebview2.exe //F` 一次再启动（WebView2 复用旧参数，冷启动才生效）
- 诊断：`D:\agents_tmp\menu_dbg.log`（毫秒时间戳）；几何全量日志门控 = 存在 `D:\agents_tmp\clockrect_debug` 文件（**当前已开启**，验完记得删）
- **日志分析陷阱**：log 里有无时间戳的历史行（毫秒时间戳功能之前所写，含已废弃 exit-pending 实验）；按时间过滤必须 `^\[HH:MM:` 前缀锚定（裸字符串比较会误捞：`"clockrect:" >= "[23:1"` 为真）
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 宽158；全屏期 (3618,2076,3769,2160)；显示器 3840×2160 @175%
- PotPlayer 路径：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- ffplay：`D:\environment\ffmpeg\ffmpeg-9.0.1-full_build\bin\ffplay.exe -fs -autoexit D:\agents_tmp\test_video8.mp4`；回归脚本 `D:\agents_tmp\zslip_test.ps1`
- 排障方法论：每轮修复必须有日志/截图实测证据；真机现象与自动化回归是两套判据（ffplay 退出无动画，复现不了回摆）；结论措辞留余地；**用户的感知描述要精确到"哪个动作的时刻"再归因**（R4/R5 两轮弯轧均源于把"能察觉到"默认理解为"动作明显"）

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10 |
| REVIEW-*-第三阶段分析.md / *-第四阶段分析.md | 外部评审（大部分已采纳） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/R6 实时跟踪） |
| src-tauri/src/windows_hook/{mouse_hook,clock_window}.rs | 钩子/探测（轮询节奏在 mouse_hook.rs 重探线程） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端渲染 |
