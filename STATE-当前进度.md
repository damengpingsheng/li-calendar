# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 深夜（R5 后） · 用途：新会话窗口快速接续
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**（先读它）
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）
> 两份外部评审：`REVIEW-全屏退出时钟瞬态错位-第三阶段分析.md` / `REVIEW-...-第四阶段分析.md`（均已采纳大部分意见）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 完成；R1-R3 提交（c2eeed1）+ 真机复验①②③通过；R4「归位即缩」（14ebbce）被真机复验**证伪**——真机日志暴露病根是**退出动画期左缘反复横跳**（PotPlayer ~5s 动画内布局以 ~1.4s 周期回摆，几何逐次跟随）；**R5「单周期化」已实施、已部署、ffplay 回归通过（一次展开/一次收缩），未提交、未真机复验**。

## 2. R4→R5 的归因链（重要，防走回头路）

- R4 假设"收缩慢是因为保守等待"——真机复验后用户仍说"能察觉到"，读 23:39 真机日志：**收缩本身已是 9ms 级，可感知的是 PotPlayer 退出动画 ~5s 内任务栏布局以 ~1.4s 周期在全屏位形/常规位形回摆（原生时钟 3618↔3660 多次往返），几何逐次跟随 → 左缘 42px 反复跳变 + 三段原生残块暴露窗**（40.575/41.722/43.864 的 phase->normal applied endorsed 而 UIA 仍读 3618）。R1-R3 时代同样存在（22:49 日志两轮扩展-收缩），只是被 2x 确认拉长了每轮周期。
- **R5 单周期化**（当前未提交改动，clock_overlay.rs + mouse_hook.rs）：
  - `SETTLE_STABLE_MS=1200` 收缩 settle 门：遮盖展开后须「探测 Visible + UIA==认可位」持续 ≥1.2s（>动画期任一可见窗长度）才收缩一次；`shrink_requested` 字段删除，`settled_since` 计时取代。
  - 回摆证据跨相位翻转保留：Covered 重入不再清 `native_observed`（R2 的逐轮清空会反复丢证据）；Normal 相位收到非认可位读数且退出轮在场（mask/native_observed 任一非空）也喂 `native_observed` 并打断 settle。
  - Visible 分支新遮盖展开路径：探测 Visible 但原生观测≠认可位 → 立即展开（堵残块暴露窗；左段探测点与时钟区失配的兜底）。
  - fast-path（Covered+UIA==endorsed+mask 展开）保留但只做相位/z 切换，不再触发收缩，只累计 settle 计时。
  - 轮询加密条件改为 `clock_overlay_reprobe_interval_ms()`：退出 pending（Normal+mask）/进全屏等首观测（Covered 无 mask）150ms；**整场全屏播放（Covered+mask）回 500ms/2s**（R4 的 bug：整场加密）。
- 物理下限（对用户的话术）：布局回摆期遮盖必须存在（否则原生残块压托盘区），最优即"一次盖住→动画结束+1.2s→一次放开"，喇叭图标隐现从 N 次压成 1 次。

## 3. 部署与回归状态

- ✅ R5 构建通过、已部署（特征串 `settle shrink (visible+endorsed stable` 校验在产物中）、进程在跑
- ✅ ffplay 回归（23:55 时间线）：34.24 covered → 35.01 mask prepared（**唯一一次**，Covered+无mask 加密轮询 0.77s 展开比旧路径快）→ 42.79 phase->normal → 44.37 settle shrink（**唯一一次**，4ms 落位）→ 稳态；无二次扩展、无中途收缩
- ❌ **未提交**（clock_overlay.rs / windows.rs / window_manager.rs / mouse_hook.rs / STATE modified）
- ❌ **未真机复验**（请用户 PotPlayer 复验④：预期喇叭图标区一次变暗盖住、动画结束约 1.2~1.5s 后一次放开，无反复跳变）

## 4. 下一步（新会话按序执行）

1. `git add -A && git commit`（中文信息：R5 单周期化——settle 门/回摆证据跨相位保留/Visible 分支展开/轮询条件改退出 pending）
2. 用户 PotPlayer 真机复验④；若用户仍不满意，向其说明物理下限（§2 末），并可选做"mask 只盖原生时钟实际矩形（不并集到 3818）"的小优化——无法减少盖住时长，只能略减盖住面积
3. **遗留独立疑点**：真机 22:49:10 任务栏滑出/滑回一次后覆盖层落位 y=2071（认可 2076，垂直漂 5px），read_tray_state 跟随 dy 路径，与 R4/R5 无关
4. 闭环后：删 `D:\agents_tmp\clockrect_debug` 标记；进 Phase 2 输入接管（PLAN §3，安全顺序：先 `OVERLAY_TAKEOVER` 开关验证后翻默认）

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`，构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`）；**构建产物在 `src-tauri\target\release\li-calendar.exe`**（`cd src-tauri` 后构建，路径易错）
- 部署后要 `taskkill //IM msedgewebview2.exe //F` 一次再启动（WebView2 复用旧参数，冷启动才生效）
- 诊断：`D:\agents_tmp\menu_dbg.log`（毫秒时间戳）；几何全量日志门控 = 存在 `D:\agents_tmp\clockrect_debug` 文件（**当前已开启**，验完记得删）
- **日志分析陷阱**：log 里有无时间戳的历史行（毫秒时间戳功能之前所写，含已废弃 exit-pending 实验）；按时间过滤必须 `^\[HH:MM:` 前缀锚定（裸字符串比较会误捞：`"clockrect:" >= "[23:1"` 为真）
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 宽158；全屏期 (3618,2076,3769,2160)；显示器 3840×2160 @175%
- PotPlayer 路径：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- ffplay：`D:\environment\ffmpeg\ffmpeg-9.0.1-full_build\bin\ffplay.exe -fs -autoexit D:\agents_tmp\test_video8.mp4`；回归脚本 `D:\agents_tmp\zslip_test.ps1`
- 排障方法论：每轮修复必须有日志/截图实测证据；真机现象与自动化回归是两套判据（ffplay 退出无动画，复现不了回摆，R4 就是 ffplay 通过但真机不通过）；结论措辞留余地

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10 |
| REVIEW-*-第三阶段分析.md / *-第四阶段分析.md | 外部评审（大部分已采纳） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/R5 settle 门） |
| src-tauri/src/windows_hook/{mouse_hook,clock_window}.rs | 钩子/探测（R5 轮询节奏在 mouse_hook.rs 重探线程） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端渲染 |
