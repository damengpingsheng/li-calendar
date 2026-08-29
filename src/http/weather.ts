/**
 * 天气数据：使用中国气象网（weather.com.cn）免费接口，国内可直连、无需 key。
 *
 * 两个接口都是「JS 赋值脚本」风格（无 CORS 限制），因此用动态 <script> 注入读取全局变量：
 *  - IP 定位：https://wgeo.weather.com.cn/ip/  ->  window.cityid
 *  - 实况：   https://www.weather.com.cn/data/sk/{cityid}.html -> window.dataSK.weatherinfo
 *
 * cityid 缓存到 localStorage，减少请求；实况文本内存缓存 30 分钟。
 */
interface WeatherInfo {
  temp?: string;
  weather?: string;
  wind?: string;
}

let weatherTextCache: { text: string; ts: number } | null = null;

/** 动态注入 script，跨源读取（无 CORS）后端返回的 JS 变量。 */
function loadScript(url: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const script = document.createElement('script');
    script.src = url;
    script.onload = () => {
      script.remove();
      resolve();
    };
    script.onerror = () => {
      script.remove();
      reject(new Error(`天气接口加载失败: ${url}`));
    };
    document.head.appendChild(script);
  });
}

function readCityId(): string {
  const cityId = (window as unknown as { cityid?: string }).cityid;
  if (cityId) {
    localStorage.setItem('lical_cityid', cityId);
    return cityId;
  }
  return localStorage.getItem('lical_cityid') ?? '';
}

/** 用 IP 自动定位城市码（成功后缓存），返回 cityid。 */
async function ensureCityId(): Promise<string> {
  const cached = localStorage.getItem('lical_cityid');
  if (cached) {
    return cached;
  }
  try {
    await loadScript(`https://wgeo.weather.com.cn/ip/?_=${Date.now()}`);
  } catch {
    return '';
  }
  return readCityId();
}

/** 拉取指定城市实时天气文本（如「26℃ 晴」）。 */
async function fetchWeather(cityId: string): Promise<WeatherInfo> {
  try {
    await loadScript(
      `https://www.weather.com.cn/data/sk/${cityId}.html?ts=${Date.now()}`,
    );
    const info = (window as unknown as { dataSK?: { weatherinfo: WeatherInfo } }).dataSK
      ?.weatherinfo;
    return info ?? {};
  } catch {
    return {};
  }
}

/** 综合定位 + 实况，带 30 分钟缓存；失败返回空串（界面不显示天气）。 */
export async function fetchWeatherText(): Promise<string> {
  if (weatherTextCache && Date.now() - weatherTextCache.ts < 30 * 60 * 1000) {
    return weatherTextCache.text;
  }
  try {
    const cityId = await ensureCityId();
    if (!cityId) {
      return '';
    }
    const info = await fetchWeather(cityId);
    const text = [info.temp ? `${info.temp}℃` : '', info.weather ?? '']
      .filter(Boolean)
      .join(' ');
    weatherTextCache = { text, ts: Date.now() };
    return text;
  } catch {
    return '';
  }
}