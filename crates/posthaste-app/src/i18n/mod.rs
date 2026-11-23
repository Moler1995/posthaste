//! Interface language support.
//!
//! The interface is rendered in one of two languages, English (the default) or
//! Simplified Chinese, chosen in the settings dialog and stored in the
//! machine-local preferences document. The choice is process-wide, like the
//! theme: views read [`current`] at paint time, so a switch needs only a
//! repaint.
//!
//! # What is translated
//!
//! Only interface prose: button, menu, tab, section, and dialog labels,
//! tooltips, placeholders, empty and error states, and the sentences that
//! present counts, names, and diagnostics. Each sentence is one catalogue
//! entry with `{}` placeholders, so word order and punctuation belong to the
//! language rather than to a concatenation.
//!
//! # What is not translated
//!
//! - User data: request, collection, environment, variable, and header names
//!   and values.
//! - Values the application stores in workspace documents: the default names
//!   `New Request`, `New Collection`, `New Folder`, and `New Environment`.
//! - Protocol and product tokens: HTTP methods and status codes and reason
//!   phrases, header names, media types, `JSON`, `cURL`, `wget`, `.http`,
//!   `Postman`, `Posthaste`, `Cookie`, keyboard combinations, units (`B`, `KiB`,
//!   `MiB`, `ms`, `s`), URLs, and file paths.
//! - Diagnostics produced by the storage and transport layers. They appear as
//!   arguments inside a translated sentence.
//! - Identifiers: `debug_selector` values, element ids, file names, and
//!   `tracing` logs.
//!
//! Chinese and English entries that are identical are listed in
//! [`catalogue::INVARIANT_KEYS`], and the catalogue test asserts the list is
//! exactly the set of entries whose texts match.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

pub use posthaste_core::preferences::LanguageChoice as Lang;

/// Defines the key enum, its `ALL` list, and the two-language text lookup.
///
/// Every entry carries both languages, so a key without a translation cannot
/// compile.
macro_rules! text_table {
    ($($variant:ident => $en:literal / $zh:literal,)*) => {
        /// One interface string.
        ///
        /// The variant names read as catalogue keys, so a name like
        /// `AuthApiKey` is the intended spelling rather than a type prefix.
        #[allow(clippy::enum_variant_names)]
        // The title bar and its workspace menu exist only on Windows, so their
        // keys are never constructed elsewhere.
        #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Key {
            $($variant,)*
        }

        impl Key {
            /// Every key, in declaration order.
            #[cfg(test)]
            pub const ALL: &'static [Key] = &[$(Key::$variant,)*];

            /// The key's identifier, for diagnostics and tests.
            #[cfg(test)]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)*
                }
            }

            /// The text in `lang`.
            pub const fn text(self, lang: $crate::i18n::Lang) -> &'static str {
                match lang {
                    $crate::i18n::Lang::English => match self {
                        $(Self::$variant => $en,)*
                    },
                    $crate::i18n::Lang::Chinese => match self {
                        $(Self::$variant => $zh,)*
                    },
                }
            }
        }
    };
}

/// Defines the message key enum and its two-language templates.
///
/// A template is one whole sentence with `{}` placeholders, filled by [`msg`].
macro_rules! message_table {
    ($($variant:ident => $en:literal / $zh:literal,)*) => {
        /// One parameterized interface sentence.
        ///
        /// The title bar's workspace menu exists only on Windows, so its
        /// message keys are never constructed elsewhere.
        #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum MessageKey {
            $($variant,)*
        }

        impl MessageKey {
            /// Every message key, in declaration order.
            #[cfg(test)]
            pub const ALL: &'static [MessageKey] = &[$(MessageKey::$variant,)*];

            /// The key's identifier, for diagnostics and tests.
            #[cfg(test)]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)*
                }
            }

            /// The sentence template in `lang`.
            pub const fn template(self, lang: $crate::i18n::Lang) -> &'static str {
                match lang {
                    $crate::i18n::Lang::English => match self {
                        $(Self::$variant => $en,)*
                    },
                    $crate::i18n::Lang::Chinese => match self {
                        $(Self::$variant => $zh,)*
                    },
                }
            }
        }
    };
}

mod catalogue;

#[cfg(test)]
pub use catalogue::INVARIANT_KEYS;
pub use catalogue::{Key, MessageKey, counted};

const ENGLISH: u8 = 0;
const CHINESE: u8 = 1;

static CURRENT: AtomicU8 = AtomicU8::new(ENGLISH);

/// The interface language in effect.
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        CHINESE => Lang::Chinese,
        _ => Lang::English,
    }
}

/// Sets the process-wide interface language. The caller repaints.
pub fn set_language(lang: Lang) {
    // The language is process-wide while the tests share one process, so a
    // test that owns the interface through `InterfaceGuard` is its only writer
    // until the guard drops; another test's preference application must not
    // pull the interface out from under a layout it is clicking through.
    #[cfg(test)]
    if !crate::test_globals::may_change() {
        return;
    }
    let value = match lang {
        Lang::English => ENGLISH,
        Lang::Chinese => CHINESE,
    };
    CURRENT.store(value, Ordering::Relaxed);
}

/// The text of `key` in the current language.
pub fn t(key: Key) -> &'static str {
    key.text(current())
}

/// The sentence of `key` in the current language with `args` substituted.
pub fn msg(key: MessageKey, args: &[&dyn fmt::Display]) -> String {
    fill(key.template(current()), args)
}

/// Substitutes `args` into each `{}` of `template`, left to right.
///
/// A placeholder without a matching argument is left as written rather than
/// panicking, and extra arguments are ignored.
pub fn fill(template: &str, args: &[&dyn fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut args = args.iter();
    while let Some(index) = rest.find("{}") {
        out.push_str(&rest[..index]);
        match args.next() {
            Some(arg) => {
                if fmt::write(&mut out, format_args!("{arg}")).is_err() {
                    out.push_str("{}");
                }
            }
            None => out.push_str("{}"),
        }
        rest = &rest[index + 2..];
    }
    out.push_str(rest);
    out
}

/// Test support for the process-wide language.
#[cfg(test)]
pub(crate) mod testing {
    /// The interface globals, owned by one test at a time.
    pub(crate) use crate::test_globals::InterfaceGuard;
}

#[cfg(test)]
mod tests {
    use super::testing::InterfaceGuard;
    use super::*;

    #[test]
    fn english_is_the_default_and_a_change_is_visible_until_restored() {
        let _guard = InterfaceGuard::take();
        assert_eq!(current(), Lang::English);

        set_language(Lang::Chinese);
        assert_eq!(current(), Lang::Chinese);

        drop(_guard);
        assert_eq!(current(), Lang::English);
    }

    #[test]
    fn substitution_runs_left_to_right() {
        assert_eq!(
            fill("{} then {}", &[&"first", &"second"]),
            "first then second"
        );
        assert_eq!(fill("no placeholders", &[&1]), "no placeholders");
        assert_eq!(fill("{}", &[&7]), "7");
    }

    #[test]
    fn a_missing_argument_keeps_the_placeholder_and_extra_arguments_are_ignored() {
        assert_eq!(fill("a {} b {}", &[&1]), "a 1 b {}");
        assert_eq!(fill("a", &[&1, &2]), "a");
        assert_eq!(fill("{}", &[]), "{}");
    }

    #[test]
    fn counts_follow_the_language_form() {
        let _guard = InterfaceGuard::take();
        assert_eq!(counted::cookie(1), "1 cookie");
        assert_eq!(counted::cookie(3), "3 cookies");
        assert_eq!(counted::item(counted::Item::Request, 1), "1 request");
        assert_eq!(
            counted::list(&["1 folder".to_owned(), "2 requests".to_owned()]),
            "1 folder and 2 requests"
        );

        set_language(Lang::Chinese);
        assert_eq!(counted::cookie(3), "3 个 Cookie");
        assert_eq!(counted::item(counted::Item::Folder, 2), "2 个文件夹");
        assert_eq!(
            counted::list(&["1 个文件夹".to_owned(), "2 个请求".to_owned()]),
            "1 个文件夹和2 个请求"
        );
    }

    #[test]
    fn the_catalogue_is_complete_in_both_languages() {
        for key in Key::ALL {
            assert!(
                !key.text(Lang::English).is_empty(),
                "{} is missing English",
                key.name()
            );
            assert!(
                !key.text(Lang::Chinese).is_empty(),
                "{} is missing Chinese",
                key.name()
            );
        }
        for key in MessageKey::ALL {
            let english = key.template(Lang::English);
            let chinese = key.template(Lang::Chinese);
            assert!(!english.is_empty(), "{} is missing English", key.name());
            assert!(!chinese.is_empty(), "{} is missing Chinese", key.name());
            assert_eq!(
                english.matches("{}").count(),
                chinese.matches("{}").count(),
                "{} has a different placeholder count in the two languages",
                key.name()
            );
        }
    }

    #[test]
    fn only_the_declared_invariants_read_the_same_in_both_languages() {
        let same: Vec<&'static str> = Key::ALL
            .iter()
            .filter(|key| key.text(Lang::English) == key.text(Lang::Chinese))
            .map(|key| key.name())
            .collect();
        assert_eq!(same, INVARIANT_KEYS);
        let same: Vec<&'static str> = MessageKey::ALL
            .iter()
            .filter(|key| key.template(Lang::English) == key.template(Lang::Chinese))
            .map(|key| key.name())
            .collect();
        assert!(same.is_empty(), "{same:?}");
    }
}
