# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 深夜 · 用途：新会话窗口快速接续
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**（先读它）
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）
> 两份外部评审：`REVIEW-全屏退出时钟瞬态错位-第三阶段分析.md` / `REVIEW-...-第四阶段分析.md`（均已采纳大部分意见）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 已完成并验收；当前卡在**最后一个真机 bug**：PotPlayer 退出全屏时残余「原生时钟先显示一段时间→我们的时钟才出现→边缘过宽→收缩正常」序列；第四阶段评审修复（R1/R2/R3）**已实施、已部署、ffplay 回归通过，但未提交、未真机复验**——新会话从这里接手。

## 2. 当前未提交的改动（R1/R2/R3，全部在 `clock_overlay.rs`）

对应第四阶段评审（REVIEW-*-第四阶段分析.md）§3.3/§3.4/§4：

- **R1（§3.3 位置尺寸不配套）**：Covered 分支遮盖目标 = `已展开 mask 优先，否则本轮新观测并集`；保持遮盖时**位置+尺寸成套应用**（旧代码在并集==endorsed 时只回左缘不缩宽 → 右缘 3860 冲进「显示桌面」区盖住图标）。`LAST_APPLIED_RECT` 同步保留。
- **R2（§3.4 跨轮复用+单样本收缩）**：新一轮 Covered 清空 `native_observed`/`shrink_requested`（防上轮 3618 直接扩展本轮）；`shrink_confirm` 计数——连续 **两次** probe==endorsed 才置收缩请求（单次相等可能是过渡期碰巧）。
- **R3（§4 F1 完善版）**：去抖等待期（Covered 态遇 Visible 探测、<300ms）分类化提前夺 z——时钟中心 `WindowFromPoint`：命中任务栏或本覆盖层根 → 立即 `HWND_TOPMOST`（仅 z，几何保持遮盖位）并清 `LAST_FOLLOW_POS`（使后续真 Covered 可重新潜入）；命中其他窗口（含还在放视频的播放器）**不动作**。
- 另：`dbg_log` 已加毫秒时间戳（Cargo 新增 `Win32_System_SystemInformation`）。

## 3. 部署与回归状态

- ✅ 构建通过、已部署到 `D:\Program Files\li-calendar\liCalendar.exe`、进程在跑
- ✅ ffplay 自动化回归通过：单次 covered→mask 扩展(200宽)→z-reclaim early×2（R3 生效证据）→phase->normal→shrink，末段探测全 3660
- ❌ **未提交**（`git status`: clock_overlay.rs modified；REVIEW-第四阶段.md untracked）
- ❌ **未真机复验**（ffplay 从未复现用户所见的完整异常序列，只能验机制不能验现象）

## 4. 下一步（新会话按序执行）

1. `git add -A && git commit`（中文信息：第四阶段评审修复 R1/R2/R3 + 评审文档入库）
2. 请用户 **PotPlayer 真机复验**，按四阶段现象逐项对照（DEBUG §0.10）：
   - ①原生时钟先显示的时长（R3 应大幅缩短）
   - ②我们的时钟出现是否干脆
   - ③边缘过宽（R1 应消除右缘冲进显示桌面区）
   - ④收缩回正常（保持 ~230ms 确认期，正常）
3. 若仍有残余：
   - `clockrect_debug` 标记文件已开（`D:\agents_tmp\clockrect_debug`），menu_dbg.log 有毫秒时间戳全量几何日志——先读日志归因再动手；
   - 评审遗留的待验证假设（勿当已证事实）：任务栏重申 topmost 未被直接观测；左侧探测点与右侧时钟区遮挡可能失配（§3.2——探测点在任务栏左段，代表不了时钟区）；WebView 背景绘制/合成未就绪未排除（§3.5——外框正确≠内容已画）；
   - 可做的单变量实验：F1 分类化已做，若①仍长 → 记录 overlay/tray/player 三窗口 topmost 属性与相对 z 序（评审第一步清单）。
4. 全部闭环后：Phase 2 输入接管（见 PLAN §3 表格，含安全顺序：先加 `OVERLAY_TAKEOVER` 开关部署验证、后翻默认值）。

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`，构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`）
- 部署后要 `taskkill //IM msedgewebview2.exe //F` 一次再启动（WebView2 浏览器进程复用旧参数，冷启动才生效）
- 诊断：`D:\agents_tmp\menu_dbg.log`（毫秒时间戳）；几何全量日志门控 = 存在 `D:\agents_tmp\clockrect_debug` 文件（**当前已开启**，验完问题记得删，否则日志涨得快）
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 宽158；全屏期 (3618,2076,3769,2160)；显示器 3840×2160 @175%
- PotPlayer 路径：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- ffplay：`D:\environment\ffmpeg\ffmpeg-9.0.1-full_build\bin\ffplay.exe -fs -autoexit D:\agents_tmp\test_video8.mp4`（测试有声）
- 排障方法论（本 bug 的教训）：每轮修复必须有日志/截图实测证据；用户真机现象与自动化回归是两套判据，ffplay 通过≠真机通过；外部评审的多处"过度推断"指摘都成立，结论措辞要留余地

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（重写版：架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10（含各阶段已撤销推断的勘误标注） |
| REVIEW-*-第三阶段分析.md / *-第四阶段分析.md | 外部评审（大部分意见已采纳，留个别未验证假设） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/显隐/跟随） |
| src-tauri/src/windows_hook/{mouse_hook,clock_window}.rs | 钩子/探测（note_probe 喂状态机） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端渲染 |
