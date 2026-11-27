use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En = 0,
    Zh = 1,
}

impl Lang {
    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Zh,
            _ => Self::En,
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Zh => "zh",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Zh => "简体中文",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase().replace('_', "-");
        match norm.as_str() {
            "en" | "en-us" | "en-gb" | "eng" | "english" | "英文" | "英语" => Some(Self::En),
            "zh" | "zh-cn" | "zh-hans" | "cn" | "chs" | "chinese" | "中文" | "简体中文" => {
                Some(Self::Zh)
            }
            _ => None,
        }
    }
}

static ACTIVE: AtomicU8 = AtomicU8::new(Lang::En as u8);

std::thread_local! {
    static TEST_OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

pub fn current() -> Lang {
    if let Some(lang) = TEST_OVERRIDE.with(|c| c.get()) {
        return lang;
    }
    Lang::from_u8(ACTIVE.load(Ordering::Relaxed))
}

pub fn set(lang: Lang) {
    ACTIVE.store(lang as u8, Ordering::Relaxed);
}

pub fn is_zh() -> bool {
    current() == Lang::Zh
}

pub fn tr<'a>(en: &'a str, zh: &'a str) -> &'a str {
    if is_zh() { zh } else { en }
}

/// Runs `f` on the current thread with `lang` active without disturbing parallel threads.
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<Lang>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_OVERRIDE.with(|c| c.set(self.0));
        }
    }
    let prev = TEST_OVERRIDE.with(|c| c.replace(Some(lang)));
    let _reset = Reset(prev);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_english_and_with_lang_is_scoped() {
        assert_eq!(current(), Lang::En);
        assert_eq!(tr("hello", "你好"), "hello");
        with_lang(Lang::Zh, || {
            assert_eq!(current(), Lang::Zh);
            assert!(is_zh());
            assert_eq!(tr("hello", "你好"), "你好");
            with_lang(Lang::En, || {
                assert_eq!(current(), Lang::En);
                assert_eq!(tr("hello", "你好"), "hello");
            });
            assert_eq!(current(), Lang::Zh);
        });
        assert_eq!(current(), Lang::En);
    }

    #[test]
    fn parse_handles_common_aliases() {
        for raw in ["en", "EN", "en_US", "en-gb", "english"] {
            assert_eq!(Lang::parse(raw), Some(Lang::En), "{raw}");
        }
        for raw in ["zh", "zh-CN", "zh_hans", "cn", "chinese", "中文"] {
            assert_eq!(Lang::parse(raw), Some(Lang::Zh), "{raw}");
        }
        assert_eq!(Lang::parse(""), None);
        assert_eq!(Lang::parse("fr"), None);
    }
}
