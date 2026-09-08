# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-09 凌晨（R6 后，**应用户要求暂停**） · 用途：新会话窗口快速接续
> **本轮（R1~R6）完整总结见 `总结-退出全屏左缘收缩排障R1-R6.md`（先读它）**
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 完成；R1-R3 真机复验①②③通过；④左缘收缩感知经 R4（归位即缩）→R5（settle 门，方向反）→**R6 实时跟踪（b66cfb3，当前方案）**三轮迭代，真机复验「收缩开始时刻已快很多，但仍可察觉」，**用户决定暂停接受现状**；遗留 ④残余感知、y=2071 垂直漂移两项疑点，主线下一步为 Phase 2 输入接管。

## 2. R4→R5→R6 归因链（详见总结文档 §3/§6）

- 真机 23:39 完整 UIA 读数序列（关键证据）：退出后**原生时钟自己在 3618↔3660 间回摆 4 趟约 5s**（PotPlayer 退出驱动布局反复切换）。
- R4 病因：第一次收缩及时，但原生又回全屏位被迫再扩展+回摆证据被相位翻转清空致残块暴露；另 mask 在场即 150ms 加密致整场全屏 UIA 开销。
- R5 病因：settle 门 1.2s 与用户诉求（收缩开始时刻要贴住归位时刻）正反。
- **R6 实时跟踪**：扩展/收缩逐读数对齐原生真实位置——读到认可位立即收缩（时点=归位+一次读数量化 ≤150ms）、读到非认可位立即扩展；回摆证据跨相位保留；轮询加密按「3s 内见过 Visible」维持。
- 真机复验结论：收缩开始时刻大幅改善，残余感知接受现状（物理下限分析见总结文档 §6）。

## 3. 部署与回归状态

- ✅ R6 已提交（`b66cfb3`，squashed 分支，未推送）、已部署（特征串 `shrink (native endorsed)` 校验通过）、进程正常运行
- ✅ 用户真机复验：④明显改善、残余可感知，**暂停**
- 调试设施已收：`clockrect_debug` 标记文件已删（恢复=重建空文件）；5 分钟心跳自动化已删

## 4. 下一步（若恢复，新会话按序执行）

1. 读 `总结-退出全屏左缘收缩排障R1-R6.md` §7 恢复建议（首选截屏对齐分析定位残余感知来源）
2. **遗留独立疑点**：y=2071 垂直漂移（read_tray_state 跟随 dy，自动隐藏任务栏场景）
3. 主线：Phase 2 输入接管（PLAN §3，安全顺序：先 `OVERLAY_TAKEOVER` 开关验证后翻默认）

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
