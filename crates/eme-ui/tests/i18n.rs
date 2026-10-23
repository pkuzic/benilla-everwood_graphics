//! Locale files: both languages carry the same keys, every key the source uses exists.

use eme_ui::{EmeI18n, EmeLang};
use std::collections::BTreeSet;

fn placeholders(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        out.insert(rest[i..i + j + 1].to_owned());
        rest = &rest[i + j + 1..];
    }
    out
}

#[test]
fn both_languages_have_the_same_keys_and_placeholders() {
    let i = EmeI18n::builtin();
    let en = i.table(EmeLang::EnUs);
    let ru = i.table(EmeLang::RuRu);
    let en_keys: BTreeSet<_> = en.keys().collect();
    let ru_keys: BTreeSet<_> = ru.keys().collect();
    assert_eq!(en_keys, ru_keys, "enUS and ruRU key sets differ");
    for (k, v) in en {
        assert!(k.starts_with("MONKEY_EME_"), "{k}");
        assert!(!v.trim().is_empty(), "{k} empty in enUS");
        assert!(!ru[k].trim().is_empty(), "{k} empty in ruRU");
        assert_eq!(
            placeholders(v),
            placeholders(&ru[k]),
            "{k}: placeholders differ"
        );
    }
}

#[test]
fn russian_is_really_russian() {
    // Every string with letters must differ from English unless it is a proper noun or glyph-only.
    let i = EmeI18n::builtin();
    let en = i.table(EmeLang::EnUs);
    let ru = i.table(EmeLang::RuRu);
    let same: Vec<_> = en
        .iter()
        .filter(|(k, v)| &ru[*k] == *v)
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        same,
        vec!["MONKEY_EME_TITLE"],
        "untranslated strings: {same:?}"
    );
}

/// Every `"MONKEY_EME_*"` literal in the crate's source names a key both files define.
#[test]
fn every_key_used_in_source_exists() {
    let i = EmeI18n::builtin();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let text = std::fs::read_to_string(&p).unwrap();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\"MONKEY_EME_") {
            let tail = &rest[at + 1..];
            let end = tail.find('"').unwrap();
            let key = &tail[..end];
            rest = &tail[end + 1..];
            if key.contains('{') || key.ends_with('_') {
                continue; // a format template, e.g. MONKEY_EME_{}NAME
            }
            found += 1;
            for lang in [EmeLang::EnUs, EmeLang::RuRu] {
                assert!(
                    i.has(lang, key),
                    "{key} (used in {}) missing in {lang:?}",
                    p.display()
                );
            }
        }
    }
    assert!(found > 40, "scanner found only {found} keys");
}

#[test]
fn builtin_tool_names_exist() {
    let i = EmeI18n::builtin();
    for id in [
        "SCULPT", "FLATTEN", "WATER", "AREA", "OBJECTS", "RESTORE", "LOG",
    ] {
        let k = format!("MONKEY_EME_{id}_NAME");
        assert!(i.has(EmeLang::EnUs, &k) && i.has(EmeLang::RuRu, &k), "{k}");
    }
}

#[test]
fn lookup_falls_back_and_extends() {
    let mut i = EmeI18n::builtin();
    assert_eq!(i.get(EmeLang::EnUs, "MONKEY_EME_NOPE"), "MONKEY_EME_NOPE");
    i.extend_json(EmeLang::EnUs, r#"{"MONKEY_EME_X_ONLY_EN":"hello {0}"}"#)
        .unwrap();
    // A key missing in ruRU falls back to enUS.
    assert_eq!(i.get(EmeLang::RuRu, "MONKEY_EME_X_ONLY_EN"), "hello {0}");
    assert_eq!(
        i.format(EmeLang::EnUs, "MONKEY_EME_X_ONLY_EN", &["w"]),
        "hello w"
    );
    assert_eq!(EmeLang::from_locale("ruRU"), EmeLang::RuRu);
    assert_eq!(EmeLang::from_locale("deDE"), EmeLang::EnUs);
}
