//! Every glyph the shell can draw exists in the egui fonts (no "tofu" boxes): the locale strings
//! of both languages and the status glyphs in `shell.rs`.

use bevy_egui::egui::{self, FontId};

fn ctx() -> egui::Context {
    let ctx = egui::Context::default();
    // Fonts load on the first pass.
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    ctx
}

fn missing(ctx: &egui::Context, text: &str) -> Vec<char> {
    let font = FontId::proportional(14.0);
    ctx.fonts_mut(|f| {
        text.chars()
            .filter(|c| !c.is_whitespace() && !f.has_glyph(&font, *c))
            .collect()
    })
}

/// The non-ASCII characters inside string literals of `src` (comments skipped).
fn literal_glyphs(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines() {
        let code = line.split("//").next().unwrap_or("");
        let mut inside = false;
        for c in code.chars() {
            if c == '"' {
                inside = !inside;
            } else if inside && !c.is_ascii() {
                out.push(c);
            }
        }
    }
    out
}

#[test]
fn every_shell_glyph_and_locale_string_has_a_font_glyph() {
    let ctx = ctx();
    let shell = literal_glyphs(include_str!("../src/shell.rs"));
    assert!(!shell.is_empty());
    assert_eq!(missing(&ctx, &shell), Vec::<char>::new(), "shell.rs glyphs");
    for (name, json) in [
        ("enUS", include_str!("../locales/enUS.json")),
        ("ruRU", include_str!("../locales/ruRU.json")),
    ] {
        let table: std::collections::HashMap<String, String> = serde_json::from_str(json).unwrap();
        for (k, v) in table {
            assert_eq!(missing(&ctx, &v), Vec::<char>::new(), "{name} {k} = {v:?}");
        }
    }
}
