//! MONKEY (EME): the host glue between benilla and the EME editor plugin (feature `eme`).
//!
//! - adds [`eme::EmePlugin`] (F9 for server rank ≥ 2, free camera, picking, brush preview,
//!   strokes through the voxel seam, and the `eme-ui` shell fed from `EmeHost`) — its chat
//!   transport is `net::eme_relay`;
//! - puts the shell on the shared egui lane (`crate::egui_lane`: one `EguiPlugin`, one overlay
//!   camera, shared with the dev panel) and keeps that lane running while EME is open;
//! - the shell's language from the client locale (`ruRU` → Russian), `WOW_EME_LANG` overriding;
//!   its panel layout persists in `benilla-config/eme-layout.toml`;
//! - hides the stock player UI while EME is open the way Alt+Z does (`UIParent:Hide()`), the chat
//!   lifted onto `WorldFrame` first so it stays, and puts both back on close. `UIParent:Show()`
//!   fires the stock OnShow re-layout (the pet bar slides up, the chat with it), so every managed
//!   frame's anchors are recorded before the hide and put back after the show:
//!   close restores EXACTLY the pre-open layout;
//! - tells EME about benilla's own input each frame ([`eme_core::EmeHostInput`]: an EditBox has
//!   the keyboard, the pointer is over the player UI);
//! - holds the character still while EME is open: `player.detached` (the mover is parked, the
//!   world streams around the camera, server moves / teleports still apply), restored on close;
//! - while EME is open the LEFT button is the brush's, not the world's (no orbit drag, no click
//!   targeting): `player::camera::latch_world_mouse` reads [`eme_core::EmeActive`]. A press over
//!   an EME panel never reaches the world either: the shell's hover feeds `EguiPointerOver`, hence
//!   `PointerOverUi`.

use bevy::prelude::*;

use benilla_ui::script::UiScript;
use eme::eme_ui::{EmeLang, EmeLanguage, EmeLayout, EmeLayoutPath, EmeShell};
use eme_core::{EmeActive, EmeHostInput};

pub(crate) struct EmeHostPlugin;

impl Plugin for EmeHostPlugin {
    fn build(&self, app: &mut App) {
        // The lane first, so `EmeUiPlugin` finds `EguiPlugin` added and keeps this camera setup.
        crate::egui_lane::ensure(app);
        app.add_plugins(eme::EmePlugin)
            .add_systems(Startup, eme_shell_setup)
            .add_systems(
                Update,
                (
                    eme_host_input.before(eme::EmeSet::Input),
                    eme_hold_character,
                    eme_egui_demand
                        .after(eme::EmeSet::Frame)
                        .in_set(crate::egui_lane::EguiDemandSet),
                    eme_hide_player_ui.after(crate::ui_script::UiInput),
                ),
            );
    }
}

/// The shell's language and layout file.
fn eme_shell_setup(
    mut lang: ResMut<EmeLanguage>,
    mut path: ResMut<EmeLayoutPath>,
    mut shell: ResMut<EmeShell>,
) {
    lang.0 = eme_lang();
    info!("EME: shell language {:?}", lang.0);
    path.0 = crate::config_dir().map(|d| d.join("eme-layout.toml"));
    if let Some(p) = &path.0 {
        shell.layout = EmeLayout::load(p);
    }
}

/// `WOW_EME_LANG` (`ruRU` / `enUS`), else the client locale's configured tag (`Wow.ini`'s
/// `Language` + `Country`, kept even where the index table lacks it, as it lacks `ruRU`).
fn eme_lang() -> EmeLang {
    if let Ok(v) = std::env::var("WOW_EME_LANG") {
        return EmeLang::from_locale(v.trim());
    }
    let tag = crate::ui_script::client_locale().tag().to_be_bytes();
    EmeLang::from_locale(&String::from_utf8_lossy(&tag))
}

/// Keep the egui lane awake while EME is open.
fn eme_egui_demand(active: Res<EmeActive>, mut demand: ResMut<crate::egui_lane::EguiLaneDemand>) {
    if demand.eme != active.0 {
        demand.eme = active.0;
    }
}

/// Hide the stock UI the way Alt+Z does (`UIParent:Hide()`, no per-frame Hide / Show, so no
/// OnShow re-layout of the bars), but first lift the chat (`ChatFrame1..7`, their tabs, the edit
/// box and the menu button) onto `WorldFrame` at the same effective scale, so it stays; on close
/// everything goes back to `UIParent` and `UIParent` shows. 1.12 Lua: no `#`.
const HIDE_PLAYER_UI: &str = r#"
if not MonkeyEmeHidden and UIParent and WorldFrame then
  MonkeyEmeHidden = {}
  local s = UIParent:GetScale() or 1
  local function lift(f)
    if f and f.GetParent and f:GetParent() == UIParent then
      local sc = f:GetScale() or 1
      table.insert(MonkeyEmeHidden, { f = f, scale = sc })
      f:SetParent(WorldFrame)
      f:SetScale(sc * s)
    end
  end
  for i = 1, 7 do
    lift(getglobal("ChatFrame" .. i))
    lift(getglobal("ChatFrame" .. i .. "Tab"))
  end
  lift(getglobal("ChatFrameEditBox"))
  lift(getglobal("ChatFrameMenuButton"))
  MonkeyEmeUiWasShown = UIParent:IsShown()
  -- Record the layout the stock re-layout on Show would move (anchors).
  MonkeyEmeLayout = {}
  local names = {
    "PetActionBarFrame", "ShapeshiftBarFrame", "BonusActionBarFrame", "MainMenuBar",
    "MultiBarBottomLeft", "MultiBarBottomRight", "MultiBarLeft", "MultiBarRight",
    "CastingBarFrame", "BuffFrame", "TemporaryEnchantFrame", "MinimapCluster",
    "ChatFrameMenuButton", "ChatFrameEditBox",
    "MainMenuBarArtFrame", "MainMenuBarBackpackButton", "ExhaustionTick",
  }
  for i = 1, 7 do
    table.insert(names, "ChatFrame" .. i)
    table.insert(names, "ChatFrame" .. i .. "Tab")
  end
  if UIPARENT_MANAGED_FRAME_POSITIONS then
    for k, _ in pairs(UIPARENT_MANAGED_FRAME_POSITIONS) do
      if type(k) == "string" then table.insert(names, k) end
    end
  end
  local seen = {}
  for _, n in ipairs(names) do
    if not seen[n] then
      seen[n] = true
      -- A global of that name may be anything (a number, a table): never let one abort the hide.
      pcall(function()
        local f = getglobal(n)
        if f and f.GetNumPoints and f.IsShown then
          local rec = { f = f, pts = {} }
          for i = 1, f:GetNumPoints() do
            local p, r, rp, x, y = f:GetPoint(i)
            table.insert(rec.pts, { p, r, rp, x, y })
          end
          table.insert(MonkeyEmeLayout, rec)
        end
      end)
    end
  end
  UIParent:Hide()
end
"#;

const SHOW_PLAYER_UI: &str = r#"
if MonkeyEmeHidden then
  for _, e in ipairs(MonkeyEmeHidden) do
    pcall(e.f.SetParent, e.f, UIParent)
    pcall(e.f.SetScale, e.f, e.scale)
  end
  MonkeyEmeHidden = nil
  if MonkeyEmeUiWasShown then UIParent:Show() end
  -- The stock OnShow re-layout just ran: put every recorded frame back exactly as it was.
  if MonkeyEmeLayout then
    for _, rec in ipairs(MonkeyEmeLayout) do
      local f = rec.f
      pcall(function()
        if table.getn(rec.pts) > 0 then
          f:ClearAllPoints()
          for _, pt in ipairs(rec.pts) do
            f:SetPoint(pt[1], pt[2], pt[3], pt[4], pt[5])
          end
        end
      end)
    end
    MonkeyEmeLayout = nil
  end
end
"#;

/// The stock UI hides while EME is open (the chat stays), and comes back on close.
fn eme_hide_player_ui(
    active: Res<EmeActive>,
    script: Option<NonSendMut<UiScript>>,
    // Keyed on the UI session: a reloaded VM starts with everything shown, so it is hidden anew.
    mut memo: Local<crate::ui_script::VmMemo<bool>>,
) {
    let Some(script) = script else {
        return;
    };
    let hidden = memo.get(&script);
    if active.0 == *hidden {
        return;
    }
    let chunk = if active.0 {
        HIDE_PLAYER_UI
    } else {
        SHOW_PLAYER_UI
    };
    if let Err(e) = script.run(chunk) {
        warn!(
            "EME: player UI {}: {e}",
            if active.0 { "hide" } else { "show" }
        );
    }
    *hidden = active.0;
}

/// benilla's input state → EME.
fn eme_host_input(
    capture: Option<Res<crate::ui_script::UiKeyboardCapture>>,
    hover: Option<Res<crate::ui_script::PlayerUiHover>>,
    mut input: ResMut<EmeHostInput>,
) {
    // The PLAYER-UI half only (the chat frame); EME's own panels arrive as `EmePointerOverUi`.
    let next = EmeHostInput {
        typing: capture.is_some_and(|c| c.typing),
        pointer_over_ui: hover.is_some_and(|o| o.0.is_some()),
    };
    if *input != next {
        *input = next;
    }
}

/// Detach the eye from the body while EME is open; give it back on close.
fn eme_hold_character(
    active: Res<EmeActive>,
    player: Option<ResMut<crate::player::Player>>,
    mut held: Local<Option<bool>>,
) {
    let Some(mut player) = player else {
        return;
    };
    match (active.0, *held) {
        (true, None) => {
            *held = Some(player.detached);
            player.detached = true;
        }
        (false, Some(was)) => {
            player.detached = was;
            *held = None;
        }
        _ => {}
    }
}
