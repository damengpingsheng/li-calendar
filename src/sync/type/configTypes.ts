import type { TrayClockDateFormat, TrayClockTimeFormat } from '../../enums/trayClockEnum.ts';

export type MacosVibrancyEffect =
  | 'blur'
  | 'acrylic'
  | 'popover'
  | 'sidebar'
  | 'mica'
  | 'mica-dark'
  | 'mica-light'
  | 'hud-window'
  | 'tabbed'
  | 'tabbed-dark'
  | 'tabbed-light'
  | 'header-view'
  | 'vibrancy'
  | 'liquid-glass'
  | 'under-window-background';

export type FrontendWindowEffect = 'transparent';
export type CalendarTheme = 'light' | 'dark';

export type MacosTrayDateIconStyle = 'filled' | 'outlined';

/**
 * 菜单栏主图标类型：`date` = 日期数字（DateIconView），`calendar` = SF Symbol「日历」（与 LunarBar createCalendarIcon 一致）。
 */
export type MacosTrayBarIconKind = 'date' | 'calendar';

export interface ConfigItem
  extends SystemConfig,
    CalendarFooterVisible,
    ConfigWindows,
    ConfigMacos {}

export interface SystemConfig {
  // 开机自启动
  autostart: boolean;
  theme: CalendarTheme;
  /** 日历小窗是否固定在最前（与顶栏图钉一致，持久化） */
  calendarPinned: boolean;
}

export interface CalendarFooterVisible {
  /** 显示底部信息区域 */
  calendarFooterVisible: boolean;
  /** 显示节日信息 */
  footerFestivalVisible: boolean;
  /** 显示宜忌信息 */
  footerYiJiVisible: boolean;
  /** 显示倒计时信息 */
  footerCountdownVisible: boolean;
}

/** 注入式时钟段样式（S 阶段；时间段=原生样式不支持自定义，S0 定案） */
export interface ClockbarStyle {
  /** 显示顺序（五元素 id 各恰一次，含 time） */
  order: string[];
  /** 数据段显示开关（time 恒显） */
  show: Record<string, boolean>;
  /** 自定义颜色（"#rrggbb"；缺省=跟随主题） */
  colors: Record<string, string>;
  /** 段字号倍率（0.5~2.0，缺省 1.0） */
  sizes: Record<string, number>;
  /** 段行归属（1=时间行，2=日期行；缺省 weather/festival/term=1，lunar=2） */
  rows: Record<string, number>;
  /** 段间距 px（0~40，缺省 10；时间行） */
  gap: number;
  /** 日期行段间距 px（0~40，缺省 10；与时间行分开调节） */
  gap2: number;
  /** 两行垂直间距 px（0~20，缺省 0） */
  vgap: number;
  /** 时间行水平对齐（0=靠左 1=居中 2=靠右） */
  halignTime: 0 | 1 | 2;
  /** 日期行水平对齐（0=靠左 1=居中 2=靠右） */
  halignDate: 0 | 1 | 2;
  /** 天气段城区名（手动配置，拼在天气段最前；空=自动跟随 adcode 对应城区名） */
  weatherCity: string;
  /** 高德 adcode（留空=IP 自动定位，直辖市只到市级；填 6 位=锁定区县级） */
  weatherAdcode: string;
  /** 天气段 emoji 图标开关（缺省开） */
  weatherEmoji: boolean;
  /** emoji 变体（true=彩色 U+FE0F 缺省 / false=黑白 U+FE0E；实测仅部分字形真黑白） */
  weatherEmojiColor: boolean;
  /** 天气现象文字开关（缺省开） */
  weatherText: boolean;
  /** 风向风级开关（高德数据源；缺省开） */
  weatherWind: boolean;
}

export interface ConfigWindows extends WindowsDesktop, WindowsTaskbar {
  /** 启用桌面组件 */
  desktopWidgetEnabled: boolean;
  /** 启用任务栏弹窗组件 */
  taskbarWidgetEnabled: boolean;
  /** 注入式任务栏时钟（与旧覆盖层互斥，默认开启；关闭时仅原生时钟） */
  clockbarInjectionEnabled: boolean;
  /** 注入式时钟段样式（缺省=现行为） */
  clockbarStyle: ClockbarStyle;
}

export interface WindowsDesktop {
  /** 桌面窗口位置 */
  desktopWindowPosition: { x: number; y: number } | null;
}
export interface WindowsTaskbar {
  /** 自定义托盘时钟 */
  customTrayClockEnabled: boolean;
  /** 时间格式 */
  timeFormat: TrayClockTimeFormat;
  /** 日期格式 */
  dateFormat: TrayClockDateFormat;
}

export interface ConfigMacos {
  /** macOS 半透明 */
  isWindowsEffect: boolean;
  /** macOS 半透明效果 */
  macosEffect: MacosVibrancyEffect;
  frontendWindowEffectEnabled: boolean;
  frontendWindowEffect: FrontendWindowEffect;
  /** 纯前端效果下窗口背景的透明度 0–100，数值越大越透明 */
  frontendWindowTransparency: number;
  /** macOS 菜单栏图标文案模板 */
  macosTrayTitleTemplate: string;
  /** 菜单栏主图标：`date` = 日期数字，`calendar` = SF Symbol 日历图标（LunarBar 同款） */
  macosTrayBarIcon: MacosTrayBarIconKind;
  /**
   * macOS 菜单栏日期图标样式（与 LunarBar 一致：实心 = 整面填充 + 数字镂空，描边 = 圆角框 + 数字）
   */
  macosTrayDateIconStyle: MacosTrayDateIconStyle;
  /** 菜单栏日期图标位图宽度（像素），后端限制 16–128，默认 42（与 LunarBar 对齐的 21×18 @2× 画布之宽） */
  macosTrayIconWidth: number;
  /** 菜单栏日期图标位图高度（像素），默认 36（21×18 @2×，与 tray 约 18pt 槽 + LunarBar 15pt 图高一致） */
  macosTrayIconHeight: number;
}
