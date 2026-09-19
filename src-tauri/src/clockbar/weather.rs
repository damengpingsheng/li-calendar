// D 阶段天气：weather.com.cn 实况的 Rust 新实现（方案 §3 决策表 D5 / §3 D3 定案）。
// std-only：TcpStream 纯 HTTP（无需 TLS——端点实测均支持 http 直连）。
// 自探针 crate src/weather.rs 移植（E0），差异仅城市缓存路径（生产=安装目录）。
//
// 端点定案（2026-09-13 D 阶段实测）：
// - www.weather.com.cn/data/sk/{id}.html 已 301 死链——前端旧实现已静默失效；
// - 实况：http://d1.weather.com.cn/weather_index/{id}.html?date=...
//   （必须带 Referer: http://www.weather.com.cn/）。中文编码勘误（2026-09-19
//   od 字节级实测）：正文实为 UTF-8（cityname/WD/WS/weather 全部 UTF-8 字节），
//   D 阶段「GBK 混杂」是控制台按 GBK 误读 UTF-8 所致——v65 起直接取 dataSK
//   的 cityname/WD/WS 中文原值（降级源补城区名+风向），不再绕道规避；
// - IP 定位：http://wgeo.weather.com.cn/ip/?_=<ts>（须带 Referer），
//   返回 `var ip="...";var id="101010700";var addr=...`（只取 ASCII 的 id）。
//
// 降级纪律（方案 §5 D 行 / §6）：断网/超时/坏数据一律返回 Err，由调用方把天气段
// 降级为占位符（"--"），绝不影响其余段与 explorer。天气获取在数据线程内阻塞
// （≤8s）不触 UI 线程/管道协议。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 实况数据（temp 原样字符串，text=天气名，code=weathercode 整数——emoji 码表用，
/// wind=风向风级展示串如「东北风3~4级」，city_auto=响应自带城区名「昌平区」剥后缀）
pub struct WeatherNow {
    pub temp: String,
    pub text: String,
    /// weathercode 整数（"d13"/"n7" 去昼夜前缀后的数值；解析失败=u32::MAX→无 emoji）
    pub code: u32,
    /// 风向风级展示串（高德 lives[0] / 天气网 dataSK 的 WD+WS，两源皆有）
    pub wind: String,
    /// 城区名自动跟随（高德 lives[0].city / 天气网 dataSK.cityname 剥「市/区/县」
    /// 后缀；liConfig weatherCity 非空时被手动值覆盖）
    pub city_auto: String,
}

/// 天气段展示选项（T 阶段定案：城区名+emoji 图标+现象文字，全部可配）。
#[derive(Clone, Debug)]
pub struct DisplayOpts {
    /// 城区名（手动配置，拼在天气段最前；空=不拼）
    pub city: String,
    /// emoji 图标开关（缺省开）
    pub emoji: bool,
    /// emoji 变体：true=彩色 U+FE0F（缺省）/ false=黑白 U+FE0E
    /// （2026-09-16 任务栏实测：仅 ☁ 等有文本字形者真黑白，无文本字形回落彩色）
    pub emoji_color: bool,
    /// 天气现象文字开关（缺省开；关=只显图标+温度）
    pub show_text: bool,
    /// 风向风级开关（缺省开；仅高德源有数据，关=不拼「东北风3~4级」）
    pub show_wind: bool,
}

impl Default for DisplayOpts {
    fn default() -> Self {
        DisplayOpts {
            city: String::new(),
            emoji: true,
            emoji_color: true,
            show_text: true,
            show_wind: true,
        }
    }
}

impl DisplayOpts {
    /// 从 liConfig `clockbarStyle` 快照构建（字段缺省=图标彩色开、文字开、无城区名）。
    /// 城区名做 tap 限界解析安全清洗（值内禁引号/反斜杠/控制符）并限长 16 字符。
    pub fn from_config(cfg: Option<&crate::app_runtime::config::ClockbarStyleConfig>) -> Self {
        let mut o = DisplayOpts::default();
        let Some(cfg) = cfg else { return o };
        if let Some(c) = &cfg.weather_city {
            o.city = sanitize_seg(c, 16);
        }
        if let Some(v) = cfg.weather_emoji {
            o.emoji = v;
        }
        if let Some(v) = cfg.weather_emoji_color {
            o.emoji_color = v;
        }
        if let Some(v) = cfg.weather_text {
            o.show_text = v;
        }
        if let Some(v) = cfg.weather_wind {
            o.show_wind = v;
        }
        o
    }
}

/// tap c1set 值安全清洗：剔除引号/反斜杠/控制符（tap 限界 JSON 解析不认转义），
/// 限长 max_chars 字符。城区名等用户输入拼进天气段前必过。
fn sanitize_seg(s: &str, max_chars: usize) -> String {
    s.chars()
        .filter(|c| {
            !matches!(c, '"' | '\\') && *c >= ' ' && *c != '\u{7f}'
        })
        .take(max_chars)
        .collect()
}

impl WeatherNow {
    /// 旧口径展示文案（`26℃ 晴`，无城区名/图标）——保留给探针/兼容场景。
    pub fn display(&self) -> String {
        format_now(Some(self), &DisplayOpts { emoji: false, ..DisplayOpts::default() })
    }
}

/// 天气码 → emoji 基字符（T 待办定案 8+1 类粒度；未收录=空串→不拼图标）。
/// 变体符由 format_now 按彩色/黑白开关追加（U+FE0F/U+FE0E）。
fn weather_code_emoji(num: u32) -> &'static str {
    match num {
        0 => "\u{1F31E}",                        // 晴 🌞
        1 => "\u{26C5}",                         // 多云 ⛅
        2 => "\u{2601}",                         // 阴 ☁
        3 => "\u{1F326}",                        // 阵雨 🌦
        4 | 5 => "\u{26C8}",                     // 雷阵雨/伴冰雹 ⛈
        6 | 13..=17 | 26..=28 => "\u{1F328}",    // 雪系（含雨夹雪）🌨
        7..=12 | 19 | 21..=25 => "\u{1F327}",    // 雨系（含冻雨）🌧
        18 | 53 => "\u{1F32B}",                  // 雾/霾 🌫
        20 | 29..=31 => "\u{1F32A}",             // 沙尘暴/浮尘/扬沙/强沙尘暴 🌪
        _ => "",
    }
}

/// 高德 weather 中文现象 → 码号（复用 emoji 码表；高德返回中文与 weather_code_name
/// 同名词表）。未收录 → u32::MAX（无 emoji，文字照显）。
fn amap_text_code(text: &str) -> u32 {
    match text {
        "晴" => 0,
        "多云" => 1,
        "阴" => 2,
        "阵雨" => 3,
        "雷阵雨" => 4,
        "雷阵雨并伴有冰雹" | "雷阵雨伴有冰雹" => 5,
        "雨夹雪" => 6,
        "小雨" => 7,
        "中雨" => 8,
        "大雨" => 9,
        "暴雨" => 10,
        "大暴雨" => 11,
        "特大暴雨" => 12,
        "阵雪" => 13,
        "小雪" => 14,
        "中雪" => 15,
        "大雪" => 16,
        "暴雪" => 17,
        "雾" => 18,
        "冻雨" => 19,
        "沙尘暴" => 20,
        "小雨-中雨" => 21,
        "中雨-大雨" => 22,
        "大雨-暴雨" => 23,
        "暴雨-大暴雨" => 24,
        "大暴雨-特大暴雨" => 25,
        "小雪-中雪" => 26,
        "中雪-大雪" => 27,
        "大雪-暴雪" => 28,
        "浮尘" => 29,
        "扬沙" => 30,
        "强沙尘暴" => 31,
        "霾" => 53,
        _ => u32::MAX,
    }
}

/// 风级口径归一：高德 windpower 形如 "≤3"/"3~4"/"3-4"；"≤3" 归一为 "1~3"，
/// 其余把 "-" 归一为 "~"；拼展示串「东北风1~3级」（无风向则只显级数）。
fn format_wind(dir: &str, power: &str) -> String {
    let d = dir.trim();
    let p = power.trim().replace("≤3", "1~3").replace('-', "~");
    if p.is_empty() {
        return String::new();
    }
    if d.is_empty() || d == "无风向" {
        format!("{p}级")
    } else {
        format!("{d}风{p}级")
    }
}

/// 任务栏天气段展示文案（T 阶段）：`[城区名 ][emoji ]温度℃[ 现象]`。
/// 从未成功获取（w=None）恒为占位符 "--"（降级态保持最小，不拼图标/城区名）。
/// 每次天气刷新后由数据线程按当前配置重算——配置变更随下一 tick 上屏（≤1s）。
pub fn format_now(w: Option<&WeatherNow>, o: &DisplayOpts) -> String {
    let Some(w) = w else {
        return "--".to_string();
    };
    let t = w.temp.trim();
    if t.is_empty() {
        return "--".to_string();
    }
    let mut s = String::new();
    let city = if o.city.is_empty() { w.city_auto.trim() } else { o.city.trim() };
    if !city.is_empty() {
        s.push_str(city);
        s.push(' ');
    }
    if o.emoji {
        let e = weather_code_emoji(w.code);
        if !e.is_empty() {
            s.push_str(e);
            s.push(if o.emoji_color { '\u{FE0F}' } else { '\u{FE0E}' });
            s.push(' ');
        }
    }
    s.push_str(t);
    s.push('℃');
    if o.show_text {
        let txt = w.text.trim();
        if !txt.is_empty() {
            s.push(' ');
            s.push_str(txt);
        }
    }
    if o.show_wind {
        let wind = w.wind.trim();
        if !wind.is_empty() {
            s.push(' ');
            s.push_str(wind);
        }
    }
    sanitize_seg(&s, 60)
}

/// 天气代码表（weather.com.cn 通用码表；d1 接口 weathercode 形如 d00/n13，
/// 按整数解析以免 "00"≠"0"）。未收录码返回空串→展示退化为纯温度，不视为错误。
fn weather_code_name(num: u32) -> &'static str {
    match num {
        0 => "晴",
        1 => "多云",
        2 => "阴",
        3 => "阵雨",
        4 => "雷阵雨",
        5 => "雷阵雨伴有冰雹",
        6 => "雨夹雪",
        7 => "小雨",
        8 => "中雨",
        9 => "大雨",
        10 => "暴雨",
        11 => "大暴雨",
        12 => "特大暴雨",
        13 => "阵雪",
        14 => "小雪",
        15 => "中雪",
        16 => "大雪",
        17 => "暴雪",
        18 => "雾",
        19 => "冻雨",
        20 => "沙尘暴",
        21 => "小雨-中雨",
        22 => "中雨-大雨",
        23 => "大雨-暴雨",
        24 => "暴雨-大暴雨",
        25 => "大暴雨-特大暴雨",
        26 => "小雪-中雪",
        27 => "中雪-大雪",
        28 => "大雪-暴雪",
        29 => "浮尘",
        30 => "扬沙",
        31 => "强沙尘暴",
        53 => "霾",
        _ => "",
    }
}

/// 从脚本正文提取 `var dataSK = {...}`（实测等号前可带空格）里的 JSON
/// （首个 '{' 到配对 '}'）。
fn extract_djson(body: &str, var: &str) -> Option<String> {
    let pat = format!("var {var}");
    let start = body.find(&pat)? + pat.len();
    let rest = &body[start..];
    let bo = rest.find('{')?;
    let mut depth = 0usize;
    for (i, c) in rest[bo..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[bo..=bo + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// 有界字符串字段提取（同 tap 侧 JGetStr 口径：只认 "key":"value"，无转义）。
fn json_str(j: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = j.find(&pat)? + pat.len();
    let rest = &j[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn http_get(url_host: &str, path: &str, referer: &str, timeout: Duration) -> Result<String, String> {
    // 测试钩子：LICAL_WX_HOST 覆盖实际连接目标（host[:port]，头字段仍用真实 Host），
    // 用于坏数据/超时/断网三路径的受控降级实测。
    let conn = match std::env::var("LICAL_WX_HOST") {
        Ok(h) if !h.is_empty() => {
            if h.contains(':') { h } else { format!("{h}:80") }
        }
        _ => format!("{url_host}:80"),
    };
    let host_header = url_host;
    use std::net::ToSocketAddrs;
    let sock = conn
        .to_socket_addrs()
        .map_err(|e| format!("dns {conn}: {e}"))?
        .next()
        .ok_or_else(|| format!("dns {conn}: no addr"))?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout)
        .map_err(|e| format!("connect {conn}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("settimeout: {e}"))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nReferer: {referer}\r\n\
         User-Agent: Mozilla/5.0 liCalendar-clockbar\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let t0 = std::time::Instant::now();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if buf.len() > 512 * 1024 {
            return Err("body too large".into());
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => {
                if buf.is_empty() {
                    return Err(format!("read: {e}"));
                }
                break; // 超时但有部分数据——够解析即用
            }
        }
        if t0.elapsed() > timeout * 2 {
            break;
        }
    }
    let body = String::from_utf8_lossy(&buf);
    // 有 chunked 编码——简单剥离：定位空行后的正文（d1 的 dataSK JSON 内无换行，可容忍）
    let split = body.find("\r\n\r\n").ok_or("no header/body split")?;
    Ok(body[split + 4..].to_string())
}

/// IP 自动定位城市码（wgeo，须带 Referer）；失败返回 Err（不缓存）。
pub fn locate_cityid() -> Result<String, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let body = http_get(
        "wgeo.weather.com.cn",
        &format!("/ip/?_={ts}"),
        "http://www.weather.com.cn/",
        Duration::from_secs(8),
    )?;
    // 期望 `var id="101010700"`（id 变量名是现状实测定案）
    let pat = "var id=\"";
    let start = body.find(pat).ok_or("wgeo: no id var")? + pat.len();
    let rest = &body[start..];
    let end = rest.find('"').ok_or("wgeo: id unterminated")?;
    let id = rest[..end].to_string();
    if id.len() != 9 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("wgeo: bad id '{id}'"));
    }
    Ok(id)
}

/// 拉取实况（weather_index 端点）。任何失败 → Err（调用方降级占位）。
pub fn fetch_now(cityid: &str) -> Result<WeatherNow, String> {
    if cityid.len() != 9 || !cityid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("bad cityid '{cityid}'"));
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let body = http_get(
        "d1.weather.com.cn",
        &format!("/weather_index/{cityid}.html?date={ts}"),
        "http://www.weather.com.cn/",
        Duration::from_secs(8),
    )?;
    let j = extract_djson(&body, "dataSK").ok_or("no dataSK var")?;
    let temp = json_str(&j, "temp").ok_or("no temp field")?;
    if temp.is_empty() {
        return Err("empty temp".into());
    }
    let code = json_str(&j, "weathercode").unwrap_or_default();
    let num: u32 = if code.len() >= 2 {
        code[1..].parse().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    let text = weather_code_name(num).to_string();
    // v65 降级补全：dataSK 也有城区名（cityname）与风向风级（WD/WS，中文 UTF-8，
    // 编码勘误见文件头）。降级到天气网源时城区名/风向不再消失；WS 带「级」后缀，
    // format_wind 期望纯级数（高德 windpower 口径），先剥掉避免「2级级」。
    let ws = json_str(&j, "WS").unwrap_or_default();
    let ws = ws.trim_end_matches('级');
    let wind = format_wind(&json_str(&j, "WD").unwrap_or_default(), ws);
    let city_auto = strip_city_suffix(&json_str(&j, "cityname").unwrap_or_default());
    Ok(WeatherNow { temp, text, code: num, wind, city_auto })
}

/// 高德天气实况（restapi.amap.com/v3/weather/weatherInfo，HTTP 直连零新依赖，
/// UTF-8 JSON：lives[0] 的 weather/temperature/winddirection/windpower/humidity）。
/// 有风向风级、湿度，是中国天气网链路的升级数据源（2026-09-16 定案）。
pub fn fetch_now_amap(key: &str, adcode: &str) -> Result<WeatherNow, String> {
    if key.len() != 32 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("bad amap key len {}", key.len()));
    }
    if adcode.len() != 6 || !adcode.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("bad amap adcode '{adcode}'"));
    }
    let body = http_get(
        "restapi.amap.com",
        &format!("/v3/weather/weatherInfo?city={adcode}&key={key}"),
        "http://restapi.amap.com/",
        Duration::from_secs(8),
    )?;
    // 限界提取（同 json_str 口径，无转义 JSON——高德返回无内嵌引号的中文字段）
    let status = json_str(&body, "status").ok_or("amap: no status")?;
    if status != "1" {
        let info = json_str(&body, "info").unwrap_or_default();
        return Err(format!("amap: status={status} info={info}"));
    }
    let temp = json_str(&body, "temperature").ok_or("amap: no temperature")?;
    if temp.is_empty() {
        return Err("amap: empty temperature".into());
    }
    let text = json_str(&body, "weather").unwrap_or_default();
    let wind = format_wind(
        &json_str(&body, "winddirection").unwrap_or_default(),
        &json_str(&body, "windpower").unwrap_or_default(),
    );
    // 城区名自动跟随：lives[0].city（如「昌平区」「北京市」）剥一层 市/区/县 后缀
    let raw_city = json_str(&body, "city").unwrap_or_default();
    let city_auto = strip_city_suffix(&raw_city);
    Ok(WeatherNow { temp, code: amap_text_code(&text), text, wind, city_auto })
}

/// 城区名剥一层尾缀（「昌平区」→昌平、「北京市」→北京）；剥完为空则原样返回。
fn strip_city_suffix(name: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        return String::new();
    }
    let stripped = n
        .strip_suffix('市')
        .or_else(|| n.strip_suffix('区'))
        .or_else(|| n.strip_suffix('县'))
        .unwrap_or(n);
    if stripped.is_empty() { n.to_string() } else { stripped.to_string() }
}

/// amap adcode 缓存文件（exe 同目录）。内容两行：`adcode` + 定位时的 unix 天数
/// （7 天复验——搬家后最迟一周自动跟随新 IP；删文件立即重定位）。
fn amap_cache_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().ok();
    let dir = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join("clockbar_amap_adcode.txt")
}

fn unix_days() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0)
}

fn valid_adcode(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit())
}

/// 高德 /v3/ip 自动定位（HTTP 直连）。成功返回 adcode 并写缓存。
/// 2026-09-16 实测：直辖市只到市级（北京→110000「北京市」），非直辖市通常区级；
/// 各免费 IP 库对家庭宽带区县判定互相矛盾（高德=北京市/ipip=北京市/wgeo=怀柔），
/// 区县级精度请用 liConfig weatherAdcode 或 LICAL_AMAP_CITY 锁定。
fn locate_amap_adcode(key: &str) -> Result<String, String> {
    let body = http_get(
        "restapi.amap.com",
        &format!("/v3/ip?key={key}"),
        "http://restapi.amap.com/",
        Duration::from_secs(8),
    )?;
    let status = json_str(&body, "status").ok_or("ip: no status")?;
    if status != "1" {
        return Err(format!("ip: status={status}"));
    }
    let adcode = json_str(&body, "adcode").ok_or("ip: no adcode")?;
    if !valid_adcode(&adcode) {
        return Err(format!("ip: bad adcode '{adcode}'"));
    }
    let p = amap_cache_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, format!("{adcode}\n{}", unix_days()));
    super::dbg_log(&format!("weather: amap ip-located adcode {adcode} -> cache"));
    Ok(adcode)
}

/// 高德 adcode 解析链（v62 全自动）：
/// ① env `LICAL_AMAP_CITY`（测试/临时覆盖）
/// ② liConfig `weatherAdcode`（设置页锁定区县级精度；6 位数字才认）
/// ③ 缓存文件（≤7 天直接用；>7 天复验）
/// ④ /v3/ip 自动定位（成功写缓存）
/// ⑤ 缓存过期但定位失败 → 陈旧值兜底 → 都没有才 Err（调用方回退中国天气网链路）。
fn resolve_amap_adcode(key: &str) -> Result<String, String> {
    if let Ok(a) = std::env::var("LICAL_AMAP_CITY") {
        if valid_adcode(&a) {
            return Ok(a);
        }
    }
    let cfg = super::current_style();
    if let Some(c) = cfg.as_ref().and_then(|c| c.weather_adcode.as_deref()) {
        let c = c.trim();
        if valid_adcode(c) {
            return Ok(c.to_string());
        }
    }
    let cache = amap_cache_path();
    let cached = std::fs::read_to_string(&cache).ok();
    if let Some(content) = &cached {
        let mut lines = content.lines();
        let ad = lines.next().unwrap_or("").trim();
        let day: u64 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
        if valid_adcode(ad) && unix_days().saturating_sub(day) < 7 {
            return Ok(ad.to_string());
        }
    }
    match locate_amap_adcode(key) {
        Ok(a) => Ok(a),
        Err(e) => match cached.and_then(|c| {
            c.lines().next().map(|s| s.trim().to_string()).filter(|s| valid_adcode(s))
        }) {
            Some(stale) => {
                super::dbg_log(&format!("weather: ip locate failed ({e}), stale cache {stale}"));
                Ok(stale)
            }
            None => Err(e),
        },
    }
}

/// 高德 key 解析（v65 方案2）：① 进程环境 `GAODE_WEATHER_API`（临时覆盖优先）；
/// ② 注册表 `HKCU\Environment` 兜底。后者是 setx 持久化用户变量的落点，但长生命
/// 周期父进程（终端/宿主）的环境块不随注册表刷新，从其启动的子进程继承不到——
/// 2026-09-19 实证：变量在注册表而 bash 启动的进程没有，高德分支整体跳过→天气源
/// 静默降级、城区名消失。直读注册表与启动环境彻底解耦。
fn amap_key() -> String {
    if let Ok(k) = std::env::var("GAODE_WEATHER_API") {
        let k = k.trim();
        if !k.is_empty() {
            return k.to_string();
        }
    }
    match amap_key_registry() {
        Some(k) => {
            super::dbg_log("weather: amap key from registry HKCU\\Environment (proc env empty)");
            k
        }
        None => {
            super::dbg_log("weather: no amap key (proc env + registry), weathercn only");
            String::new()
        }
    }
}

/// 注册表读 `HKCU\Environment\GAODE_WEATHER_API`（REG_SZ/REG_EXPAND_SZ 兼容，
/// setx 落 REG_SZ）。读失败/值为空一律 None（调用方记日志后走天气网源）。
fn amap_key_registry() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut buf = [0u16; 64]; // 32 位 hex key + NUL 上限 33，64 富余
    let mut size = (buf.len() * 2) as u32;
    let res = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            w!("GAODE_WEATHER_API"),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if res != WIN32_ERROR(0) {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]);
    let s = s.trim_end_matches('\0').trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 数据源调度（T 阶段换源定案 + v62 全自动行政区划 + v65 key 注册表兜底）：
/// key 解析链 amap_key()（进程 env → 注册表 HKCU\Environment）非空 → 高德源
/// （adcode 四层解析链见 resolve_amap_adcode）；高德失败回退中国天气网旧链路
/// （cityid 解析链不变；v65 起降级源自带城区名/风向）。两者都失败返回拼接错误。
pub fn fetch_now_any() -> Result<WeatherNow, String> {
    let key = amap_key();
    if !key.is_empty() {
        match resolve_amap_adcode(&key).and_then(|ad| fetch_now_amap(&key, &ad)) {
            Ok(w) => return Ok(w),
            Err(e) => {
                let fb = fetch_now(&resolve_cityid());
                return match fb {
                    Ok(w) => {
                        super::dbg_log("weather: amap failed, fell back to weathercn");
                        Ok(w)
                    }
                    Err(e2) => Err(format!("amap: {e}; weathercn: {e2}")),
                };
            }
        }
    }
    fetch_now(&resolve_cityid())
}

/// 城市缓存文件（生产路径：exe 同目录）。首跑 IP 定位成功后写入。
fn cache_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().ok();
    let dir = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join("clockbar_city.txt")
}

/// 城市码解析：① 环境变量 LICAL_CITYID ② 缓存文件 ③ wgeo IP 定位（成功后写缓存）。
/// 旧覆盖层 localStorage（webview-data LevelDB）迁移经 D 阶段评估语义等价（同为
/// IP 定位结果），不做 LevelDB 解析迁移。
pub fn resolve_cityid() -> String {
    if let Ok(id) = std::env::var("LICAL_CITYID") {
        if id.len() == 9 {
            return id;
        }
    }
    let cache = cache_path();
    if let Ok(id) = std::fs::read_to_string(&cache) {
        let id = id.trim();
        if id.len() == 9 {
            return id.to_string();
        }
    }
    match locate_cityid() {
        Ok(id) => {
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, &id);
            super::dbg_log(&format!("weather: located cityid {id} -> cache {:?}", cache));
            id
        }
        Err(e) => {
            super::dbg_log(&format!("weather: locate_cityid failed: {e}"));
            String::new()
        }
    }
}

/// v65 天气源降级链测试：天气网 dataSK 城区名/风向提取（编码勘误后纯 UTF-8，
/// 无需转码）+ 高德 key 解析链的注册表兜底。
#[cfg(test)]
mod weather_v65_tests {
    use super::*;

    /// dataSK 真实报文样例（2026-09-19 od 字节级实测 101010700 昌平，
    /// 202609191550 时次；中文均为 UTF-8 原字节）。
    const DATASK: &str = concat!(
        r#"{"nameen":"changping","cityname":"昌平","city":"101010700","temp":"27.4","tempf":"81.3","#,
        r#""WD":"东南","wde":"SE","WS":"2级","wse":"10km\/h","SD":"55%","sd":"55%","qy":"1008","#,
        r#""njd":"17km","time":"15:50","rain":"0","rain24h":"0","aqi":"53","aqi_pm25":"53","#,
        r#""weather":"多云","weathere":"Cloudy","weathercode":"d01"}"#
    );

    /// 降级源补城区名：cityname 取中文（非 cityDZ 段的拼音 nameen）并剥后缀。
    #[test]
    fn weathercn_cityname() {
        let j = extract_djson(&format!("var dataSK ={DATASK};"), "dataSK").unwrap();
        assert_eq!(strip_city_suffix(&json_str(&j, "cityname").unwrap()), "昌平");
        // 区/县后缀同样剥一层（海淀区→海淀）；缺字段/空值→空（城区名缺省不显示）
        let j2 = r#"{"cityname":"海淀区","temp":"20"}"#;
        assert_eq!(strip_city_suffix(&json_str(j2, "cityname").unwrap()), "海淀");
        assert!(json_str(r#"{"temp":"20"}"#, "cityname").is_none());
    }

    /// 降级源补风向：WD+WS 经 format_wind 归一，WS 的「级」后缀剥掉防重复。
    #[test]
    fn weathercn_wind() {
        let j = extract_djson(&format!("var dataSK ={DATASK};"), "dataSK").unwrap();
        let ws = json_str(&j, "WS").unwrap();
        assert_eq!(ws.trim_end_matches('级'), "2");
        assert_eq!(format_wind(&json_str(&j, "WD").unwrap(), ws.trim_end_matches('级')), "东南风2级");
        // 无 WD/WS → 空串（不拼风段，与高德口径一致）
        assert_eq!(format_wind("", ""), "");
        assert_eq!(format_wind("东南", ""), "");
    }

    /// 注册表兜底：本机 HKCU\Environment\GAODE_WEATHER_API 已由 setx 写入
    /// （32 位 hex）——直接对真实注册表断言非空（无则跳过，CI 无此变量）。
    #[test]
    fn amap_key_registry_fallback() {
        match amap_key_registry() {
            Some(k) => {
                assert_eq!(k.len(), 32);
                assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
            }
            None => {} // 未配置 key 的机器上合法
        }
    }
}
