import { useEffect, useState } from 'react';
import dayjs from 'dayjs';
import 'dayjs/locale/zh-cn';
import { Solar } from 'lunar-typescript';
import { fetchWeatherText } from '../http/weather';

dayjs.locale('zh-cn');

/**
 * 任务栏时钟覆盖层视图（`index.html?window=clock_overlay`）。
 *
 * 由后端 `clock_overlay` 窗口贴合到系统时钟矩形、鼠标穿透，这里渲染
 * 「时间 + 星期」与「农历 + 天气」两行文字，替代系统时钟显示。
 */
function ClockOverlayWindow(): React.JSX.Element {
  const [now, setNow] = useState(() => dayjs());
  const [weather, setWeather] = useState<string>('');
  const [isDark, setIsDark] = useState(
    () => window.matchMedia('(prefers-color-scheme: dark)').matches,
  );

  // 秒级刷新时间与农历，并监听系统深浅主题（文字颜色自适应任务栏底色）
  useEffect(() => {
    const timer = setInterval(() => setNow(dayjs()), 1000);
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = (event: MediaQueryListEvent) => setIsDark(event.matches);
    media.addEventListener('change', onChange);
    return () => {
      clearInterval(timer);
      media.removeEventListener('change', onChange);
    };
  }, []);

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

  const color = isDark ? '#ffffff' : '#1a1a1a';
  const fontFamily =
    "'Microsoft YaHei UI','Microsoft YaHei','Segoe UI',system-ui,sans-serif";

  return (
    <div
      style={{
        height: '100vh',
        boxSizing: 'border-box',
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
      </div>
    </div>
  );
}

export default ClockOverlayWindow;