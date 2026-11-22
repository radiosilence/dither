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
//! English is the server's own error text in API answers.

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

/// Gives template structs the gettext vocabulary (`__`, `__p`, `__n`, …),
/// which askama resolves as methods on the template. A macro rather than a
/// blanket impl, which the orphan rule forbids for the crate's trait.
macro_rules! translated {
    ($($t:ty),* $(,)?) => {
        $(
            impl askama_gettext::Translate for $t {
                fn translator(&self) -> askama_gettext::Translator<'_> {
                    $crate::i18n::translator()
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
            if committed.exists() {
                std::fs::copy(&committed, &fresh).unwrap();
            } else {
                askama_gettext::merge::create_catalog(&fresh, locale).unwrap();
            }
            askama_gettext::merge::into_catalog(&fresh, locale, &messages).unwrap();
            let fresh = std::fs::read_to_string(&fresh).unwrap();
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

    #[test]
    fn untranslated_english_renders_as_written() {
        let t = translator();
        assert_eq!(t.locale(), LOCALE);
        assert_eq!(CATALOGS.gettext(LOCALE, "choose images"), "choose images");
    }
}
