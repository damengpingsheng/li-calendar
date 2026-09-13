// D 阶段时间数据：tyme4rs（锁版本 =1.5.0）每日计算（农历/节气/节日）。
// 口径与前端 lunar-typescript 抽查对照（方案 §5 D 行验收：20 敏感日期一致）。
// 时间段不在此列——时间=系统 VM 持有的原生 TimeInnerTextBlock，tap 零写入（D2）。

use tyme4rs::tyme::solar::SolarDay;
use tyme4rs::tyme::{Culture, Tyme};

/// 单日四段数据（weather 由 weather.rs 注入；空段发单空格——tap 侧限界 JSON
/// 解析拒绝空串值，且 reflow 对 DesiredSize=0 的段跳过评估，故不能发空串）。
pub struct DayData {
    pub weather: String,
    pub festival: String,
    pub term: String,
    pub lunar: String,
}

impl DayData {
    /// 空段→单空格（视觉上≈无边距的空段，reflow 可正常度量）
    fn seg_or_blank(s: String) -> String {
        if s.trim().is_empty() { " ".to_string() } else { s }
    }

    /// c1set JSON（tap 限界解析口径：值内禁引号/反斜杠/控制符，UTF-8 直传）
    pub fn c1set_json(&self, style: &str) -> String {
        format!(
            "{{\"weather\":\"{}\",\"festival\":\"{}\",\"term\":\"{}\",\"lunar\":\"{}\"{}{}}}",
            self.weather,
            self.festival,
            self.term,
            self.lunar,
            if style.is_empty() { "" } else { "," },
            style
        )
    }
}

/// 农历行：`{月}{日}`（闰月自动带「闰」前缀），与前端 ClockOverlayWindow
/// 旧口径 `${getMonthInChinese()}月${getDayInChinese()}` 一致。
/// tyme 月名为「十一月/十二月」，按前端 lunar-typescript 惯例映射为「冬月/腊月」
/// （D 阶段对照实测发现的唯一月名分歧，映射后与 lunar-typescript 逐字一致）。
pub fn lunar_text(solar: &SolarDay) -> String {
    let ld = solar.get_lunar_day();
    let mut m = ld.get_lunar_month().get_name();
    if m == "十一月" {
        m = "冬月".to_string();
    } else if m == "十二月" {
        m = "腊月".to_string();
    }
    format!("{}{}", m, ld.get_name())
}

/// 节气行：仅节气日本身显示节气名（与前端月视图格子口径一致），非节气日为空。
pub fn term_text(solar: &SolarDay) -> String {
    if solar.get_term_day().get_day_index() == 0 {
        solar.get_term().get_name()
    } else {
        String::new()
    }
}

/// 第 n 个星期日（w=该月 1 日的星期索引，0=周日）；母亲节=5 月第 2 个周日，
/// 父亲节=6 月第 3 个周日（lunar-typescript 节日表有、tyme 无，此处补齐——
/// 2026-06-21 父亲节对照实测暴露）。
fn nth_sunday(first_weekday: usize, n: usize) -> usize {
    1 + (7 - first_weekday) % 7 + (n - 1) * 7
}

/// 节日行：阳历节日 → 农历节日 → 补充节日（情人节/母亲节/父亲节，tyme 节日表
/// 无而前端 lunar-typescript 有）→ 「除夕」兜底（tyme 已内置，保险）。
/// 清明日节日段与节气段同源重复（tyme 报「清明节」、节气段已显示「清明」，
/// 与前端「格子只显示节气」口径一致），此时节日段留空。
pub fn festival_text(solar: &SolarDay) -> String {
    let term = term_text(solar);
    let mut f = if let Some(x) = solar.get_festival() {
        x.get_name()
    } else {
        let ld = solar.get_lunar_day();
        if let Some(x) = ld.get_festival() {
            x.get_name()
        } else {
            String::new()
        }
    };
    if f.is_empty() {
        let (m, d) = (solar.get_month(), solar.get_day());
        let first = SolarDay::from_ymd(solar.get_year(), m, 1);
        f = match (m, d) {
            (2, 14) => "情人节".to_string(),
            (5, dd) if dd == nth_sunday(first.get_week().get_index(), 2) => "母亲节".to_string(),
            (6, dd) if dd == nth_sunday(first.get_week().get_index(), 3) => "父亲节".to_string(),
            _ => String::new(),
        };
    }
    if !term.is_empty() && f == format!("{term}节") {
        return String::new();
    }
    if f.is_empty() {
        let ld = solar.get_lunar_day();
        let nd = ld.next(1);
        let nm = nd.get_lunar_month();
        if nm.get_name() == "正" && nd.get_day() == 1 {
            return "除夕".to_string();
        }
    }
    f
}

/// 计算某日四段数据（weather 由调用方传入，可为占位符 "--"）。
pub fn compute_day(y: i32, m: u32, d: u32, weather: &str) -> DayData {
    let solar = SolarDay::from_ymd(y as isize, m as usize, d as usize);
    DayData {
        weather: weather.to_string(),
        festival: DayData::seg_or_blank(festival_text(&solar)),
        term: DayData::seg_or_blank(term_text(&solar)),
        lunar: lunar_text(&solar),
    }
}
