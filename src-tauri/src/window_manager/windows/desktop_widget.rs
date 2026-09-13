//! 桌面日历窗口：普通置底顶层窗口、被动显示与刷新。
//!
//! 历史：曾用 `SetWindowLongPtrW(GWLP_HWNDPARENT, Progman)` 把窗口挂为 Progman
//! 的 owned window。跨进程的 owner 关系会让系统**隐式合并两个线程的输入队列**
//! （等价 AttachThreadInput），导致本进程与 explorer 桌面线程共享队列状态；
//! 交互/退出时队列残留坏状态，表现为桌面左键点选失灵而任务栏正常、右键一次
//! 自愈。已改为普通置底顶层窗口（HWND_BOTTOM，壁纸之上、应用之下），不再与
//! explorer 建立任何跨进程窗口关系。
use super::{get_window_hwnd, CalendarWindowManager};
use crate::window_manager::shared::popup_manager::PopupManager;
use tauri::WebviewWindow;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetForegroundWindow, GetWindow, GetWindowLongPtrW, IsIconic,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_STYLE, GW_HWNDLAST,
    GWLP_HWNDPARENT, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WS_CHILD,
};

/// 把窗口压到非置顶层最底部（壁纸/桌面之上、其他应用窗口之下）。
/// 仅当尚不处于最底时才调用 `SetWindowPos`，避免每次点击都产生层级抖动。
fn pin_window_to_bottom(window_hwnd: HWND) {
    // 诊断开关：存在标记文件时恢复旧的 Progman 跨进程挂接（层 1 变量隔离实验用）——
    // 跨进程 owner 会隐式合并两线程输入队列（等价 AttachThreadInput）。
    if std::path::Path::new(r"D:\agents_tmp\pin_progman").exists() {
        unsafe {
            if let Ok(progman) = FindWindowW(w!("Progman"), None) {
                if !progman.0.is_null() {
                    SetWindowLongPtrW(window_hwnd, GWLP_HWNDPARENT, progman.0 as isize);
                    let _ = SetWindowPos(
                        window_hwnd,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                    );
                    println!("⚠️ 诊断：桌面日历已挂接 Progman（pin_progman 开关）");
                    return;
                }
            }
        }
    }
    unsafe {
        // 已是最底层（前面没有同带窗口）则跳过。
        let last = GetWindow(window_hwnd, GW_HWNDLAST).unwrap_or_default();
        if last.0 == window_hwnd.0 {
            return;
        }
        // 已被设为其他窗口的子窗口时不要盲目置底（异常状态，交由重建处理）。
        if GetWindowLongPtrW(window_hwnd, GWL_STYLE) & WS_CHILD.0 as isize != 0 {
            println!("⚠️ 桌面日历窗口处于子窗口状态，跳过置底");
            return;
        }
        let _ = SetWindowPos(
            window_hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        );
    }
}

impl CalendarWindowManager {
    fn reapply_saved_desktop_vibrancy(&self, window: &WebviewWindow) {
        if self.desktop_vibrancy_enabled {
            let _ = Self::apply_desktop_window_vibrancy(
                window,
                self.desktop_vibrancy_effect.as_deref(),
            );
        } else {
            Self::clear_window_vibrancy(window);
        }
    }

    /// 初始化桌面组件的窗口层级与显示状态。
    /// `initial_position` 为物理像素坐标，在 show() 前先定好位，避免位置跳变。
    pub fn refresh_desktop_widget_chrome(
        window: &WebviewWindow,
        initial_position: Option<(i32, i32)>,
    ) {
        let hwnd = get_window_hwnd(window);

        // 若有持久化位置，在 show() 前用 Win32 直接移动到目标物理坐标，
        // 这样 Tauri show() 触发时窗口已在正确位置，彻底消除闪烁。
        if let (Some(hwnd), Some((x, y))) = (hwnd, initial_position) {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
            }
        }

        // 用 Tauri show() 激活 WebView2 合成管线，立即用 SW_SHOWNOACTIVATE 压制激活，
        // 再归还焦点给之前的前台窗口，避免任务栏弹窗因失焦而自动隐藏。
        let prev_foreground = unsafe { GetForegroundWindow() };
        let _ = window.show();
        unsafe {
            if let Some(hwnd) = get_window_hwnd(window) {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                // 置底到非置顶层最底（壁纸之上、应用之下），替代旧的 Progman 挂接。
                pin_window_to_bottom(hwnd);
            }
            if !prev_foreground.0.is_null() {
                let _ = SetForegroundWindow(prev_foreground);
            }
        }
    }

    /// 构建桌面组件 WebviewWindow，**不需要持有 `window_manager` 锁**，可在锁外并发调用。
    /// 构建完成后调用 `attach_desktop_window` 将其存入 manager。
    pub fn build_desktop_window(
        app_handle: &tauri::AppHandle,
        initial_position: Option<(i32, i32)>,
    ) -> Result<tauri::WebviewWindow, Box<dyn std::error::Error>> {
        let mut builder = tauri::WebviewWindowBuilder::new(
            app_handle,
            "desktop_calendar",
            tauri::WebviewUrl::App("index.html?window=desktop".into()),
        );
        // WebView2 用户数据固定到 D 盘（webview-data/desktop），不再写 C 盘 LocalAppData。
        builder = builder.data_directory(std::path::PathBuf::from(
            r"D:\Program Files\li-calendar\webview-data\desktop",
        ));
        let window = builder
        .title("桌面日历")
        .inner_size(360.0, 520.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(false)
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .build()?;
        Self::refresh_desktop_widget_chrome(&window, initial_position);
        Ok(window)
    }

    /// 将已构建好的桌面组件窗口存入 manager 并应用毛玻璃效果。
    pub fn attach_desktop_window(&mut self, window: tauri::WebviewWindow) {
        self.reapply_saved_desktop_vibrancy(&window);
        self.desktop_widget_window = Some(window);
    }

    /// 确保桌面组件窗口存在，不存在时创建并完成桌面层绑定（持锁版，供单线程路径使用）。
    pub fn ensure_desktop_window(
        &mut self,
        initial_position: Option<(i32, i32)>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.desktop_widget_window.is_some() {
            return Ok(());
        }
        let window = Self::build_desktop_window(&self.app_handle.clone(), initial_position)?;
        self.attach_desktop_window(window);
        Ok(())
    }

    /// 关闭并释放桌面组件窗口句柄。
    pub fn close_desktop_window(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(desktop_widget_window) = self.desktop_widget_window.take() {
            desktop_widget_window.close()?;
        }
        Ok(())
    }

    /// 任务栏时钟左键点击时只保留一个日历：优先切换桌面日历的显示/隐藏，
    /// 若任务栏弹窗仍在显示则先隐藏它，避免同时出现两个月历。
    /// 桌面组件未启用（无窗口）时回退为原有的任务栏弹窗切换。
    ///
    /// 可见性完全以 `desktop_widget_visible` 状态位为准：启动阶段窗口的
    /// `is_visible()` 可能返回与实际不符的值（曾表现为首次点击时钟不隐藏）。
    pub fn toggle_clock_calendar(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(desktop_window) = self.desktop_widget_window.clone() {
            if let Some(popup_window) = &self.taskbar_popup_window {
                if popup_window.is_visible().unwrap_or(false) {
                    let _ = popup_window.hide();
                }
            }
            if self.desktop_widget_visible {
                self.desktop_widget_visible = false;
                desktop_window.hide()?;
            } else {
                self.desktop_widget_visible = true;
                self.refresh_desktop_window_visibility();
            }
            return Ok(());
        }
        self.toggle_popup()
    }

    /// 重新显示并置底桌面组件窗口（用于 WIN+D 或显示桌面后的恢复）。
    /// 用户主动隐藏（`desktop_widget_visible == false`）时不恢复。
    /// 调用 `show()` 后立即用 SW_SHOWNOACTIVATE 压制激活，防止失焦导致窗口自动隐藏。
    pub fn refresh_desktop_window_visibility(&mut self) {
        if !self.desktop_widget_visible {
            return;
        }
        if let Some(ref window) = self.desktop_widget_window {
            let prev_foreground = unsafe { GetForegroundWindow() };
            let _ = window.show();
            unsafe {
                if let Some(hwnd) = get_window_hwnd(window) {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    pin_window_to_bottom(hwnd);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOACTIVATE
                            | SWP_NOOWNERZORDER
                            | SWP_SHOWWINDOW,
                    );
                }
                if !prev_foreground.0.is_null() {
                    let _ = SetForegroundWindow(prev_foreground);
                }
            }
        }
    }

    /// 仅当窗口被最小化（如 WIN+D）时才恢复显示与置底；可见时零开销返回。
    /// 供钩子在任意桌面左键点击时调用（任何点击都可能发生在 WIN+D 之后）。
    pub fn ensure_desktop_widget_on_desktop(&mut self) {
        if !self.desktop_widget_visible {
            return;
        }
        let Some(ref window) = self.desktop_widget_window else {
            return;
        };
        let iconic = get_window_hwnd(window)
            .map(|hwnd| unsafe { IsIconic(hwnd).as_bool() })
            .unwrap_or(false);
        if iconic {
            println!("桌面日历窗口处于最小化（WIN+D），恢复显示");
            self.refresh_desktop_window_visibility();
        }
    }
}
