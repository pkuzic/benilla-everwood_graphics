//! MONKEY (EME): the host glue between benilla and the EME editor plugin (feature `eme`).
//!
//! - adds [`eme::EmePlugin`] (F9 for server rank ≥ 2, free camera, picking, brush preview,
//!   strokes through the voxel seam) — its chat transport is `net::eme_relay`;
//! - tells EME about benilla's own input each frame ([`eme_core::EmeHostInput`]: an EditBox has
//!   the keyboard, the pointer is over the player UI);
//! - holds the character still while EME is open: `player.detached` (the mover is parked, the
//!   world streams around the camera, server moves / teleports still apply), restored on close;
//! - while EME is open the LEFT button is the brush's, not the world's (no orbit drag, no click
//!   targeting): `player::camera::latch_world_mouse` reads [`eme_core::EmeActive`].

use bevy::prelude::*;

use eme_core::{EmeActive, EmeHostInput};

pub(crate) struct EmeHostPlugin;

impl Plugin for EmeHostPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(eme::EmePlugin).add_systems(
            Update,
            (
                eme_host_input.before(eme::EmeSet::Input),
                eme_hold_character,
            ),
        );
    }
}

/// benilla's input state → EME.
fn eme_host_input(
    capture: Option<Res<crate::ui_script::UiKeyboardCapture>>,
    over: Option<Res<crate::ui_script::PointerOverUi>>,
    mut input: ResMut<EmeHostInput>,
) {
    let next = EmeHostInput {
        typing: capture.is_some_and(|c| c.typing),
        pointer_over_ui: over.is_some_and(|o| o.0),
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
