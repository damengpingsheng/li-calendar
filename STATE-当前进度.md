# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 深夜（R4 后） · 用途：新会话窗口快速接续
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**（先读它）
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）
> 两份外部评审：`REVIEW-全屏退出时钟瞬态错位-第三阶段分析.md` / `REVIEW-...-第四阶段分析.md`（均已采纳大部分意见）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 已完成；R1/R2/R3 已提交（c2eeed1）并经**用户真机复验通过**（①原生时钟几乎不可见 ②出现干脆 ③右缘正常），用户对④左缘收缩的感知度提出「刚好盖住原生归位时间即可」的要求 → **R4「归位即缩」已实施、已部署、ffplay 回归通过，未提交、未真机复验**。

## 2. R4 改动内容（归位即缩，本轮未提交）

用户诉求：遮盖保持期不再附加保守等待，收缩时点=原生时钟归位时点。三处修改：

- **note_probe 权威提前确认**（clock_overlay.rs）：Covered 态收到 UIA 读数==endorsed 且 mask 已展开 → 跳过覆盖探测去抖直接切 Normal + 置 shrink_requested（UIA 是定位原生时钟的同一权威；mask 未展开不触发，防左段探测误判 Covered 时 Normal/Covered 空翻）。Normal 态收缩确认从 2x 降为 1x（UIA 单次确认，`shrink_confirm` 字段已删）——误确认最坏代价是多一次扩展-收缩循环（设计内自愈）。
- **遮盖保持期 UIA 加密轮询**（mouse_hook.rs 重探线程）：`clock_overlay_mask_held()` 为真时轮询 500ms→150ms，且每针都做 UIA 重探+重贴（原来 2s 一针）；mask 清除后自动回 500ms。加密只持续全屏退出瞬态（数秒），开销可忽略。
- ffplay 回归实测（23:14）：mask 保持期 UIA 读数间隔 ~230ms；退出时 `phase->normal (uia native-back fast-path)` 后 **9ms** 内完成收缩；遮盖保持期=原生归位时间+一次轮询量化（改造前真机实测归位后多等 ~1.3s）。

## 3. 部署与回归状态

- ✅ R4 构建通过、已部署（特征字符串 `uia native-back fast-path` 已验证在产物中）、进程在跑
- ✅ ffplay 自动化回归通过（见 §2 时间线）
- ❌ **未提交**（clock_overlay.rs / windows.rs / window_manager.rs / mouse_hook.rs modified；STATE 本身）
- ❌ **未真机复验**（请用户 PotPlayer 进出全屏，重点看④左缘收缩是否已不显眼）

## 4. 下一步（新会话按序执行）

1. `git add -A && git commit`（中文信息：R4 归位即缩——UIA 权威单次确认+遮盖保持期 150ms 加密轮询）
2. 请用户 **PotPlayer 真机复验**第④项：左缘收缩应缩到"几乎无感"（约 0.2s 内完成）；①②③上轮已通过
3. **遗留独立疑点（未归因完）**：真机 22:49:10 出现任务栏滑出/滑回一次后覆盖层落位 y=2071（认可 2076，垂直漂 5px）——read_tray_state 跟随 dy 路径的漂移，与 R4 无关，复验时顺带观察时钟是否垂直贴边
4. 全部闭环后：删 `D:\agents_tmp\clockrect_debug` 标记文件；进入 Phase 2 输入接管（见 PLAN §3 表格，含安全顺序：先加 `OVERLAY_TAKEOVER` 开关部署验证、后翻默认值）

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`，构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`）
- 部署后要 `taskkill //IM msedgewebview2.exe //F` 一次再启动（WebView2 浏览器进程复用旧参数，冷启动才生效）
- 诊断：`D:\agents_tmp\menu_dbg.log`（毫秒时间戳）；几何全量日志门控 = 存在 `D:\agents_tmp\clockrect_debug` 文件（**当前已开启**，验完问题记得删，否则日志涨得快）
- **日志分析陷阱**：log 里混有无时间戳的历史行（毫秒时间戳功能加入前所写，含已废弃的 exit-pending 实验日志）；用 awk/grep 按时间过滤时务必带 `^\[HH:MM:` 前缀锚定，字符串比较会误捞无前缀行（`"clockrect:" >= "[23:1"` 为真）
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 宽158；全屏期 (3618,2076,3769,2160)；显示器 3840×2160 @175%
- PotPlayer 路径：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- ffplay：`D:\environment\ffmpeg\ffmpeg-9.0.1-full_build\bin\ffplay.exe -fs -autoexit D:\agents_tmp\test_video8.mp4`（测试有声）；回归脚本 `D:\agents_tmp\zslip_test.ps1`
- 排障方法论（本 bug 的教训）：每轮修复必须有日志/截图实测证据；用户真机现象与自动化回归是两套判据，ffplay 通过≠真机通过；外部评审的多处"过度推断"指摘都成立，结论措辞要留余地

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（重写版：架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10（含各阶段已撤销推断的勘误标注） |
| REVIEW-*-第三阶段分析.md / *-第四阶段分析.md | 外部评审（大部分意见已采纳，留个别未验证假设） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/显隐/跟随/R4 fast-path） |
| src-tauri/src/windows_hook/{mouse_hook,clock_window}.rs | 钩子/探测（note_probe 喂状态机；R4 加密轮询在 mouse_hook.rs 重探线程） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端渲染 |
