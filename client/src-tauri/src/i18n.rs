//! 界面语言：简体中文 / 英文。
//!
//! * 托盘菜单和控制台都可以切换，选的语言记在 config.toml 的 `language` 里（见 [`crate::config`]，
//!   Windows 安装版由服务里的被控端写），下次启动接着用；
//! * 没选过就跟随系统界面语言：系统是中文就用中文，其它语言（或识别不出来）一律用英文；
//! * 托盘和窗口标题按当前语言生成（用 [`tr!`]）；控制台的文字在前端（`src/lib/i18n.ts`）；
//! * 发给界面的问题与提示都是类型（见 [`crate::problem`]），显示时才按语言变成文字；日志始终是英文。

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Lang {
    Zh = 1,
    En = 2,
}

impl Lang {
    /// 由语言标签判断（`zh-CN`、`zh_TW.UTF-8`、`en-US`、`C`…）：只认中文，其它都是英文。
    /// Windows 直接用语言 ID 判断，用不到它。
    #[cfg(any(not(windows), test))]
    pub fn from_tag(tag: &str) -> Lang {
        let primary = tag.trim().split(['-', '_', '.', '@']).next().unwrap_or("");
        if primary.eq_ignore_ascii_case("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }

    /// 前端和配置文件使用的代码（`"zh"` / `"en"`）
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// [`Lang::code`] 反过来（配置文件里的 `language`），认不出来时为 `None`
    pub fn from_code(code: &str) -> Option<Lang> {
        match code.trim() {
            "zh" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        }
    }
}

/// 0 = 尚未确定（第一次使用时读配置文件里选过的，没有就读系统语言）
static CURRENT: AtomicU8 = AtomicU8::new(0);

fn decode(v: u8) -> Option<Lang> {
    match v {
        1 => Some(Lang::Zh),
        2 => Some(Lang::En),
        _ => None,
    }
}

pub fn current() -> Lang {
    if let Some(l) = decode(CURRENT.load(Ordering::Relaxed)) {
        return l;
    }
    let l = crate::config::settings().language.unwrap_or_else(system);
    // 多个线程同时第一次调用时，以先写入的为准（用户已经切换过就不会被覆盖）
    match CURRENT.compare_exchange(0, l as u8, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => l,
        Err(v) => decode(v).unwrap_or(l),
    }
}

/// 切换界面语言（只是这个进程里的，记进配置文件的是 [`crate::config::save_language`]），返回是否有变化。
pub fn set(lang: Lang) -> bool {
    CURRENT.swap(lang as u8, Ordering::Relaxed) != lang as u8
}

pub fn is_zh() -> bool {
    current() == Lang::Zh
}

/// 系统界面语言。只看首选语言：不是中文就用英文。
#[cfg(windows)]
pub fn system() -> Lang {
    // 用户的"显示语言"（不是"区域格式"）；主语言 ID 0x04 = LANG_CHINESE（简体、繁体都算中文）
    const LANG_CHINESE: u16 = 0x04;
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == LANG_CHINESE {
        Lang::Zh
    } else {
        Lang::En
    }
}

/// 系统界面语言。只看首选语言：不是中文就用英文。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn system() -> Lang {
    posix_lang(|k| std::env::var(k).ok())
}

/// 系统界面语言："系统设置 → 通用 → 语言与地区"里排第一的语言（例如 `zh-Hans-CN`）。
/// 从程序坞 / 访达启动的程序没有 `LANG` 之类的环境变量，所以不看环境变量。
#[cfg(target_os = "macos")]
pub fn system() -> Lang {
    let first = objc2::rc::autoreleasepool(|_| {
        let langs = objc2_foundation::NSLocale::preferredLanguages();
        (langs.count() > 0).then(|| langs.objectAtIndex(0).to_string())
    });
    first.map_or(Lang::En, |l| Lang::from_tag(&l))
}

/// 与 gettext 相同的规则：`LC_ALL` > `LC_MESSAGES` > `LANG` 决定区域设置；
/// 区域设置不是 C/POSIX 时，`LANGUAGE`（冒号分隔的优先列表）优先，取第一个。
#[cfg(any(not(any(windows, target_os = "macos")), test))]
fn posix_lang(var: impl Fn(&str) -> Option<String>) -> Lang {
    let get = |k: &str| var(k).filter(|v| !v.trim().is_empty());
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(get);
    let is_c = locale
        .as_deref()
        .is_none_or(|l| l == "C" || l == "POSIX" || l.starts_with("C."));
    let language = if is_c { None } else { get("LANGUAGE") };
    let tag = language
        .as_deref()
        .and_then(|l| l.split(':').find(|s| !s.is_empty()))
        .or(locale.as_deref());
    tag.map_or(Lang::En, Lang::from_tag)
}

/// 按当前界面语言二选一并格式化，写法同 `format!`：`tr!("显示器 {n}", "Display {n}")`。
macro_rules! tr {
    ($zh:literal, $en:literal $(, $($args:tt)*)?) => {
        if $crate::i18n::is_zh() { format!($zh $(, $($args)*)?) } else { format!($en $(, $($args)*)?) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        for t in [
            "zh",
            "zh-CN",
            "zh-Hans-CN",
            "zh_TW.UTF-8",
            "ZH_cn",
            "zh@pinyin",
        ] {
            assert_eq!(Lang::from_tag(t), Lang::Zh, "{t}");
        }
        for t in [
            "en",
            "en-US",
            "en_GB.UTF-8",
            "fr-FR",
            "ja_JP.UTF-8",
            "C",
            "POSIX",
            "C.UTF-8",
            "",
            "zhx",
        ] {
            assert_eq!(Lang::from_tag(t), Lang::En, "{t}");
        }
    }

    #[test]
    fn posix() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(posix_lang(env(&[])), Lang::En);
        assert_eq!(posix_lang(env(&[("LANG", "zh_CN.UTF-8")])), Lang::Zh);
        assert_eq!(
            posix_lang(env(&[("LANG", "zh_CN.UTF-8"), ("LC_ALL", "en_US.UTF-8")])),
            Lang::En
        );
        assert_eq!(
            posix_lang(env(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_MESSAGES", "zh_CN.UTF-8")
            ])),
            Lang::Zh
        );
        assert_eq!(
            posix_lang(env(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "zh_CN:en")])),
            Lang::Zh
        );
        // 首选语言不是中文：用英文（不往后找）
        assert_eq!(
            posix_lang(env(&[("LANG", "fr_FR.UTF-8"), ("LANGUAGE", "fr:zh_CN")])),
            Lang::En
        );
        // 区域设置是 C 时忽略 LANGUAGE
        assert_eq!(
            posix_lang(env(&[("LANG", "C"), ("LANGUAGE", "zh_CN")])),
            Lang::En
        );
        assert_eq!(posix_lang(env(&[("LANGUAGE", "zh_CN")])), Lang::En);
        assert_eq!(
            posix_lang(env(&[("LANG", ""), ("LC_ALL", "zh_SG")])),
            Lang::Zh
        );
    }

    /// 配置文件里的 `language` 只认这两个代码
    #[test]
    fn codes() {
        for l in [Lang::Zh, Lang::En] {
            assert_eq!(Lang::from_code(l.code()), Some(l));
        }
        for c in ["", "zh-CN", "EN", "system"] {
            assert_eq!(Lang::from_code(c), None, "{c}");
        }
    }

    #[test]
    fn switch() {
        let before = current();
        set(Lang::En);
        assert_eq!(tr!("中文 {}", "English {}", 1), "English 1");
        let n = 2;
        assert!(!set(Lang::En));
        assert!(set(Lang::Zh));
        assert_eq!(tr!("中文 {n}", "English {n}"), "中文 2");
        set(before);
    }
}
