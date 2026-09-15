// D 阶段天气：weather.com.cn 实况的 Rust 新实现（方案 §3 决策表 D5 / §3 D3 定案）。
// std-only：TcpStream 纯 HTTP（无需 TLS——端点实测均支持 http 直连）。
// 自探针 crate src/weather.rs 移植（E0），差异仅城市缓存路径（生产=安装目录）。
//
// 端点定案（2026-09-13 D 阶段实测）：
// - www.weather.com.cn/data/sk/{id}.html 已 301 死链——前端旧实现已静默失效；
// - 实况：http://d1.weather.com.cn/weather_index/{id}.html?date=...
//   （必须带 Referer: http://www.weather.com.cn/），正文 GBK/UTF-8 混杂——
//   只取 ASCII 字段 temp / weathercode，天气名用代码表映射，规避 GBK 解码；
// - IP 定位：http://wgeo.weather.com.cn/ip/?_=<ts>（须带 Referer），
//   返回 `var ip="...";var id="101010700";var addr=...`（只取 ASCII 的 id）。
//
// 降级纪律（方案 §5 D 行 / §6）：断网/超时/坏数据一律返回 Err，由调用方把天气段
// 降级为占位符（"--"），绝不影响其余段与 explorer。天气获取在数据线程内阻塞
// （≤8s）不触 UI 线程/管道协议。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 实况数据（temp 原样字符串，text=天气名，code=weathercode 整数——emoji 码表用）
pub struct WeatherNow {
    pub temp: String,
    pub text: String,
    /// weathercode 整数（"d13"/"n7" 去昼夜前缀后的数值；解析失败=u32::MAX→无 emoji）
    pub code: u32,
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
    /// 现象文字开关（缺省开；关=只显图标+温度）
    pub show_text: bool,
}

impl Default for DisplayOpts {
    fn default() -> Self {
        DisplayOpts { city: String::new(), emoji: true, emoji_color: true, show_text: true }
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
    let city = o.city.trim();
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
    Ok(WeatherNow { temp, text, code: num })
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
