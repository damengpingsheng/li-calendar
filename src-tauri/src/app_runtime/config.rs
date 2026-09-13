use serde::{Deserialize, Serialize};
use std::fs;
use tauri::{AppHandle, Manager};

/// 桌面组件窗口的持久化位置（物理像素坐标）。
#[cfg(windows)]
#[derive(Default, Deserialize, Clone)]
pub struct PersistedPosition {
    pub x: i32,
    pub y: i32,
}

/// 注入式任务栏时钟样式（S 阶段，S0 定案：时间段=原生样式不支持自定义）。
/// 下发链路：liConfig.json → 启动载入/设置命令更新 → data.rs 组进 c1set 的
/// `style` 对象（host 侧先钳制校验，tap 侧仍自校验——双端防御）。
#[cfg(windows)]
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockbarStyleConfig {
    /// 显示顺序（五元素 id 各恰一次；缺省=weather,festival,term,lunar,time）。
    #[serde(default)]
    pub order: Vec<String>,
    /// 数据段显示开关（time 恒显；缺省全开）。
    #[serde(default)]
    pub show: std::collections::BTreeMap<String, bool>,
    /// 自定义颜色（"#rrggbb"；缺省=跟随主题，"theme" 同义）。
    #[serde(default)]
    pub colors: std::collections::BTreeMap<String, String>,
    /// 段字号倍率（0.5~2.0，作用于 fontscale 之上；缺省 1.0）。
    #[serde(default)]
    pub sizes: std::collections::BTreeMap<String, f64>,
    /// 段间距 px（0~40，缺省 10）。
    #[serde(default)]
    pub gap: Option<f64>,
}

/// 自 `liConfig.json` 反序列化的功能开关快照。
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedFeatureConfig {
    /// 是否启用桌面日历小组件（仅 Windows）。
    #[cfg(windows)]
    pub desktop_widget_enabled: Option<bool>,
    /// 是否启用任务栏日历替换/拦截（仅 Windows）。
    #[cfg(windows)]
    pub taskbar_widget_enabled: Option<bool>,
    /// 是否启用注入式任务栏时钟（仅 Windows；用户决策 #4 默认开启，关闭时仅原生时钟）。
    #[cfg(windows)]
    pub clockbar_injection_enabled: Option<bool>,
    /// 注入式任务栏时钟样式（仅 Windows；S 阶段，缺省=现行为）。
    #[cfg(windows)]
    pub clockbar_style: Option<ClockbarStyleConfig>,
    /// 桌面日历小组件上次保存的物理像素位置（仅 Windows）。
    #[cfg(windows)]
    pub desktop_window_position: Option<PersistedPosition>,
    /// 菜单栏标题模板字符串（仅 macOS）。
    #[cfg(target_os = "macos")]
    pub macos_tray_title_template: Option<String>,
    /// 菜单栏日期图标样式配置键（仅 macOS）。
    #[cfg(target_os = "macos")]
    pub macos_tray_date_icon_style: Option<String>,
    /// 托盘图标宽度像素（仅 macOS）。
    #[cfg(target_os = "macos")]
    pub macos_tray_icon_width: Option<u32>,
    /// 托盘图标高度像素（仅 macOS）。
    #[cfg(target_os = "macos")]
    pub macos_tray_icon_height: Option<u32>,
    /// 菜单栏主图标类型：`date` / `calendar`（仅 macOS）。
    #[cfg(target_os = "macos")]
    pub macos_tray_bar_icon: Option<String>,
}

/// 从应用配置目录加载持久化功能开关，读取失败时返回默认配置。
pub fn load_persisted_feature_config(app_handle: &AppHandle) -> PersistedFeatureConfig {
    let Some(config_dir) = app_handle.path().app_config_dir().ok() else {
        return PersistedFeatureConfig::default();
    };
    let config_path = config_dir.join("liConfig.json");
    let Ok(content) = fs::read_to_string(config_path) else {
        return PersistedFeatureConfig::default();
    };
    serde_json::from_str::<PersistedFeatureConfig>(&content).unwrap_or_default()
}
