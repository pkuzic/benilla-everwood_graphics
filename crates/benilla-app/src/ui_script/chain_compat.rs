//! **MONKEY (stock FrameXML)** — the compatibility layer a NON-STOCK FrameXML needs.
//!
//! Decision 1751 points this interface at "the stock 1.12 FrameXML run off the player's own
//! install" ([`super::reference_ui`]). That sentence has an unstated premise: that what the chain
//! hands back for `Interface\FrameXML\…` **is** Blizzard's file. On a private-server client it is
//! not. Everwood ships a Turtle-derived install whose `patch-9.mpq` replaces 49 of the manifest's
//! 89 chain entries with Turtle's own rewrites — files written against the **Turtle client's**
//! engine surface, not 1.12's, and that surface is a superset.
//!
//! ## Why this is a shim layer and not a source rule
//!
//! The obvious fix is to make `Interface\FrameXML\…` resolve only from the stock archives and let
//! the custom patches keep everything else. It is not available on this install: a past MPQ
//! de-duplication pass stripped from the base archives every file a later patch overrode, so the
//! stock copies **are not there to prefer**. Measured against
//! `G:\tw_experiment\Everwood - Working\Data`, 2026-09-17:
//!
//! | file | stock archives (`interface.MPQ`, `patch.MPQ`, `patch-2.MPQ`) | chain winner |
//! |---|---|---|
//! | `UIParent.lua` | **absent from all three** | `patch-9.mpq` |
//! | `ChatFrame.lua`, `FloatingChatFrame.lua`, `GameTooltip.lua`, `ContainerFrame.lua`, … | **absent** | `patch-9.mpq` |
//! | `FrameXML.toc` | **absent** | `patch-9.mpq` |
//!
//! `interface.MPQ` still lists 46 FrameXML entries, but every one of them is a file **no** patch
//! overrode (`AuctionFrame`, `TalentFrame`, `TradeSkillFrame`, …) — which is exactly the fingerprint
//! of a de-dup, and it is why "prefer the stock archive" would silently resolve to nothing. A
//! second, un-de-duplicated 1.12 install on the same machine (`G:\twmoa_1181_cn\twmoa_1181\Data`)
//! *does* still carry `Interface\FrameXML\OptionsFrame.lua` in all three; that copy is where
//! [`OPTIONS_FRAME_SLIDERS`] below was read from, so the transcription is the reference's own text
//! rather than a guess.
//!
//! So: the chain stays the one source, and the gap is closed from our side. Each shim below is one
//! name, with the reason it is missing and which files call it.
//!
//! ## The rule
//!
//! Seeded **before** the manifest walk ([`super::manifest::load_core`]), which makes the
//! precedence right by construction and needs no knowledge of where in the toc a file sits: on a
//! genuinely stock install the reference's own `OptionsFrame.lua` runs later and overwrites the
//! seed with its nine authoritative rows, and `wipe` is simply never called. On this install
//! nothing overwrites either, and Turtle's files find what they expect. Nothing is *replaced*; a
//! hole is filled, and the reference wins every collision it turns up for.
//!
//! `WOW_FRAMEXML=stock` skips the layer (trust the install, run the reference bare);
//! `WOW_FRAMEXML=chain` forces it on. Unset is [`framexml_source`]'s verdict, taken off the chain.

use std::path::Path;
use std::sync::OnceLock;

use benilla_ui::script::UiScript;
use bevy::prelude::*;

/// The file the verdict is taken on. It is the right probe for three reasons: every install has it,
/// it is the file the whole HUD's layout pass lives in (so a rewrite of it is what actually hurts),
/// and it is the one the live failure named — Turtle's copy calls `wipe` at l.1929 inside
/// `UIParent_ManageFramePositions`, which our load-time bootstrap runs.
const PROBE: &str = r"Interface\FrameXML\UIParent.lua";

/// Where this VM's `Interface\FrameXML\…` actually comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameXmlSource {
    /// A stock Blizzard archive won the probe — the reference's own text, so no shim is wanted.
    Stock,
    /// A custom patch won it (or the file is absent entirely, which a stock install cannot do) —
    /// the shims go in.
    Chain,
}

/// The archives Blizzard shipped, and the only ones whose contents are the reference's by
/// construction: the ten base archives [`benilla_formats::VANILLA_BASE_ORDER`] names, plus the two
/// official patch archives of the 1.12 line. Anything else in `Data\` is somebody's mod.
fn is_stock_archive(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    benilla_formats::VANILLA_BASE_ORDER
        .iter()
        .chain(["patch.MPQ", "patch-2.MPQ"].iter())
        .any(|stock| name.eq_ignore_ascii_case(stock))
}

/// The verdict, with the environment override folded in — **taken as arguments** so the whole rule
/// is testable without touching the process environment (the reason
/// `addons::root_from` exists, and the same flake it was avoiding).
///
/// An ABSENT probe reads as [`FrameXmlSource::Chain`], not `Stock`: a stock install always has
/// `UIParent.lua`, so "nowhere in the chain" means a broken or partial install, and a filled hole
/// is strictly better there than a nil-global raise.
fn framexml_source_from(winner: Option<&Path>, over: Option<&str>) -> FrameXmlSource {
    match over.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("stock") => FrameXmlSource::Stock,
        Some(s) if s.eq_ignore_ascii_case("chain") => FrameXmlSource::Chain,
        _ => match winner {
            Some(p) if is_stock_archive(p) => FrameXmlSource::Stock,
            _ => FrameXmlSource::Chain,
        },
    }
}

/// [`framexml_source_from`] against the real chain and the real environment, decided **once per
/// process** — the chain does not change under a running client, and the INFO line below is a fact
/// about the install, not about this VM. (The shims themselves are per-VM; only the verdict and its
/// one log line are cached.)
pub(super) fn framexml_source() -> FrameXmlSource {
    static VERDICT: OnceLock<FrameXmlSource> = OnceLock::new();
    *VERDICT.get_or_init(|| {
        let over = std::env::var("WOW_FRAMEXML").ok();
        let winner = super::reference_ui::winning_archive(PROBE);
        let verdict = framexml_source_from(winner, over.as_deref());
        let from = winner
            .and_then(|p| p.file_name())
            .map_or_else(|| "nothing in the chain".to_string(), |n| n.to_string_lossy().into_owned());
        match verdict {
            FrameXmlSource::Stock => info!(
                "ui_script: FrameXML compat shims OFF — {PROBE} comes from {from}{}, so the \
                 reference's own files run bare",
                if over.is_some() { " (forced by WOW_FRAMEXML=stock)" } else { ", a stock Blizzard archive" }
            ),
            FrameXmlSource::Chain => info!(
                "ui_script: FrameXML compat shims ON — {PROBE} comes from {from}{}; seeding the \
                 globals a non-stock FrameXML expects (wipe, OptionsFrameSliders). \
                 WOW_FRAMEXML=stock turns this off",
                if over.is_some() { " (forced by WOW_FRAMEXML=chain)" } else { ", not a stock Blizzard archive" }
            ),
        }
        verdict
    })
}

/// **Shim 1 — `wipe(t)`.** Empty a table in place and hand it back.
///
/// Not a 1.12 global (it arrives in 2.0), so nothing in benilla registers it and stock FrameXML
/// never calls it — but the Turtle client has it, and Turtle's FrameXML leans on it at **eleven
/// sites across six** of the files this manifest sources: `UIParent.lua` l.1420/1929/1930,
/// `BattlefieldFrame.lua` l.103/107, `FloatingChatFrame.lua` l.1106, `FriendsFrame.lua` l.492,
/// `OptionsFrame.lua` l.131/320, `WorldStateFrame.lua` l.248/330.
///
/// That one at l.1929 is what made this a *regression* rather than a missing feature: the managed
/// -positions pass runs at load ([`super::manifest::bootstrap_positions`]) and from the stance bar's
/// and the world-state frame's `OnLoad`, so the nil-global raise aborted the pass three times per
/// login and left the whole HUD shelf unseated — the empty unit frame and the chat window's
/// unmoved plate the director photographed.
///
/// Semantics are 2.0's: clear every key, **and return the table** — the return is load-bearing, not
/// decoration, because `WorldStateFrame.lua` l.248 writes it straight back
/// (`CustomStatData[name] = wipe(CustomStatData[name])`) and that same line can hand in a nil, which
/// therefore has to come back rather than raise. Guarded on absence so a client that really does
/// have the global keeps its own (and a `ReloadUI` re-seeds identically).
const WIPE: &str = r#"
if not wipe then
    function wipe(t)
        if type(t) == "table" then
            for k in pairs(t) do t[k] = nil end
        end
        return t
    end
end
"#;

/// **Shim 2 — `OptionsFrameSliders`.** The video options window's published slider table.
///
/// Stock `OptionsFrame.lua` declares it at l.24-34 and PUBLISHES it: addons read and write the rows
/// (pfUI's `hdgraphic` opens with `_G.OptionsFrameSliders[3].maxValue = 15`). Our own
/// `assets/ui/OptionsFrame.xml` stopped declaring it when decision 2177 put the stock window on the
/// manifest — correctly, because a declaration *after* the reference's would have replaced nine
/// rows with three — and now reads rows 1, 2, 3, 6 and 9 for its Graphics page's bounds.
///
/// Turtle's `OptionsFrame.lua` is not that file. It is a from-scratch categories/search options
/// window that keeps its bounds in its own `Options.lua` table and never declares this name at all,
/// so all five `OnLoad`s indexed a nil.
///
/// The rows are the reference's own text, transcribed from an install that still carries it
/// (`interface.MPQ`/`patch.MPQ`/`patch-2.MPQ` of `G:\twmoa_1181_cn\twmoa_1181\Data`; the `patch-2`
/// copy is the 1.12.1 one and is what is written here). `OPTIONS_FARCLIP_MIN/MAX` are NOT seeded:
/// Turtle's own `Options.lua` l.1-2 declares them, with the reference's 177/777, and it loads
/// ahead of any reader. `text`/`tooltipText` stay symbolic so they pick up whatever
/// `GlobalStrings.lua` this install ships.
const OPTIONS_FRAME_SLIDERS: &str = r#"
if not OptionsFrameSliders then
    OptionsFrameSliders = {
        { text = UI_SCALE, func = "uiscale", minValue = 0.64, maxValue = 1.0, valueStep = 0.01, tooltipText = OPTION_TOOLTIP_UI_SCALE },
        { text = FARCLIP, func = "farclip", minValue = 177, maxValue = 777, valueStep = 60, tooltipText = OPTION_TOOLTIP_FARCLIP },
        { text = ENVIRONMENT_DETAIL, func = "WorldDetail", minValue = 0, maxValue = 2, valueStep = 1, tooltipText = OPTION_TOOLTIP_ENVIRONMENT_DETAIL },
        { text = TERRAIN_MIP, func = "TerrainMip", minValue = 0, maxValue = 1, valueStep = 1, tooltipText = OPTION_TOOLTIP_TERRAIN_TEXTURE, restartClient = 1, tooltipRequirement = OPTION_RESTART_REQUIREMENT },
        { text = TEXTURE_DETAIL, func = "BaseMip", minValue = 0, maxValue = 1, valueStep = 1, tooltipText = OPTION_TOOLTIP_TEXTURE_DETAIL },
        { text = GAMMA, func = "Gamma", cvar1 = "gamma", minValue = -0.5, maxValue = 0.5, valueStep = 0.1, tooltipText = OPTION_TOOLTIP_GAMMA },
        { text = ANISOTROPIC, func = "anisotropic", minValue = 1, maxValue = 4, valueStep = 1, tooltipText = OPTION_TOOLTIP_ANISOTROPIC, restartClient = 1, tooltipRequirement = OPTION_RESTART_REQUIREMENT },
        { text = SPELL_DETAIL, func = "spellEffectLevel", minValue = 0, maxValue = 2, valueStep = 1, tooltipText = OPTION_TOOLTIP_SPELL_DETAIL },
        { text = WEATHER_DETAIL, func = "weatherDensity", minValue = 0, maxValue = 3, valueStep = 1, tooltipText = OPTION_TOOLTIP_WEATHER_DETAIL },
    }
end
"#;

/// Every shim, in one list, seeded into `script` — the whole layer, and the whole answer to "what
/// do we add that the reference does not have".
const SHIMS: &[(&str, &str)] = &[("wipe", WIPE), ("OptionsFrameSliders", OPTIONS_FRAME_SLIDERS)];

/// Seed the layer, if this install wants it. Called at the head of every manifest walk, so it lands
/// before any chain file runs and re-lands on the VM a `ReloadUI` mints.
///
/// A failure here is logged and nothing else: a shim that will not compile is our bug, and it must
/// not take the interface down with it — the walk that follows is the thing the player needs.
pub(super) fn install(script: &UiScript) {
    if framexml_source() == FrameXmlSource::Stock {
        return;
    }
    for (name, chunk) in SHIMS {
        if let Err(e) = script.run(chunk) {
            error!("ui_script: FrameXML compat shim `{name}` did not install: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mpq(name: &str) -> PathBuf {
        PathBuf::from(r"G:\tw_experiment\Everwood - Working\Data").join(name)
    }

    /// **A custom patch winning a FrameXML file is what turns the layer on** — the live shape:
    /// Everwood's `patch-9.mpq` (Turtle) owns `UIParent.lua` because the de-dup left no stock copy
    /// under it.
    #[test]
    fn a_custom_patch_winning_framexml_turns_the_shims_on() {
        assert_eq!(
            framexml_source_from(Some(&mpq("patch-9.mpq")), None),
            FrameXmlSource::Chain
        );
        assert_eq!(
            framexml_source_from(Some(&mpq("patch-Z.mpq")), None),
            FrameXmlSource::Chain
        );
    }

    /// **A stock archive winning it leaves the reference bare.** Case-insensitively, because the
    /// mount law matches archive names that way and an install spells them however it likes.
    #[test]
    fn a_stock_archive_winning_framexml_leaves_it_alone() {
        for name in ["patch.MPQ", "patch-2.MPQ", "interface.MPQ", "PATCH.mpq", "Interface.MPQ"] {
            assert_eq!(
                framexml_source_from(Some(&mpq(name)), None),
                FrameXmlSource::Stock,
                "{name} is Blizzard's"
            );
        }
    }

    /// **Only `Interface\FrameXML\…` is judged; the verdict says nothing about any other path.**
    /// `patch-Z.mpq` owning the glue screens or `patch-Y.mpq` owning a race model is the ordinary
    /// chain doing its job — this layer neither knows nor cares, because it adds Lua globals and
    /// re-routes nothing. The assertion that matters is therefore about the PROBE: it is a
    /// FrameXML path, and the one the rule is taken on.
    #[test]
    fn the_verdict_is_taken_on_a_framexml_path_and_nothing_else() {
        assert!(PROBE.starts_with(r"Interface\FrameXML\"));
        assert_eq!(SHIMS.len(), 2, "every shim is listed in SHIMS");
    }

    /// **The environment override wins over the chain, both ways** (`WOW_FRAMEXML=stock|chain`).
    #[test]
    fn the_environment_override_wins_both_ways() {
        assert_eq!(
            framexml_source_from(Some(&mpq("patch-9.mpq")), Some("stock")),
            FrameXmlSource::Stock
        );
        assert_eq!(
            framexml_source_from(Some(&mpq("patch.MPQ")), Some("chain")),
            FrameXmlSource::Chain
        );
        // Anything else is not an override, it is a typo — fall back to the chain's own answer
        // rather than silently picking a side.
        assert_eq!(
            framexml_source_from(Some(&mpq("patch.MPQ")), Some("yes please")),
            FrameXmlSource::Stock
        );
    }

    /// **No probe in the chain reads as a broken install, and gets the shims.** A stock install
    /// cannot be missing `UIParent.lua`, so the alternative would be to leave a partial install
    /// raising nil-globals for a file it does not have either way.
    #[test]
    fn an_absent_probe_gets_the_shims() {
        assert_eq!(framexml_source_from(None, None), FrameXmlSource::Chain);
    }

    /// **`wipe` empties in place and returns the same table** — the 2.0 contract Turtle's FrameXML
    /// calls it under, including the `t == wipe(t)` identity `FriendsFrame.lua` relies on when it
    /// re-uses the table it just cleared.
    #[test]
    fn the_wipe_shim_empties_in_place_and_returns_the_table() {
        let s = UiScript::new().unwrap();
        s.run(WIPE).unwrap();
        assert!(s
            .eval::<bool>(
                "local t = { 1, 2, 3, a = 1 } local r = wipe(t) \
                 return r == t and next(t) == nil"
            )
            .unwrap());
        // A non-table is handed straight back rather than raising: the shim is a hole-filler, and
        // a raise from inside it would be OUR error appearing in the reference's traceback.
        assert!(s.eval::<bool>("return wipe(nil) == nil").unwrap());
    }

    /// **A VM that already has the globals keeps its own.** The seed is a hole-filler; on a stock
    /// install the reference's own `OptionsFrame.lua` runs AFTER it and owns the name outright, and
    /// this is the same property asserted at the one place it is cheap to assert.
    #[test]
    fn the_shims_never_clobber_a_global_that_already_exists() {
        let s = UiScript::new().unwrap();
        s.run("wipe = function() return 'mine' end OptionsFrameSliders = { 'mine' }")
            .unwrap();
        for (_, chunk) in SHIMS {
            s.run(chunk).unwrap();
        }
        assert!(s
            .eval::<bool>("return wipe() == 'mine' and OptionsFrameSliders[1] == 'mine'")
            .unwrap());
    }

    /// **The slider table carries every row our Graphics page indexes**, with the reference's own
    /// bounds — rows 1, 2, 3, 6 and 9 (`assets/ui/OptionsFrame.xml`'s
    /// `BENILLA_GRAPHICS_SLIDER_ROWS`), out of the reference's nine.
    #[test]
    fn the_slider_seed_carries_the_rows_our_graphics_page_reads() {
        let s = UiScript::new().unwrap();
        s.run(OPTIONS_FRAME_SLIDERS).unwrap();
        assert_eq!(
            s.eval::<i64>("return getn(OptionsFrameSliders)").unwrap(),
            9,
            "the reference's nine rows, so `OptionsFrame_Load`'s OptionsFrameSlider1..9 walk is whole"
        );
        for (index, func, min, max, step) in [
            (1, "uiscale", 0.64, 1.0, 0.01),
            (2, "farclip", 177.0, 777.0, 60.0),
            (3, "WorldDetail", 0.0, 2.0, 1.0),
            (6, "Gamma", -0.5, 0.5, 0.1),
            (9, "weatherDensity", 0.0, 3.0, 1.0),
        ] {
            let row = format!("OptionsFrameSliders[{index}]");
            assert_eq!(
                s.eval::<String>(&format!("return {row}.func")).unwrap(),
                func
            );
            for (field, want) in [("minValue", min), ("maxValue", max), ("valueStep", step)] {
                let got: f64 = s.eval(&format!("return {row}.{field}")).unwrap();
                assert!(
                    (got - want).abs() < 1e-9,
                    "row {index} {field}: {got} vs the reference's {want}"
                );
            }
        }
    }
}
