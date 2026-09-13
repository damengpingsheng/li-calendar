# 会话续作指引：覆盖层全屏退出错位排障（当前进度）

> 更新时间：2026-09-10 晚（R7.6 后，**应用户要求暂停**） · 用途：新会话窗口快速接续
> **本轮（R1~R7.6）完整总结见 `总结-退出全屏左缘收缩排障R1-R6.md`（先读它）**
> R7 的驱动评审：**REVIEW-退出全屏左缘收缩-R1-R6新一轮分析.md**
> 总方案/架构/排障史：**PLAN-时钟覆盖式接管.md**
> 本 bug 的完整归因链：**DEBUG-全屏退出时钟瞬态错位.md**（§0~§0.10）

---

## 1. 一句话现状

覆盖式接管 Phase 0/1 完成；④左缘收缩感知经 R4→R5→R6→R7(.1~.6) 十轮迭代收敛：收缩时刻贴住机制下限（读到归位 1~6ms 落位）、进全屏残块暴露窗消除（位形记忆）、启动右缘闪现修复（ATTACHED 门）、底色动态跟随（色差大幅改善）；**残留=退出全屏后偶发色差**（首轮 fast-path 收缩前 ≤220ms 换色窗口来不及），**用户接受现状暂停**；遗留 y=2071 垂直漂移疑点；主线下一步 Phase 2 输入接管。

## 2. R7 系列速查（详见总结文档 §3）

| 轮 | 修的什么 | 关键证据 |
|---|---|---|
| R7 | 评审 C/D/E：收缩后跟踪中断（观测窗 6s）/扩展滞后一轮/非原子应用 | 23:49 日志 5 次收缩 1~6ms 落位 |
| R7.1 | Covered 相位 relocate 即时展开（修扩展暴露窗 150~300ms） | 用户复验①「现象还是差不多」→ 定位 |
| R7.2 | 全屏位形记忆 last_native_layout（Covered 当轮预测展开） | 01:57 帧取证：暴露窗 191~323ms 内原生残块+托盘气泡可见 |
| R7.3 | ATTACHED 门（attach 前禁止几何应用，修 298×91 过宽启动闪现=「右缘偶发异常」元凶） | 02:03:19 日志 after 右缘 3947 |
| R7.4 | 根除收缩→重建死循环（Visible 分支只信当轮观测） | 02:16-17 诊断行 150ms 连环互搏 |
| R7.5 | 底色动态跟随：异步采样+≥3RGB 才 emit+采样点归属校验 | 三竞态采出 #00FFFD/#67EDE8/#C8F2EF 全实锤 |
| R7.6 | 换色时机三层：Visible 切换点 / 遮盖在场每针 / 收缩后 400ms 兜底 | 用户复验④「色差收缩完成后才消失」→ 复验⑤「有改善仍偶现」→ 暂停 |

## 3. 部署与调试设施

- ✅ R7.6 已提交 `cf6a2f8`（squashed 分支，未推送）、已部署（cmp 字节一致）、进程正常运行
- `D:\agents_tmp\clockrect_debug` 标记**当前已开启**（covered-fallback / visible-rebuild-check 诊断行常驻；不需要时删除该空文件即可）
- 工具脚本：`clock_baseline_probe.ps1`（A 基线对照）、`frame_capture.ps1`（31fps 帧取证）、`capture_with_potplayer.ps1`（一键录制）
- **3 分钟心跳自动化（automation-16a8e4d1）仍存在，应删**

## 4. 下一步（若恢复）

1. **偶发色差残留**（总结 §7.1）：进全屏前预采样存底 / z-reclaim 首个 Visible 探测点即时采样。
2. **轻微字体抖动**（§7.2）：CSS `transform: translateZ(0)` 提层。
3. **y=2071 垂直漂移**（独立疑点，自动隐藏任务栏场景）。
4. 主线：**Phase 2 输入接管**（PLAN §3，先 `OVERLAY_TAKEOVER` 开关验证后翻默认）。

## 5. 关键环境事实（新会话必读）

- 部署路径 `D:\Program Files\li-calendar\liCalendar.exe`；构建命令与 MSVC 环境见项目 AGENTS.md（必须带 `--features tauri/custom-protocol`，**改前端后必须先 `pnpm build`**）；构建产物 `src-tauri\target\release\li-calendar.exe`
- 部署后重启前清理 WebView2 **限定本应用进程**（按命令行含 li-calendar 过滤）
- **日志锚定必须按最近一次 `attached at attempt` 行号**——NTP 回拨致时间戳撞车，昨天的旧日志会冒充新回归（已踩坑一轮）
- 端到端量化 ≈220ms（150ms sleep + ~70ms uia-class 探测，本机 HWND 快速路径恒不命中）
- 时钟矩形本机实测：正常 (3660,2076,3818,2160) 与 (3649,2076,3819,2160) 随时间文字宽度摆动；全屏位形对应 (3618..)/(3607..)；显示器 3840×2160 @175%
- PotPlayer：`F:\Software\PureCodec20260630\PureCodec\x64\PotPlayerMini64.exe`（用户测试用，勿自动化）
- 排障方法论：每轮修复必须有日志/像素证据；结论措辞留余地；用户感知描述精确到「哪个动作的时刻」再归因；真机现象与 ffplay 回归是两套判据（ffplay 无退出动画）

## 6. 相关文件清单

| 文件 | 内容 |
|---|---|
| PLAN-时钟覆盖式接管.md | 总方案（架构+实施全集+排障史索引） |
| DEBUG-全屏退出时钟瞬态错位.md | 本 bug 完整归因链 §0~§0.10 |
| REVIEW-退出全屏左缘收缩-R1-R6新一轮分析.md | R7 驱动评审（C/D/E 证实修复；A 未做） |
| src-tauri/src/window_manager/windows/clock_overlay.rs | 覆盖层核心（状态机/遮盖/R7 全部机制/底色跟随） |
| src/windows/ClockOverlayWindow.tsx | 覆盖层前端（clock-appearance 事件监听） |
