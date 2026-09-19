// D 阶段时间数据：tyme4rs（锁版本 =1.5.0）每日计算（农历/节气/节日）。
// 口径与前端 lunar-typescript 抽查对照（方案 §5 D 行验收：24 敏感日期一致）。
// v64：时间/日期段改自建段——文案在此格式化（token 子集，走时线程 c1t 下发 +
// c1set 引导恒带），tap 侧零格式逻辑。
// 自探针 crate src/data.rs 逐字移植（E0）。

use tyme4rs::tyme::solar::SolarDay;
use tyme4rs::tyme::{Culture, Tyme}; // Culture=各类 get_name()；Tyme=LunarDay::next()（探针同款导入）

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
        self.c1set_json_with_clock(style, "")
    }

    /// c1set JSON + v64 时钟文本扩展（clock=`"time":"..","date":".."`，无前导逗号）。
    /// 走时稳态不进 c1set（避免秒级变化触发全量重发）；c1set 每次发送恒带当前
    /// 时间/日期=会话引导/重建即时有值（走时线程 c1t 补差）。
    pub fn c1set_json_with_clock(&self, style: &str, clock: &str) -> String {
        format!(
            "{{\"weather\":\"{}\",\"festival\":\"{}\",\"term\":\"{}\",\"lunar\":\"{}\"{}{}{}{}}}",
            self.weather,
            self.festival,
            self.term,
            self.lunar,
            if style.is_empty() && clock.is_empty() { "" } else { "," },
            style,
            if style.is_empty() || clock.is_empty() { "" } else { "," },
            clock
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

/// 西方节日名单（v63 用户定案：任务栏节日段不显示西方节日、也不作倒计时目标）。
/// tyme 节日表本无西方节日（D 阶段实证），此表防御 D 阶段补充表类的外部来源。
fn is_western_festival(name: &str) -> bool {
    matches!(
        name,
        "情人节"
            | "白色情人节"
            | "愚人节"
            | "复活节"
            | "母亲节"
            | "父亲节"
            | "感恩节"
            | "万圣节"
            | "平安夜"
            | "圣诞节"
    )
}

/// 倒计时目标白名单（v63：中国传统节日+法定节日，名称=tyme 节日表实查）。
/// 妇女节/植树节/青年节/儿童节/建党节/建军节/教师节/中元节/龙头节/冬至节等
/// 当天仍可显示名，但不倒数（非节假日口径）。
fn is_countdown_target(name: &str) -> bool {
    matches!(
        name,
        "元旦"
            | "除夕"
            | "春节"
            | "元宵节"
            | "清明节"
            | "劳动节"
            | "端午节"
            | "七夕节"
            | "中秋节"
            | "重阳节"
            | "国庆节"
            | "腊八节"
    )
}

/// 当日节日原始名（阳历节日→农历节日→「除夕」兜底；不去重不过滤）。
fn festival_raw(solar: &SolarDay) -> String {
    let f = if let Some(x) = solar.get_festival() {
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
        let ld = solar.get_lunar_day();
        let nd = ld.next(1);
        let nm = nd.get_lunar_month();
        if nm.get_name() == "正" && nd.get_day() == 1 {
            return "除夕".to_string();
        }
    }
    f
}

/// 节日行（任务栏节日段当日名）：西方节日不显示（v63）；清明日与节气段同源
/// 去重返回空（tyme 报「清明节」、节气段已显示「清明」）。D 阶段的西方节日
/// 补充表（情人节/母亲节/父亲节）按用户定案整体移除。
pub fn festival_text(solar: &SolarDay) -> String {
    let f = festival_raw(solar);
    if f.is_empty() || is_western_festival(&f) {
        return String::new();
    }
    let term = term_text(solar);
    if !term.is_empty() && f == format!("{term}节") {
        return String::new();
    }
    f
}

/// 节日段文本（v63 用户定案）：当天有中国节日（传统+法定+纪念日）→ 节日名；
/// 否则 → 正向逐日扫描 tyme 节日表，距最近一个白名单目标（中国传统节日/法定
/// 节日，≤400 天必中）显示「距春节10天」。清明日在白名单内但当日名去重给节气
/// 段，扫描用 festival_raw（不去重）保证「距清明节N天」可达。
pub fn festival_segment_text(y: i32, m: u32, d: u32) -> String {
    let solar = SolarDay::from_ymd(y as isize, m as usize, d as usize);
    let today = festival_text(&solar);
    if !today.is_empty() {
        return today;
    }
    for i in 1..=400usize {
        let f = festival_raw(&solar.next(i as isize));
        if is_countdown_target(&f) {
            return format!("距{f}{i}天");
        }
    }
    String::new()
}

/// 计算某日数据（weather=天气段文本；festival_seg=节日段文本（v63 含倒计时，
/// 由调用方按日缓存后传入——倒计时需正向扫描多日，不能每秒重算））。
pub fn compute_day(y: i32, m: u32, d: u32, weather: &str, festival_seg: &str) -> DayData {
    let solar = SolarDay::from_ymd(y as isize, m as usize, d as usize);
    DayData {
        weather: weather.to_string(),
        festival: DayData::seg_or_blank(festival_seg.to_string()),
        term: DayData::seg_or_blank(term_text(&solar)),
        lunar: lunar_text(&solar),
    }
}

/// 本地日期 (年, 月, 日)（日界翻转判定用）。
pub fn local_ymd() -> (i32, u32, u32) {
    let st = super::ffi::now_local_time();
    (st.year as i32, st.month as u32, st.day as u32)
}

// ── v64 时间/日期文案格式化（token 子集；tap 侧零格式逻辑）─────────────

/// 星期短名（day_of_week: 0=周日）。
const WEEK_SHORT: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
/// 星期全名。
const WEEK_FULL: [&str; 7] = ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"];

/// token 渲染：按「长 token 优先」逐位匹配，未匹配字符按 utf-8 字面输出。
fn render_tokens(fmt: &str, tokens: &[(&str, String)]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < fmt.len() {
        let mut matched = false;
        for (p, v) in tokens {
            if fmt[i..].starts_with(p) {
                out.push_str(v);
                i += p.len();
                matched = true;
                break;
            }
        }
        if !matched {
            let ch = fmt[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn num(v: u16, pad2: bool) -> String {
    if pad2 && v < 10 {
        format!("0{v}")
    } else {
        v.to_string()
    }
}

/// 时间文案。token：HH(00-23)/H、hh(01-12)/h、mm/m、ss/s、tt(上午/下午)。
/// 缺省 "HH:mm"。字面字符（含中文）原样输出；无转义机制。
pub fn format_time_text(st: &super::ffi::LOCAL_TIME, fmt: &str) -> String {
    let h12 = match st.hour % 12 {
        0 => 12,
        x => x,
    };
    let tt = if st.hour < 12 { "上午" } else { "下午" };
    render_tokens(
        fmt,
        &[
            ("HH", num(st.hour, true)),
            ("hh", num(h12, true)),
            ("mm", num(st.minute, true)),
            ("ss", num(st.second, true)),
            ("tt", tt.to_string()),
            ("H", num(st.hour, false)),
            ("h", num(h12, false)),
            ("m", num(st.minute, false)),
            ("s", num(st.second, false)),
        ],
    )
}

/// 日期文案。token：yyyy/yy、MM/M、dd/d、ddd(周六)/dddd(星期六)。
/// 缺省 "yyyy/M/d"。字面字符原样输出。
pub fn format_date_text(st: &super::ffi::LOCAL_TIME, fmt: &str) -> String {
    let dow = (st.day_of_week % 7) as usize;
    render_tokens(
        fmt,
        &[
            ("yyyy", st.year.to_string()),
            ("dddd", WEEK_FULL[dow].to_string()),
            ("ddd", WEEK_SHORT[dow].to_string()),
            ("yy", format!("{:02}", st.year % 100)),
            ("MM", num(st.month, true)),
            ("dd", num(st.day, true)),
            ("M", num(st.month, false)),
            ("d", num(st.day, false)),
        ],
    )
}

/// 配置取时间格式（空/缺省→"HH:mm"）。
fn time_format_of(cfg: Option<&crate::app_runtime::config::ClockbarStyleConfig>) -> &str {
    cfg.and_then(|c| c.time_format.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("HH:mm")
}

/// 配置取日期格式（空/缺省→"yyyy/M/d"）。
fn date_format_of(cfg: Option<&crate::app_runtime::config::ClockbarStyleConfig>) -> &str {
    cfg.and_then(|c| c.date_format.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("yyyy/M/d")
}

/// 时钟文本清洗：剔除 tap 限界 JSON 不接受的字符（引号/反斜杠/控制符）。
/// 格式串来自预设或 liConfig——手改配置可能带入，双端防御。
fn sanitize_seg_text(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .collect()
}

/// 生产默认样式（v56：fontscale 1.0=段字号与所在行系统文本对齐（行感知基准，
/// 见 tap v56 注）；segmaxw 220（v61：天气段加城区名/风向风级后 170 截断「东北
/// 风1~3级」实测，放宽单段预算，capw 总预算+优先级隐藏仍护底）/ capw 620 / input 1）。
pub const DEFAULT_STYLE: &str = "\"fontscale\":1.0,\"segmaxw\":220,\"capw\":620,\"input\":1";

/// S 阶段段 id 与 tap SEGNAME 对齐（hide/rows 仍仅数据段）；v64 外观键（colors/sizes/
/// order）覆盖全部六段——time/date 也是自建段。
const SEG_IDS: [&str; 4] = ["weather", "festival", "term", "lunar"];
const LOOK_IDS: [&str; 6] = ["weather", "festival", "term", "lunar", "time", "date"];

/// v64 order 迁移：五元素旧配置（无 date）→ 在首个行 2 元素前插入 date。
/// 旧模型日期行=[原生 Date, 行2 数据段…]——date 插在行 2 序列最前=视觉不变。
fn migrate_order_5to6(
    order: &[String],
    cfg: &crate::app_runtime::config::ClockbarStyleConfig,
) -> Vec<String> {
    if order.len() == 5 && order.iter().any(|id| id == "time") && !order.iter().any(|id| id == "date")
    {
        let row_of = |id: &str| -> i32 {
            if id == "time" {
                return 1;
            }
            let d = if id == "lunar" { 2 } else { 1 };
            match cfg.rows.get(id) {
                Some(&v) if v == 2 => 2,
                _ => d,
            }
        };
        let pos = order
            .iter()
            .position(|id| row_of(id) == 2)
            .unwrap_or(order.len());
        let mut migrated = order.to_vec();
        migrated.insert(pos, "date".to_string());
        migrated
    } else {
        order.to_vec()
    }
}

fn clamp_f64(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// #rrggbb（6 位十六进制）校验（tap 侧还会再校验一次——双端防御）。
fn is_hex6(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[0] == b'#'
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c) || (b'A'..=b'F').contains(c))
}

/// v51 style 扩展片段：由 liConfig `clockbarStyle` 构建追加点（`,"style":{...}`）。
/// host 侧先做校验/钳制（order 合法排列、色值 #rrggbb、sizes/gap 钳制），
/// 非法字段静默丢弃（tap 缺省=现行为，双保险）。无配置 → 空串（tap 会话复位兜底）。
/// v55 定式：配置存在时**全量恒发**（hide/rows/colors/sizes/gap 缺省值也发）——
/// 「增量字段+缺省不发」必致会话内残留（hide/show/colors/sizes 四次同型实测）。
pub fn style_ext_json() -> String {
    let Some(cfg) = super::current_style() else {
        return String::new();
    };
    let mut f: Vec<String> = Vec::new();

    // order：必须是六个已知 id 的合法排列（含 time、date 各恰一次）才下发；
    // v64 迁移：旧五元素配置先插 date（migrate_order_5to6）再校验
    let order: Vec<String> = {
        let filtered: Vec<String> = cfg
            .order
            .iter()
            .filter(|id| LOOK_IDS.contains(&id.as_str()))
            .cloned()
            .collect();
        migrate_order_5to6(&filtered, &cfg)
    };
    let mut order = order;
    order.dedup();
    let mut want_time = false;
    let mut want_date = false;
    let mut seg_count = 0usize;
    for id in &order {
        if id == "time" {
            want_time = true;
        } else if id == "date" {
            want_date = true;
        } else {
            seg_count += 1;
        }
    }
    if want_time && want_date && seg_count == SEG_IDS.len() && order.len() == 6 {
        f.push(format!("\"order\":\"{}\"", order.join(",")));
    }

    // hide 恒发（v53——tap 空值=全开）
    let hidden: Vec<&str> = SEG_IDS
        .iter()
        .copied()
        .filter(|id| !cfg.show.get(*id).copied().unwrap_or(true))
        .collect();
    f.push(format!("\"hide\":\"{}\"", hidden.join(",")));

    // rows 恒发（v55 行归属：1=时间行，2=日期行；缺省 weather/festival/term→1，lunar→2）
    for id in SEG_IDS {
        let d = if id == "lunar" { 2 } else { 1 };
        let r = match cfg.rows.get(id) {
            Some(&v) if v == 2 => 2,
            _ => d,
        };
        f.push(format!("\"row_{id}\":{r}"));
    }

    // colors 恒发（v55/v64——"theme" 兜底，消会话内残留；time/date 缺省同样跟随主题）
    for id in LOOK_IDS {
        match cfg.colors.get(id) {
            Some(c) if is_hex6(c) => {
                f.push(format!("\"color_{id}\":\"{}\"", c.to_lowercase()));
            }
            _ => f.push(format!("\"color_{id}\":\"theme\"")),
        }
    }

    // sizes 恒发（v55/v64——缺省 1.00；钳制 0.5~2.0；time/date=字号可调本体）
    for id in LOOK_IDS {
        let v = match cfg.sizes.get(id) {
            Some(&v) if v.is_finite() => clamp_f64(v, 0.5, 2.0),
            _ => 1.0,
        };
        f.push(format!("\"size_{id}\":{v:.2}"));
    }

    // gap/gap2：0~40 钳制（恒发，缺省 10；gap=时间行，gap2=日期行——v57 分开调节）
    f.push(format!(
        "\"gap\":{:.0}",
        cfg.gap.map(|g| clamp_f64(g, 0.0, 40.0)).unwrap_or(10.0)
    ));
    f.push(format!(
        "\"gap2\":{:.0}",
        cfg.gap2.map(|g| clamp_f64(g, 0.0, 40.0)).unwrap_or(10.0)
    ));
    // vgap：两行垂直间距（0~20 钳制恒发，缺省 0=原生紧排；v59 tap 第二行 Margin）
    f.push(format!(
        "\"vgap\":{:.0}",
        cfg.vgap.map(|g| clamp_f64(g, 0.0, 20.0)).unwrap_or(0.0)
    ));

    // align1/align2：行水平对齐（0=靠左缺省 1=居中 2=靠右——v58，时间数字右侧
    // 空白=系统内边距+两行宽度差，对齐可配让用户消除窄行右侧留白）
    let ha = |v: Option<i32>| match v {
        Some(1) => 1,
        Some(2) => 2,
        _ => 0,
    };
    f.push(format!("\"align1\":{}", ha(cfg.halign_time)));
    f.push(format!("\"align2\":{}", ha(cfg.halign_date)));

    if f.is_empty() {
        return String::new();
    }
    format!(",\"style\":{{{}}}", f.join(","))
}

/// 数据线程（每个会话一份；watch 建会话成功后 spawn）：
/// 1s 粒度日界翻转比对；天气 30min 刷新。v59：fetch 失败/无城市/坏数据一律
/// **保留上次成功值**（温度变化慢，陈旧值远比 -- 有用；从未成功过才维持 --）；
/// 失败 30s 快速重试（唤醒后网络栈就绪典型 10~60s）。
/// pipe 写失败不再退出（v57）：休眠唤醒时 tap 心跳兜底 AUTO 会断/重连管道实例，
/// 写失败可能只是重连窗口——退避重试，等待 watch 的 REINIT 信号或管道自愈。
pub fn spawn_data_thread() {
    std::thread::Builder::new()
        .name("clockbar-data".into())
        .spawn(|| {
            let mut cur_date = local_ymd();
            // T 阶段：保存 WeatherNow 对象（非展示串），每 tick 按当前配置重算展示
            // （城区名/emoji/现象文字配置变更 ≤1s 生效，无需等 30min 刷新）；None=从未
            // 成功获取（显示 -- 占位）
            let mut wx: Option<super::weather::WeatherNow> = None;
            let mut last_wx = std::time::Instant::now()
                - std::time::Duration::from_secs(30 * 60);
            let mut last_sent = String::new();
            let mut write_retry: u32 = 0;
            let mut just_refreshed = false;
            // v63 节日段（倒计时/当日名）按日缓存——正向扫描多日不能每秒重算
            let mut fest_date = (0i32, 0u32, 0u32);
            let mut fest_seg = String::new();
            loop {
                if super::STOP.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // v57：tap AUTO 恢复信号（watch 置位）→ 强制重发当前行重建面板+立即刷天气
                if super::DATA_REINIT.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    super::dbg_log("data: REINIT — resend current row + immediate weather refresh");
                    last_sent.clear();
                    last_wx = std::time::Instant::now()
                        - std::time::Duration::from_secs(30 * 60);
                }
                let (y, m, d) = local_ymd();
                if (y, m, d) != cur_date {
                    super::dbg_log(&format!(
                        "data: DAY ROLLOVER {}-{:02}-{:02} -> {y}-{m:02}-{d:02}",
                        cur_date.0, cur_date.1, cur_date.2
                    ));
                    cur_date = (y, m, d);
                }
                if (y, m, d) != fest_date {
                    fest_seg = festival_segment_text(y, m, d);
                    fest_date = (y, m, d);
                    if !fest_seg.is_empty() {
                        super::dbg_log(&format!("data: festival segment: {fest_seg}"));
                    }
                }
                if last_wx.elapsed() >= std::time::Duration::from_secs(30 * 60) {
                    last_wx = std::time::Instant::now();
                    just_refreshed = true;
                    // v61：高德源优先（GAODE_WEATHER_API 在场，含风向风级/湿度），
                    // 失败自动回退中国天气网旧链路——调度在 weather::fetch_now_any
                    match super::weather::fetch_now_any() {
                        Ok(w) => {
                            wx = Some(w);
                            super::dbg_log("data: weather refreshed");
                        }
                        Err(e) => {
                            // v59：失败保留上次成功值（温度变化慢，陈旧值远比 -- 有用；
                            // 从未成功过才维持 -- 占位）。60s→30s 快速重试（唤醒后网络
                            // 栈就绪典型 10~60s，日志实证 22:29:57 失败→22:30:58 恢复）
                            super::dbg_log(&format!(
                                "data: weather fetch failed ({e}) — keep last good value"
                            ));
                            last_wx = std::time::Instant::now()
                                - std::time::Duration::from_secs(29 * 60 + 30);
                        }
                    }
                }
                let dd = {
                    // T 阶段：展示串按当前 liConfig 每 tick 重算（含 emoji/城区名/现象文字）
                    let cfg = super::current_style();
                    let opts = super::weather::DisplayOpts::from_config(cfg.as_ref());
                    let wx_disp = super::weather::format_now(wx.as_ref(), &opts);
                    if just_refreshed {
                        // 刷新当轮日志带展示串（含 emoji），确认映射与配置生效
                        just_refreshed = false;
                        super::dbg_log(&format!("data: weather display: {wx_disp}"));
                    }
                    compute_day(y, m, d, &wx_disp, &fest_seg)
                };
                // v57：样式每 tick 重读全局（设置变更 ≤1s 生效）；行变化才发 c1set。
                // v64：发送行恒带当前时间/日期文本（会话引导/重建即时有值）；
                // 比较口径=不含时钟字段的 base（秒级变化不触发重发，稳态走 c1t）
                let style = format!("{DEFAULT_STYLE}{}", style_ext_json());
                let base = dd.c1set_json(&style);
                let line = {
                    let cfg = super::current_style();
                    let st_now = super::ffi::now_local_time();
                    let t_txt = sanitize_seg_text(&format_time_text(&st_now, time_format_of(cfg.as_ref())));
                    let d_txt = sanitize_seg_text(&format_date_text(&st_now, date_format_of(cfg.as_ref())));
                    let clock = format!("\"time\":\"{t_txt}\",\"date\":\"{d_txt}\"");
                    dd.c1set_json_with_clock(&style, &clock)
                };
                if base != last_sent || write_retry > 0 {
                    match super::session::send_c1set(&line) {
                        Some(ack) => {
                            last_sent = base;
                            write_retry = 0;
                            super::dbg_log(&format!("data: c1set ack {ack}"));
                        }
                        None => {
                            // v57：写失败（休眠唤醒断管重连窗口）退避重试，不退出——
                            // 旧设计在此 return，data 链死亡且 watch 心跳无感=天气永停 --
                            write_retry += 1;
                            if write_retry <= 5 || write_retry % 30 == 1 {
                                super::dbg_log(&format!(
                                    "data: c1set write failed (retry #{write_retry})"
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            continue;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
        })
        .ok();
}

/// v64 走时线程：独立于天气数据循环（同步 HTTP 会拖住循环，时钟不能跟着停摆）。
/// 250ms 轮询 + 按文本变化才发送（秒显=每秒一拍，无秒=每分钟一拍；发送经
/// send_c1t——幂等不等待 ack，避免与数据线程互吃全局游标响应）。REINIT（tap AUTO
/// 恢复/断管重连）→ 清缓存强制补发。进程内单例（原子闸）。
pub fn spawn_tick_thread() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("clockbar-tick".into())
        .spawn(|| {
            super::dbg_log("tick: thread started (v64)");
            let mut last: Option<(String, String)> = None;
            let mut fail: u32 = 0;
            loop {
                if super::STOP.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // v64 重连补发：与 DATA_REINIT 同源置位（watch），独立消费
                if super::TICK_REINIT.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    super::dbg_log("tick: REINIT — force resend");
                    last = None;
                }
                let cfg = super::current_style();
                let st = super::ffi::now_local_time();
                let t = sanitize_seg_text(&format_time_text(&st, time_format_of(cfg.as_ref())));
                let d = sanitize_seg_text(&format_date_text(&st, date_format_of(cfg.as_ref())));
                if last.as_ref() != Some(&(t.clone(), d.clone())) {
                    if super::session::send_c1t(&t, &d) {
                        if fail > 0 {
                            super::dbg_log(&format!("tick: write recovered after {fail} fails"));
                        }
                        last = Some((t, d));
                        fail = 0;
                    } else {
                        // 断管窗口（休眠唤醒/会话切换）：清缓存待管道自愈后补发；
                        // 2s 退避（与 data 线程同款节奏，不刷日志洪泛）
                        fail += 1;
                        last = None;
                        if fail <= 3 || fail % 20 == 1 {
                            super::dbg_log(&format!("tick: c1t write failed (retry #{fail})"));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2000));
                        continue;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .ok();
}

#[cfg(test)]
mod festival_v63_tests {
    use super::*;

    /// v63 节日段语义锁定：当日名（中国节日）/西方节日不显示/倒计时格式/清明去重。
    /// 期望值=2026-09-16 实测 tyme4rs 1.5.0 输出（名称以 tyme 节日表为准）。
    #[test]
    fn festival_segments_2026() {
        let cases: [(i32, u32, u32, &str); 15] = [
            (2026, 1, 1, "元旦"),
            (2026, 2, 14, "距除夕2天"),   // 情人节：西方不显示→倒数除夕
            (2026, 2, 16, "除夕"),
            (2026, 2, 17, "春节"),
            (2026, 2, 18, "距元宵节13天"), // 春节次日
            (2026, 3, 8, "妇女节"),       // 纪念日当天显示、非倒计时目标
            (2026, 4, 5, "距劳动节26天"),  // 清明日：节日段去重给节气段
            (2026, 4, 4, "距清明节1天"),   // 前一日：raw 扫描可达清明
            (2026, 5, 1, "劳动节"),
            (2026, 5, 10, "距端午节40天"), // 母亲节：西方不显示
            (2026, 6, 19, "端午节"),
            (2026, 9, 16, "距中秋节9天"),
            (2026, 9, 25, "中秋节"),
            (2026, 10, 1, "国庆节"),
            (2026, 12, 25, "距元旦7天"), // 圣诞：西方不显示
        ];
        for (y, m, d, want) in cases {
            assert_eq!(festival_segment_text(y, m, d), want, "{y}-{m:02}-{d:02}");
        }
        // 西方节日过滤与清明去重（当日名口径）
        let valentine = SolarDay::from_ymd(2026, 2, 14);
        assert_eq!(festival_text(&valentine), "");
        let qm = SolarDay::from_ymd(2026, 4, 5);
        assert_eq!(festival_text(&qm), "");
        assert_eq!(festival_raw(&qm), "清明节");
    }
}
