import { useEffect, useState } from 'react';
import dayjs from 'dayjs';
import 'dayjs/locale/zh-cn';
import { Solar } from 'lunar-typescript';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { fetchWeatherText } from '../http/weather';

dayjs.locale('zh-cn');

/** 覆盖层外观（后端实采任务栏底色 + 对比前景色，hex）。 */
interface ClockOverlayAppearance {
  bg: string;
  fg: string;
}

/**
 * 任务栏时钟覆盖层视图（`index.html?window=clock_overlay`）。
 *
 * 覆盖式接管 Phase 0：由后端把窗口贴合到系统时钟矩形，这里渲染
 * 「时间 + 星期」与「农历 + 天气」两行文字。背景必须不透明（后端
 * 实采任务栏底色，绝不允许透明——透底会叠出原生时钟文字）；
 * 悬停落在覆盖层上，系统时钟 tooltip 不再触发；点击仍由钩子处理
 * （Phase 2 才交接输入），右键默认菜单兜底拦截。
 */
function ClockOverlayWindow(): React.JSX.Element {
  const [now, setNow] = useState(() => dayjs());
  const [weather, setWeather] = useState<string>('');
  const [isDark, setIsDark] = useState(
    () => window.matchMedia('(prefers-color-scheme: dark)').matches,
  );
  // 等待后端配色到达前的过渡色也必须不透明（按系统深浅取近似任务栏色）
  const [appearance, setAppearance] = useState<ClockOverlayAppearance | null>(null);

  // 秒级刷新时间与农历；系统深浅主题切换时重新拉取覆盖层配色（任务栏底色随主题变）
  useEffect(() => {
    const timer = setInterval(() => setNow(dayjs()), 1000);
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const loadAppearance = () => {
      invoke<ClockOverlayAppearance>('clock_overlay_appearance')
        .then(setAppearance)
        .catch(() => {});
    };
    const onChange = (event: MediaQueryListEvent) => {
      setIsDark(event.matches);
      loadAppearance();
    };
    loadAppearance();
    media.addEventListener('change', onChange);
    // R7.5：后端在遮盖展开/收缩后重采样，色变时推送（任务栏底色随亚克力
    // /壁纸动态变化，静态采样会留色差——遮盖边界全程可见）
    const unlisten = listen<ClockOverlayAppearance>('clock-appearance', (event) => {
      setAppearance(event.payload);
    });
    return () => {
      clearInterval(timer);
      media.removeEventListener('change', onChange);
      void unlisten.then((fn) => fn());
    };
  }, []);

  // 合成保活：窗口被移出屏幕（全屏/任务栏收起）期间 DWM 会丢弃其表面，
  // Chromium 只在有新绘制帧时产出缓冲——若只靠 1s 的时钟 tick，移回屏幕后要等
  // 下一个 tick（实测 100~400ms 空窗透出原生时钟）。200ms 一次强制重绘保持
  // 表面常新。注意：纯 React 状态更新若不改变绘制结果不会产生新帧（实测无效），
  // 必须真的改动像素——1px 点在两个几乎相同的颜色间切换（不可感知）。
  const [keepaliveTick, setKeepaliveTick] = useState(0);
  useEffect(() => {
    const keepalive = setInterval(() => setKeepaliveTick((t) => t + 1), 200);
    return () => clearInterval(keepalive);
  }, []);
  const keepaliveColor = keepaliveTick % 2 === 0 ? 'rgba(0,0,0,0.004)' : 'rgba(0,0,0,0.008)';

  // 天气：初始拉取一次，之后每 30 分钟刷新
  useEffect(() => {
    let disposed = false;
    const load = async () => {
      const text = await fetchWeatherText();
      if (!disposed) setWeather(text);
    };
    void load();
    const interval = setInterval(() => void load(), 30 * 60 * 1000);
    return () => {
      disposed = true;
      clearInterval(interval);
    };
  }, []);

  const lunar = Solar.fromDate(now.toDate()).getLunar();
  const lunarText = `${lunar.getMonthInChinese()}月${lunar.getDayInChinese()}`;
  const timeText = now.format('HH:mm');
  const weekText = now.format('dddd');

  const color = appearance?.fg ?? (isDark ? '#ffffff' : '#1a1a1a');
  const backgroundColor = appearance?.bg ?? (isDark ? '#202020' : '#f3f3f3');
  const fontFamily =
    "'Microsoft YaHei UI','Microsoft YaHei','Segoe UI',system-ui,sans-serif";

  return (
    <div
      onContextMenu={(e) => e.preventDefault()}
      style={{
        height: '100vh',
        width: '100vw',
        boxSizing: 'border-box',
        backgroundColor,
        display: 'flex',
        flexDirection: 'column',
        justifyContent: 'center',
        alignItems: 'flex-end',
        paddingRight: 10,
        overflow: 'hidden',
        userSelect: 'none',
        cursor: 'default',
      }}
    >
      <div style={{ fontSize: 12, lineHeight: '17px', color, fontFamily, fontWeight: 600 }}>
        {timeText} {weekText}
      </div>
      <div style={{ fontSize: 12, lineHeight: '17px', color, fontFamily, whiteSpace: 'nowrap' }}>
        {lunarText}
        {weather ? `  ${weather}` : ''}
        {/* 合成保活像素：颜色微变强制绘制失效（见上） */}
        <span
          style={{ display: 'inline-block', width: 1, height: 1, backgroundColor: keepaliveColor }}
        />
      </div>
    </div>
  );
}

export default ClockOverlayWindow;
