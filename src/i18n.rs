//! Every word the pages show goes through gettext (askama_gettext, as blit's
//! pages do): the English is the msgid, written in the template as
//! `{{ __("…") }}`, and a locale's catalogue in `locales/<locale>/messages.po`
//! translates it. A msgid with no translation renders as written.
//!
//! Only en-GB exists, and the msgids are already en-GB, so nothing is loaded
//! at run time and the binary needs no catalogue beside it. Adding a locale
//! is a catalogue, its code in the catalogue test's list, and loading it and
//! choosing it per request; the templates need nothing.
//!
//! The catalogue is extracted from the templates by a test, which fails while
//! it is stale; `just i18n` rewrites it, keeping any translations.
//!
//! What the browser's script shows comes from the layout's
//! `<template id="copy">`, rendered through the same calls. What stays
//! as it is, until there is a second language, is the server's own error
//! text in API answers and the formatting of numbers, sizes and times.

use std::path::Path;
use std::sync::LazyLock;

use askama_gettext::{Catalogs, Fuzzy, Translator};

/// The source locale: what the msgids are written in.
pub const LOCALE: &str = "en-GB";

static CATALOGS: LazyLock<Catalogs> = LazyLock::new(|| {
    // The source locale's catalogue holds no translations, so none is read.
    Catalogs::load(Path::new(""), &[], LOCALE, Fuzzy::Skip)
        .expect("loading no catalogues cannot fail")
});

pub fn translator() -> Translator<'static> {
    Translator::new(&CATALOGS, LOCALE)
}

/// A count from each plural form the locale has, in the catalogue's order:
/// the copy the browser's script reads gives a counted line once at each,
/// and the browser files each under the category its count falls in.
pub fn plural_samples() -> &'static [u64] {
    static SAMPLES: LazyLock<Vec<u64>> = LazyLock::new(|| {
        let forms = askama_gettext::plural::Forms::new(LOCALE)
            .expect("CLDR has plural rules for the source locale");
        let mut first = vec![None; forms.count()];
        for n in 0..1000 {
            first[forms.index(n)].get_or_insert(n);
        }
        first.into_iter().flatten().collect()
    });
    &SAMPLES
}

/// A translated message, as a template's `__`, `__p` and `__n` give it:
/// escaped where it is shown, as askama_gettext's own is, and taken by a
/// koan-kit-web component as its text, which askama_gettext's type cannot be,
/// both types being other crates'.
#[derive(Clone)]
pub struct Words(pub askama_gettext::Message);

impl Words {
    /// Supplies a `%{name}` value.
    pub fn with(self, name: &str, value: impl std::fmt::Display) -> Self {
        Self(self.0.with(name, value))
    }
}

impl std::fmt::Display for Words {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Words> for koan_kit_web::Text<'_> {
    fn from(words: Words) -> Self {
        words.to_string().into()
    }
}

impl From<Words> for koan_kit_web::Content<'_> {
    fn from(words: Words) -> Self {
        words.to_string().into()
    }
}

impl From<&Words> for koan_kit_web::Text<'_> {
    fn from(words: &Words) -> Self {
        words.to_string().into()
    }
}

/// Gives template structs the gettext vocabulary (`__`, `__p`, `__n`), which
/// askama resolves as methods on the template, each giving [`Words`]. A macro
/// rather than a blanket impl, which the orphan rule forbids for the crate's
/// trait.
macro_rules! translated {
    ($($t:ty),* $(,)?) => {
        $(
            impl askama_gettext::Translate for $t {
                fn translator(&self) -> askama_gettext::Translator<'_> {
                    $crate::i18n::translator()
                }
            }

            #[allow(dead_code, reason = "a template uses the calls its words need")]
            impl $t {
                fn __(&self, msgid: &str) -> $crate::i18n::Words {
                    $crate::i18n::Words(askama_gettext::Translate::__(self, msgid))
                }

                fn __p(&self, context: &str, msgid: &str) -> $crate::i18n::Words {
                    $crate::i18n::Words(askama_gettext::Translate::__p(self, context, msgid))
                }

                fn __n(&self, msgid: &str, plural: &str, count: u64) -> $crate::i18n::Words {
                    $crate::i18n::Words(askama_gettext::Translate::__n(self, msgid, plural, count))
                }
            }
        )*
    };
}
pub(crate) use translated;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Every locale with a catalogue.
    const LOCALES: &[&str] = &[LOCALE];

    /// Every `.html` under `templates/`, sorted so the catalogue's order holds.
    fn templates(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(templates(&path));
            } else if path.extension().is_some_and(|e| e == "html") {
                found.push(path);
            }
        }
        found.sort();
        found
    }

    /// A catalogue with its references to where each message is used naming
    /// files alone: references with lines would change whenever a template
    /// gained one, though none of its words had.
    fn by_file(po: &str) -> String {
        let mut out = po
            .lines()
            .map(|line| {
                let Some(refs) = line.strip_prefix("#: ") else {
                    return line.to_string();
                };
                let mut files: Vec<&str> = Vec::new();
                for r in refs.split_whitespace() {
                    let file = match r.rsplit_once(':') {
                        Some((file, at)) if at.chars().all(|c| c.is_ascii_digit()) => file,
                        _ => r,
                    };
                    if !files.contains(&file) {
                        files.push(file);
                    }
                }
                format!("#: {}", files.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if po.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Each locale's catalogue is what extraction makes of the templates now,
    /// so a template's new or changed words reach it. `just i18n` (this test
    /// with `UPDATE_CATALOGUES` set) rewrites them, keeping translations.
    #[test]
    fn the_catalogues_are_up_to_date() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let syntax = askama_parser::Syntax::default();
        let mut messages = Vec::new();
        for path in templates(&root.join("templates")) {
            // Relative, so a `#:` reference reads the same on every machine.
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let source = std::fs::read_to_string(&path).unwrap();
            messages.extend(
                askama_gettext::extract::from_str(&source, &name, &syntax)
                    .unwrap_or_else(|e| panic!("extracting from {name}: {e}")),
            );
        }
        assert!(!messages.is_empty());
        let update = std::env::var_os("UPDATE_CATALOGUES").is_some();
        for locale in LOCALES {
            let committed = root.join("locales").join(locale).join("messages.po");
            let dir = tempfile::tempdir().unwrap();
            let fresh = dir.path().join("messages.po");
            // The source locale has no translations to keep, so it is built
            // afresh, in the templates' order and with nothing marked fuzzy as
            // a rewording of what it said before.
            if committed.exists() && *locale != LOCALE {
                std::fs::copy(&committed, &fresh).unwrap();
            } else {
                askama_gettext::merge::create_catalog(&fresh, locale).unwrap();
            }
            askama_gettext::merge::into_catalog(&fresh, locale, &messages).unwrap();
            let fresh = by_file(&std::fs::read_to_string(&fresh).unwrap());
            if update {
                std::fs::create_dir_all(committed.parent().unwrap()).unwrap();
                std::fs::write(&committed, &fresh).unwrap();
            } else {
                assert_eq!(
                    std::fs::read_to_string(&committed).unwrap_or_default(),
                    fresh,
                    "{} is stale: run `just i18n`",
                    committed.display()
                );
            }
        }
    }

    /// Every key in `source` that follows `call` (`say("` reads the keys of
    /// `say("key"`).
    fn keys<'a>(source: &'a str, call: &str) -> std::collections::BTreeSet<&'a str> {
        source
            .split(call)
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .collect()
    }

    /// The browser's script asks for its words by key from the layout's copy,
    /// a counted line from the entries rendered once per plural form. A key on
    /// one side only is a word the script would show as its key, or one the
    /// catalogue keeps for nothing.
    #[test]
    fn the_script_and_its_copy_name_the_same_lines() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let script = std::fs::read_to_string(root.join("browser/app.ts")).unwrap();
        let layout = std::fs::read_to_string(root.join("templates/app/layout.html")).unwrap();
        let (mut plain, mut counted) = (
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        );
        for tag in layout.split("<span ").skip(1) {
            let tag = tag.split('>').next().unwrap();
            let Some(key) = tag
                .split("data-k=\"")
                .nth(1)
                .and_then(|k| k.split('"').next())
            else {
                continue;
            };
            if tag.contains("data-n=") {
                counted.insert(key);
            } else {
                plain.insert(key);
            }
        }
        assert_eq!(keys(&script, "say(\""), plain, "plain lines");
        assert_eq!(keys(&script, "sayN(\""), counted, "counted lines");
    }

    #[test]
    fn english_has_a_singular_and_a_plural() {
        assert_eq!(plural_samples(), [1, 0]);
    }

    #[test]
    fn untranslated_english_renders_as_written() {
        let t = translator();
        assert_eq!(t.locale(), LOCALE);
        assert_eq!(CATALOGS.gettext(LOCALE, "choose images"), "choose images");
    }
}
