# liCalendar（松鼠日历）项目规则

## 部署路径（现成的，构建完成后自动部署，无需询问）

- 目标路径：`D:\Program Files\li-calendar\liCalendar.exe`（安装目录，WebView 数据也在其下 `webview-data\`）
- 部署流程：`taskkill //IM liCalendar.exe //F`（如在运行）→ 复制新 exe 覆盖目标 → 重新启动 `liCalendar.exe` → 确认进程存活
- **部署后必须校验产物**：对比时间戳/`grep -ac "特征字符串" exe`，曾出现构建失败但旧 exe 被静默部署的情况

## 构建命令与注意事项

- 仅改 Rust 后端时：`cargo build --release --features tauri/custom-protocol`
  - **必须带 `custom-protocol`**，否则应用按 dev 模式加载 `devUrl`（localhost:3000），启动后白屏报 ERR_CONNECTION_REFUSED
  - 直接 `cargo build` 的产物名是 `target\release\li-calendar.exe`；`pnpm tauri build` 另生成 `target\release\liCalendar.exe`。部署时覆盖成目标路径的 `liCalendar.exe`
- 前端/完整发布：`pnpm tauri build`（产物另在 `target\release\bundle\nsis\`）
- 本机 MSVC 环境需手动设置（无 vswhere，Git Bash 的 `link` 会干扰，须把 MSVC 的 `Hostx64/x64` 放进 PATH 前面）：
  - PATH 追加：`D:\environment\npm-global`（pnpm）、`D:\environment\VsBuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64`
  - `LIB`：`D:\environment\VsBuildTools\VC\Tools\MSVC\14.44.35207\lib\x64;D:\environment\WindowsKits\10\Lib\10.0.26100.0\um\x64;D:\environment\WindowsKits\10\Lib\10.0.26100.0\ucrt\x64`（Windows 反斜杠路径格式）
  - `INCLUDE`：对应 MSVC `include` 与 WindowsKits `Include\10.0.26100.0` 下的 `um/ucrt/shared/winrt/cppwinrt`
- 项目内已设置 pnpm `verify-deps-before-run false`（esbuild 构建脚本会被 pnpm 策略拦截）

## 右键菜单与桌面窗口架构（重要）

- 时钟右键菜单**双模式**（`window_manager/windows/clock_context_menu.rs`）：
  - **默认：原生 `TrackPopupMenu`**（`track_native_clock_menu`，用户偏好系统风格）——属主窗口 + `TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_CENTERALIGN | TPM_BOTTOMALIGN`（y 钳制到 `SPI_GETWORKAREA` 底缘，菜单整体位于任务栏上方），同步返回命令后执行设置/退出；跟踪期间 `NATIVE_MENU_TRACKING=true`，钩子对所有事件纯放行（TPM 自带捕获，菜单外点击由它自行处理）。
  - **备用：Tauri 置顶窗口菜单**——存在 `D:\agents_tmp\ccm_tauri_menu` 文件时启用（`show_clock_context_menu` + `src/windows/ClockContextMenuWindow.tsx`）；失焦不隐藏，钩子按 `MENU_RECT` 判定菜单外点击并隐藏。
  - 共同铁律：**右键抬起（UP）才触发**；**显示时压制任务栏 tooltip**；前台获取只用 ALT 解锁 + `SetForegroundWindow` 重试（详见下方约束）。
- 桌面退出清理（`request_app_exit`）：恢复时钟注册表（留 300ms 传播）→ 显式 `uninstall_global_mouse_hook` → `std::process::exit(0)` 硬退出（`app.exit(0)` 间歇失效会留下半退出状态）。

### 结构性约束（违反会复发，血泪教训）

1. **禁止跨进程 `GWLP_HWNDPARENT`**：桌面日历曾挂为 Progman 的 owned window——跨进程 owner 会让系统**隐式合并两线程输入队列**（等价 AttachThreadInput，从启动起持续），导致桌面左键失灵/任务栏正常/右键自愈/退出残留。现为普通置底顶层窗口（`HWND_BOTTOM`，壁纸之上、应用之下），WIN+D 后由钩子在任意桌面左键点击时经 `ensure_desktop_widget_on_desktop`（IsIconic 检测）恢复。
2. **禁止 `AttachThreadInput`**（所有路径已全删；原生菜单前台获取只用 ALT 解锁 + SetForegroundWindow）。
3. **禁止失焦隐藏菜单**：真实环境焦点常被抢走，失焦即隐藏会让用户点在空处。
4. **tooltip 压制（v63 起主手段=tap 源头控制）**：任务栏 tooltip 悬浮在时钟正上方（菜单项位置），会视觉遮挡菜单并吃点击。**① 注入模式（现行生产）：host 菜单弹出前经管道发 `ttc <id>`**——tap 在 UI 线程关闭已显示的 ToolTip 并**保存+清除 ToolTipService 附加属性**（悬停计时器无从触发），回 `ttcack`（id+cleared，host 有界等待 120ms，未确认如实记录并照常弹菜单）；菜单存活期间 host 续期线程每 2s 发 `ttk <id>`（续租约+探测系统懒绑定回挂，发现即一并压制）；菜单关闭发 `ttr <id>` 恢复（回 `ttrack`）。tap 侧三层自愈：租约 8s 到期未续 → 自动恢复；管道断开（`AutoRestoreOnDisconnect`，进程死亡 ~100ms 内实测生效）→ 恢复；c1free/unload 面板拆除 → 恢复（恢复幂等）。**光标全程不动**（默认 NoMoveNoGuard；安装目录 `clock-menu-diagnostic.txt` 写 `legacy` 可回滚两段式移动+悬停守护）。② 老系统（经典任务栏）：`hide_taskbar_tooltips` 隐藏 `tooltips_class32` 窗口（对 XAML tooltip 无效，仅备用）。历史手段（已退役但保留为 legacy 回滚）：`SendInput` 两段式移动光标到「显示桌面」细条再跳菜单落点——指针必须进入其他任务栏元素才会重算悬停；XAML tooltip 对消息注入/`SetCursorPos` 免疫的实测结论仍有效，只是不再需要靠移动光标解决。
5. **右键抬起（UP）才触发菜单**；菜单显示期间钩子按 `MENU_RECT` 精确判定：点菜单内按上/下半路由动作（左右键均支持）、点菜单外隐藏菜单并放行点击给下层；**时钟区内再右键 = 关闭并立即重新弹出**（`NATIVE_DISMISS_PENDING` 整体吞掉这对事件，抬起时作为新右键投递，符合"每次右键都重新弹出"习惯）。**悬停守护已退役**（v63 默认 NoMoveNoGuard：tooltip 由 tap 源头压制，菜单开着指针停在时钟区不再自动关菜单；legacy 策略下守护随两段式移动一并回归）。注：钩子「菜单内抬起→上/下半路由」依赖的 `NATIVE_MENU_RECT` 从未写入（历史遗留死代码），实际菜单项点击由 TPM 自行处理兜底，勿依赖该路由。
6. **退出用 `std::process::exit(0)` 硬退出**：先恢复时钟注册表（留 300ms 传播）→ 卸钩 → exit。`app.exit(0)` 间歇失效会留下半退出状态。

## 验证要点（桌面集成类改动）

- 诊断日志：后端 `D:\agents_tmp\menu_dbg.log`（`dbg_log`）、前端 `D:\agents_tmp\overlay_err.log`（菜单 webview 的 loaded/mousedown/action）
- PowerShell 注入必须先 `SetProcessDPIAware()`，否则坐标被虚拟化；时钟区域约 (3792, 2159)
- **验收必须覆盖**：①启动后不做任何操作直接右键退出；②退出后立即左键点选桌面图标；③枚举 liCalendar 顶层窗口确认「桌面日历」owner=0x0（PowerShell `GetWindowLongPtr(hwnd,-8)`）
- 合成注入无法复现全部真实时序问题；最终验收以用户真实点击为准，日志能定位每次点击落点
