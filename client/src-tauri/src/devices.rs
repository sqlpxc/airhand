//! 控制台的"我的设备"：连接过的电脑，保存在 config.toml 的 `[[device]]` 里（和控制码等设置在同一个文件，见 [`crate::config`]）。
//!
//! ```toml
//! [[device]]
//! code = "482913"
//! name = "DESIGN-WS"
//! alias = "设计部"
//! os = "Windows 11 Pro"
//! last_used = 2026-11-08T10:20:30Z
//! quality = { fps = 30, bitrate_kbps = 4000, max_short = 1080, adaptive = true, codec = "h264" }
//! resume = "q83vEjRWeJCrze8SNFZ4kA=="
//! host_key = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
//! ```
//!
//! `host_key` 是那台电脑的端到端加密公钥（第一次连上时记下，见 `airhand_link::controller` 的"端到端加密"）：
//! 之后只认它，对不上就警告。是公钥，不用加密。
//!
//! 控制码能直接控制那台电脑，相当于钥匙：
//! * Windows：config.toml 在安装目录里，本机的所有账户都能读，所以 `code` 用**当前用户**的 DPAPI 加密，
//!   别的账户、别的电脑都解不开。解不开的（别的账户保存的）原样保留，不显示；
//! * macOS / Linux：文件在用户自己的目录里，权限 600（见 [`crate::config`]），`code` 直接写 6 位数字。
//!
//! 手动添加时 `code` 写 6 位数字在所有平台上都可以。

use airhand_proto::session::{Codec, QualitySettings};
use airhand_proto::signal::ResumeToken;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use toml_edit::{Datetime, InlineTable, Item, Table, Value};
use tracing::warn;

/// 每个账户最多保存多少台（控制台也按这个数截断）
pub const MAX: usize = 50;
/// 文件里最多多少项（含 Windows 上别的账户保存的）
pub const MAX_IN_FILE: usize = 200;
/// 名称等文字的长度上限（字符）
const MAX_TEXT: usize = 200;
/// 加密后的 `code` 的长度上限
const MAX_CODE: usize = 1024;

/// 一台设备。和控制台之间是 JSON（camelCase）；`code` 在内存里是 6 位数字，写进文件之前才加密（[`seal`]）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub code: String,
    /// 用户起的名字（为空时显示对方的设备名）
    #[serde(default)]
    pub alias: Option<String>,
    /// 对方的设备名
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub os: String,
    /// 重连凭证：对方不在线时等它上线，不算猜码
    #[serde(default)]
    pub resume: Option<ResumeToken>,
    /// 对方的端到端加密公钥（64 位十六进制）
    #[serde(default)]
    pub host_key: Option<String>,
    /// 最后一次连接的时间（Unix 毫秒）
    #[serde(default)]
    pub last_used: i64,
    /// 上次用的画质
    #[serde(default)]
    pub quality: Option<QualitySettings>,
}

/// 6 位数字、不以 0 开头（与 airhand_proto::parse_code 相同）
pub fn valid_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) && !code.starts_with('0')
}

fn clip(s: &str) -> String {
    s.trim().chars().take(MAX_TEXT).collect()
}

/// 整理控制台发来的列表：去掉无效的、重复的，按最后连接时间排序，最多 [`MAX`] 台
pub fn normalize(mut list: Vec<Device>) -> Vec<Device> {
    list.retain(|d| valid_code(&d.code));
    list.sort_by_key(|d| std::cmp::Reverse(d.last_used));
    let mut seen = std::collections::HashSet::new();
    list.retain(|d| seen.insert(d.code.clone()));
    list.truncate(MAX);
    for d in &mut list {
        d.name = clip(&d.name);
        d.os = clip(&d.os);
        d.alias = d.alias.as_deref().map(clip).filter(|a| !a.is_empty());
        d.host_key = d.host_key.as_deref().and_then(host_key);
    }
    list
}

/// 64 位十六进制的公钥（小写）；不像样的不要
fn host_key(s: &str) -> Option<String> {
    let k = s.trim().to_ascii_lowercase();
    airhand_link::secure::unhex32(&k).map(|_| k)
}

// ───────────────────────────── 读 ─────────────────────────────

/// 配置文件里本账户的设备（文件不存在、没有设备时为空）
pub fn load() -> Vec<Device> {
    let Some(text) = crate::config::read_text() else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        warn!("config.toml isn't valid TOML, can't read the saved devices");
        return Vec::new();
    };
    let list = entries(&doc).into_iter().filter_map(open).collect();
    normalize(list)
}

/// 文件里的所有项（`code` 还没解密）
pub fn entries(doc: &toml_edit::DocumentMut) -> Vec<Device> {
    let Some(arr) = doc.get("device").and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    arr.iter().filter_map(from_table).take(MAX_IN_FILE).collect()
}

fn from_table(t: &Table) -> Option<Device> {
    let str_of = |k: &str| t.get(k).and_then(Item::as_str).map(clip).unwrap_or_default();
    let code = t.get("code")?.as_str()?.trim().to_string();
    if code.is_empty() || code.len() > MAX_CODE {
        return None;
    }
    let alias = str_of("alias");
    Some(Device {
        code,
        alias: (!alias.is_empty()).then_some(alias),
        name: str_of("name"),
        os: str_of("os"),
        resume: t
            .get("resume")
            .and_then(Item::as_str)
            .and_then(|s| STANDARD.decode(s.trim()).ok())
            .and_then(|b| ResumeToken::try_from(b.as_slice()).ok()),
        last_used: match t.get("last_used") {
            Some(Item::Value(Value::Datetime(d))) => to_millis(d.value()).unwrap_or(0),
            Some(Item::Value(Value::Integer(i))) => *i.value(),
            _ => 0,
        },
        quality: t.get("quality").and_then(quality_from),
        host_key: t.get("host_key").and_then(Item::as_str).and_then(host_key),
    })
}

fn quality_from(item: &Item) -> Option<QualitySettings> {
    let t = item.as_table_like()?;
    let int = |k: &str| t.get(k).and_then(Item::as_integer);
    // 和界面的 JSON 一样的名字（"h264"）；之前的版本写的是 "H264"，也认
    let codec = Codec::from_name(&t.get("codec").and_then(Item::as_str)?.to_ascii_lowercase())?;
    Some(QualitySettings {
        fps: u8::try_from(int("fps")?).ok().filter(|f| (1..=120).contains(f))?,
        bitrate_kbps: u32::try_from(int("bitrate_kbps")?).ok()?,
        max_short: u16::try_from(int("max_short")?).ok()?,
        adaptive: t.get("adaptive").and_then(Item::as_bool)?,
        codec,
    })
}

/// 解开 `code`：6 位数字原样使用（macOS / Linux，或者手动添加的）；Windows 上其余的按 DPAPI 解密。
/// 解不开（别的账户保存的、坏了的）返回 `None`
fn open(mut d: Device) -> Option<Device> {
    if !valid_code(&d.code) {
        d.code = crate::config::secret::open_device_code(&d.code).ok()?;
        if !valid_code(&d.code) {
            return None;
        }
    }
    Some(d)
}

// ───────────────────────────── 写 ─────────────────────────────

/// 准备写进文件的完整列表：本账户的设备（`code` 加密后），Windows 上再加上文件里别的账户的设备（原样）
pub fn for_file(mine: Vec<Device>) -> Result<Vec<Device>> {
    let mine = normalize(mine);
    let mut out = Vec::with_capacity(mine.len());
    for d in &mine {
        out.push(seal(d.clone())?);
    }
    // 别的账户保存的（本账户解不开）原样保留；本账户的以控制台发来的为准（删掉的就不要了）
    #[cfg(windows)]
    if let Some(doc) = crate::config::read_text().and_then(|t| t.parse::<toml_edit::DocumentMut>().ok()) {
        out.extend(entries(&doc).into_iter().filter(|e| open(e.clone()).is_none()));
    }
    out.truncate(MAX_IN_FILE);
    Ok(out)
}

fn seal(mut d: Device) -> Result<Device> {
    d.code = crate::config::secret::seal_device_code(&d.code)?;
    Ok(d)
}

/// 写进 config.toml 的一项。文字太长、`code` 不像样的直接丢掉（Windows 上这份列表来自命名管道，要再检查一遍）
pub fn to_table(d: &Device) -> Option<Table> {
    let code = d.code.trim();
    if code.is_empty() || code.len() > MAX_CODE || code.contains(['\n', '\r']) {
        return None;
    }
    let mut t = Table::new();
    t["code"] = toml_edit::value(code);
    t["name"] = toml_edit::value(clip(&d.name));
    if let Some(a) = d.alias.as_deref().map(clip).filter(|a| !a.is_empty()) {
        t["alias"] = toml_edit::value(a);
    }
    if !d.os.trim().is_empty() {
        t["os"] = toml_edit::value(clip(&d.os));
    }
    if let Some(dt) = from_millis(d.last_used) {
        t["last_used"] = toml_edit::value(dt);
    }
    if let Some(q) = &d.quality {
        let mut it = InlineTable::new();
        it.insert("fps", i64::from(q.fps).into());
        it.insert("bitrate_kbps", i64::from(q.bitrate_kbps).into());
        it.insert("max_short", i64::from(q.max_short).into());
        it.insert("adaptive", q.adaptive.into());
        it.insert(
            "codec",
            q.codec.or_h264().name().unwrap_or("h264").into(),
        );
        t["quality"] = toml_edit::value(it);
    }
    if let Some(r) = &d.resume {
        t["resume"] = toml_edit::value(STANDARD.encode(r));
    }
    if let Some(k) = d.host_key.as_deref().and_then(host_key) {
        t["host_key"] = toml_edit::value(k);
    }
    Some(t)
}

// ───────────────────────────── 时间 ─────────────────────────────
// last_used 写成 TOML 的日期时间（UTC），人也看得懂。不为这点功能引入日期库：公历换算用的是
// Howard Hinnant 的 days_from_civil / civil_from_days

pub(crate) fn to_millis(d: &Datetime) -> Option<i64> {
    let date = d.date?;
    let (y, m, day) = (i64::from(date.year), i64::from(date.month), i64::from(date.day));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let (h, min, s) = d.time.map_or((0, 0, 0), |t| {
        (i64::from(t.hour), i64::from(t.minute), i64::from(t.second.unwrap_or(0)))
    });
    let offset_min = match d.offset {
        Some(toml_edit::Offset::Custom { minutes }) => i64::from(minutes),
        _ => 0,
    };
    Some(((days * 24 + h) * 60 + min - offset_min) * 60_000 + s * 1000)
}

pub(crate) fn from_millis(ms: i64) -> Option<Datetime> {
    if ms <= 0 {
        return None;
    }
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(Datetime {
        date: Some(toml_edit::Date {
            year: u16::try_from(year).ok()?,
            month: month as u8,
            day: day as u8,
        }),
        time: Some(toml_edit::Time {
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: Some((rem % 60) as u8),
            nanosecond: None,
        }),
        offset: Some(toml_edit::Offset::Z),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(code: &str, last_used: i64) -> Device {
        Device {
            code: code.into(),
            alias: None,
            name: format!("PC-{code}"),
            os: "Windows 11".into(),
            resume: Some([7; 16]),
            last_used,
            quality: Some(QualitySettings {
                fps: 60,
                bitrate_kbps: 8000,
                max_short: 1080,
                adaptive: true,
                codec: Codec::H265,
            }),
            host_key: Some("ab".repeat(32)),
        }
    }

    #[test]
    fn datetime_roundtrip() {
        for ms in [1_000, 951_782_400_000, 1_762_597_230_000, 4_102_444_799_000] {
            let dt = from_millis(ms).unwrap();
            assert_eq!(to_millis(&dt), Some(ms), "{dt}");
        }
        assert_eq!(from_millis(1_762_597_230_000).unwrap().to_string(), "2025-11-08T10:20:30Z");
        // 别的时区写的也认
        let dt: Datetime = "2025-11-08T18:20:30+08:00".parse().unwrap();
        assert_eq!(to_millis(&dt), Some(1_762_597_230_000));
        assert_eq!(from_millis(0), None);
    }

    #[test]
    fn table_roundtrip() {
        let mut d = device("482913", 1_762_597_230_000);
        d.alias = Some("设计部".into());
        let t = to_table(&d).unwrap();
        assert_eq!(from_table(&t), Some(d.clone()));
        // 没有可选项的也行
        let bare = Device { alias: None, os: String::new(), resume: None, quality: None, last_used: 0, host_key: None, ..d };
        assert_eq!(from_table(&to_table(&bare).unwrap()), Some(bare));
    }

    #[test]
    fn hand_written_entries() {
        let doc: toml_edit::DocumentMut = r#"
[[device]]
code = "123456"

[[device]]
code = "654321"
name = "Mac"
last_used = 2026-01-02T03:04:05Z
quality = { fps = 999, bitrate_kbps = 1, max_short = 0, adaptive = true, codec = "H264" }

[[device]]
code = "777777"
quality = { fps = 30, bitrate_kbps = 4000, max_short = 720, adaptive = false, codec = "H265" }

[[device]]
name = "no code"
"#
        .parse()
        .unwrap();
        let list = entries(&doc);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].code, "123456");
        assert_eq!(list[1].name, "Mac");
        assert!(list[1].last_used > 0);
        // 帧率超出范围：画质整个不要，用控制台的默认
        assert_eq!(list[1].quality, None);
        // 之前的版本写的编码格式是 "H265" 这样的：照样认
        assert_eq!(list[2].quality.map(|q| q.codec), Some(Codec::H265));
        assert!(list.into_iter().all(|d| open(d).is_some()));
    }

    #[test]
    fn normalize_dedups_sorts_and_caps() {
        let mut list: Vec<Device> = (0..60).map(|i| device(&format!("{}", 100_000 + i), i)).collect();
        list.push(device("100059", 1_000));
        list.push(device("012345", 5));
        list.push(device("12345", 5));
        let out = normalize(list);
        assert_eq!(out.len(), MAX);
        assert_eq!(out[0].code, "100059");
        assert_eq!(out[0].last_used, 1_000);
        assert!(out.iter().all(|d| valid_code(&d.code)));
    }

    /// 加密过的 code 写进文件、再读回来能解开（Windows：DPAPI；其它平台：原样）。时间只保存到秒
    #[test]
    fn sealed_code_roundtrip() {
        let d = device("482913", 1_762_597_230_000);
        let sealed = seal(d.clone()).unwrap();
        #[cfg(windows)]
        assert_ne!(sealed.code, "482913");
        let back = from_table(&to_table(&sealed).unwrap()).unwrap();
        assert_eq!(open(back), Some(d));
        // 坏了的解不开：不显示（Windows 上原样保留）
        let broken = Device { code: "AAAA".into(), ..device("482913", 1) };
        assert_eq!(open(broken), None);
    }
}
