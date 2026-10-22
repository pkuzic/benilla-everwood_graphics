//! The locale loader: `locales/{enUS,ruRU}.json`, flat `MONKEY_EME_*` → text maps.
//!
//! The shell's own strings are embedded; a host or a tool crate adds more with
//! [`EmeI18n::extend_json`]. A missing key falls back to enUS, then to the key itself (so a
//! missing translation is visible rather than blank).

use bevy::prelude::Resource;
use std::collections::HashMap;

/// The shell's embedded English table.
pub const EN_US_JSON: &str = include_str!("../locales/enUS.json");
/// The shell's embedded Russian table.
pub const RU_RU_JSON: &str = include_str!("../locales/ruRU.json");

/// A UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EmeLang {
    /// English (`enUS`).
    #[default]
    EnUs,
    /// Russian (`ruRU`).
    RuRu,
}

impl EmeLang {
    /// The client locale name.
    pub fn locale(self) -> &'static str {
        match self {
            EmeLang::EnUs => "enUS",
            EmeLang::RuRu => "ruRU",
        }
    }

    /// Parses a client locale name (`"ruRU"`); anything unknown is English.
    pub fn from_locale(name: &str) -> Self {
        if name.eq_ignore_ascii_case("ruRU") {
            EmeLang::RuRu
        } else {
            EmeLang::EnUs
        }
    }
}

/// The language the host wants. The host sets this; the shell only reads it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeLanguage(pub EmeLang);

/// The loaded string tables.
#[derive(Resource, Debug, Clone)]
pub struct EmeI18n {
    en: HashMap<String, String>,
    ru: HashMap<String, String>,
}

impl Default for EmeI18n {
    fn default() -> Self {
        Self::builtin()
    }
}

impl EmeI18n {
    /// The embedded tables.
    pub fn builtin() -> Self {
        Self {
            en: parse(EN_US_JSON).expect("embedded enUS.json is valid"),
            ru: parse(RU_RU_JSON).expect("embedded ruRU.json is valid"),
        }
    }

    /// Merges a JSON object of extra keys into `lang`'s table (later wins).
    pub fn extend_json(&mut self, lang: EmeLang, json: &str) -> Result<(), serde_json::Error> {
        let extra = parse(json)?;
        match lang {
            EmeLang::EnUs => self.en.extend(extra),
            EmeLang::RuRu => self.ru.extend(extra),
        }
        Ok(())
    }

    /// The table of `lang`.
    pub fn table(&self, lang: EmeLang) -> &HashMap<String, String> {
        match lang {
            EmeLang::EnUs => &self.en,
            EmeLang::RuRu => &self.ru,
        }
    }

    /// Whether `lang` has `key` itself (no fallback).
    pub fn has(&self, lang: EmeLang, key: &str) -> bool {
        self.table(lang).contains_key(key)
    }

    /// The text of `key` in `lang`; falls back to enUS, then to the key.
    pub fn get<'a>(&'a self, lang: EmeLang, key: &'a str) -> &'a str {
        self.table(lang)
            .get(key)
            .or_else(|| self.en.get(key))
            .map(String::as_str)
            .unwrap_or(key)
    }

    /// [`Self::get`] with `{0}`, `{1}`, … replaced by `args`.
    pub fn format(&self, lang: EmeLang, key: &str, args: &[&str]) -> String {
        let mut text = self.get(lang, key).to_string();
        for (i, a) in args.iter().enumerate() {
            text = text.replace(&format!("{{{i}}}"), a);
        }
        text
    }

    /// A language-bound view, what the drawing code carries around.
    pub fn tr(&self, lang: EmeLang) -> Tr<'_> {
        Tr { i18n: self, lang }
    }
}

fn parse(json: &str) -> Result<HashMap<String, String>, serde_json::Error> {
    serde_json::from_str(json)
}

/// A language-bound lookup.
#[derive(Clone, Copy)]
pub struct Tr<'a> {
    i18n: &'a EmeI18n,
    lang: EmeLang,
}

impl<'a> Tr<'a> {
    /// The language.
    pub fn lang(&self) -> EmeLang {
        self.lang
    }

    /// Text of `key`.
    pub fn t(&self, key: &'a str) -> &'a str {
        self.i18n.get(self.lang, key)
    }

    /// Text of `key`, or `None` when neither the language nor enUS has it.
    pub fn try_t(&self, key: &str) -> Option<&'a str> {
        self.i18n
            .table(self.lang)
            .get(key)
            .or_else(|| self.i18n.table(EmeLang::EnUs).get(key))
            .map(String::as_str)
    }

    /// Text of `key` with `{n}` placeholders filled.
    pub fn f(&self, key: &str, args: &[&str]) -> String {
        self.i18n.format(self.lang, key, args)
    }
}
