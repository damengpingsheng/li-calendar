// D 阶段天气：weather.com.cn 实况的 Rust 新实现（方案 §3 决策表 D5）。
// std-only：TcpStream 纯 HTTP（无需 TLS——端点实测均支持 http 直连）。
//
// 端点实测（2026-09-13，curl 取证）：
// - www.weather.com.cn/data/sk/{id}.html 已 301 死链（→ e.weather.com.cn）——
//   前端旧实现（src/http/weather.ts）在该端点上已静默失效；
// - 可用实况端点：http://d1.weather.com.cn/weather_index/{id}.html?date=...
//   （必须带 Referer: http://www.weather.com.cn/），正文返回
//   `var cityDZ={...};var alarmDZ={...};var dataSK={...}`——正文为 **GBK 编码**，
//   故只取 ASCII 字段 temp / weathercode，天气名用代码表映射，规避 GBK 解码；
// - IP 定位：http://wgeo.weather.com.cn/ip/?_=<ts>（须带 Referer），返回
//   `var ip="...";var id="101010700";var addr="北京,昌平,昌平";`（addr 为 GBK，
//   只取 ASCII 的 id）。注意：前端旧实现读取的是 window.cityid，而脚本实际
//   赋值的变量名是 id——旧实现的定位读取同样与现状不符（一并记录）。
//
// 降级纪律（方案 §5 D 行）：断网/超时/坏数据一律返回 Err，由调用方把天气段
// 降级为占位符（"--"），绝不影响其余段与 explorer。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// 实况数据（temp 原样字符串，text=天气名）
pub struct WeatherNow {
    pub temp: String,
    pub text: String,
}

impl WeatherNow {
    /// 任务栏天气段展示文案（与前端 ClockOverlayWindow 旧口径一致：`26℃ 晴`）
    pub fn display(&self) -> String {
        let t = self.temp.trim();
        let w = self.text.trim();
        let mut s = String::new();
        if !t.is_empty() {
            s.push_str(t);
            s.push('℃');
        }
        if !w.is_empty() {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(w);
        }
        s
    }
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
    let t0 = Instant::now();
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
    if std::env::var("LICAL_WX_DEBUG").map(|v| v == "1").unwrap_or(false) {
        eprintln!("[wx-debug] status+head: {:.400}", body.replace('\r', ""));
    }
    // 有 chunked 编码——简单剥离：定位空行后的正文（头后直接拼，不解析 chunk 长度，
    // d1 返回 chunked 时 body 里混有 chunk 尺寸行；dataSK 的 JSON 内无换行，可容忍）
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
    Ok(WeatherNow { temp, text })
}

/// 城市码解析：① 环境变量 LICAL_CITYID ② 缓存文件 ③ wgeo IP 定位（成功后写缓存）。
/// D 阶段缓存文件放会话目录；生产路径迁 E 阶段（含旧覆盖层 localStorage 迁移评估）。
pub fn resolve_cityid() -> String {
    if let Ok(id) = std::env::var("LICAL_CITYID") {
        if id.len() == 9 {
            return id;
        }
    }
    let cache = r"D:\agents_tmp\d_stage_20260913\clockbar_city.txt";
    if let Ok(id) = std::fs::read_to_string(cache) {
        let id = id.trim();
        if id.len() == 9 {
            return id.to_string();
        }
    }
    match locate_cityid() {
        Ok(id) => {
            let _ = std::fs::create_dir_all(r"D:\agents_tmp\d_stage_20260913");
            let _ = std::fs::write(cache, &id);
            id
        }
        Err(e) => {
            eprintln!("[weather] locate_cityid failed: {e}");
            String::new()
        }
    }
}
