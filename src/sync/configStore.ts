import { trayClockDateFormats, trayClockTimeFormats } from '../enums/trayClockEnum.ts';
import { createSync } from './base/crossWindowSync.ts';
import type {
  CalendarFooterVisible,
  ConfigItem,
  ConfigMacos,
  ConfigWindows,
  SystemConfig,
} from './type/configTypes.ts';

/** `satisfies`：字面量须符合对应类型，写错字符串会在编译期报错，无需 `as` 断言 */
const systemConfigDefaults = {
  autostart: false,
  theme: 'light',
  calendarPinned: false,
} satisfies SystemConfig;

const calendarFooterVisibleDefaults = {
  calendarFooterVisible: true,
  footerFestivalVisible: true,
  footerYiJiVisible: false,
  footerCountdownVisible: true,
} satisfies CalendarFooterVisible;

const configWindowsDefaults = {
  desktopWidgetEnabled: true,
  taskbarWidgetEnabled: true,
  /** 注入式任务栏时钟（用户决策 #4：默认开启） */
  clockbarInjectionEnabled: true,
  /** 注入式时钟段样式（D4 现行为=缺省；v55 农历默认在日期行；v60 天气段增强缺省=图标彩色+文字+无城区名，城区名默认取 IP 定位一致的「昌平」） */
  clockbarStyle: {
    order: ['weather', 'festival', 'term', 'lunar', 'time'],
    show: { weather: true, festival: true, term: true, lunar: true, time: true },
    colors: {},
    sizes: {},
    rows: { weather: 1, festival: 1, term: 1, lunar: 2 },
    gap: 10,
    gap2: 10,
    vgap: 0,
    halignTime: 0,
    halignDate: 0,
    weatherCity: '',
    weatherAdcode: '',
    weatherEmoji: true,
    weatherEmojiColor: true,
    weatherText: true,
    weatherWind: true,
  },
  desktopWindowPosition: null,
  customTrayClockEnabled: true,
  timeFormat: trayClockTimeFormats.HhMm,
  dateFormat: trayClockDateFormats.DddYmd,
} satisfies ConfigWindows;

const configMacosDefaults = {
  isWindowsEffect: false,
  macosEffect: 'vibrancy',
  frontendWindowEffectEnabled: false,
  frontendWindowEffect: 'transparent',
  frontendWindowTransparency: 20,
  macosTrayTitleTemplate: '',
  macosTrayBarIcon: 'date',
  macosTrayDateIconStyle: 'filled',
  macosTrayIconWidth: 42,
  macosTrayIconHeight: 36,
} satisfies ConfigMacos;

const configDefaults: ConfigItem = {
  ...systemConfigDefaults,
  ...calendarFooterVisibleDefaults,
  ...configWindowsDefaults,
  ...configMacosDefaults,
};

export const useConfigSync = createSync<ConfigItem>('liConfig', configDefaults);
