//! Host-shared state: the few plain types every EME crate (the `eme` plugin, the `eme-ui` shell,
//! a host client) must agree on by NAME, so none of them depends on another to read them.
//!
//! With the crate feature `bevy` they derive Bevy's `Resource` / `Message`; without it they are
//! plain structs (and `eme-core` stays engine-free).
//!
//! | Type | Kind | Written by | Read by |
//! |---|---|---|---|
//! | [`EmeActive`] | resource | the `eme` plugin (only after a permitted toggle) | everyone: UI shell, camera, host glue |
//! | [`EmeToggleRequested`] | message | the F9 key handler, a menu button, a script | the `eme` plugin, which applies the rank gate |
//! | [`EmePointerOverUi`] | resource | the UI shell's input arbiter (egui wants the pointer) | the viewport: no picking / brushing under a panel |
//! | [`EmeHostInput`] | resource | the host client (chat edit box focused, pointer over its own UI) | the viewport and the free camera |
//! | [`EmeUiWantsKeyboard`] | resource | the UI shell (an EME text field is focused) | the hotkeys and the free camera |

/// Whether the EME editor is open (free camera, brush, panels). Only the `eme` plugin flips it,
/// and only for a caller whose rank may open EME (`caps.rank ≥ 2`); for anyone else it stays
/// `false` and every EME system is inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::resource::Resource))]
pub struct EmeActive(pub bool);

/// "Toggle EME" (F9, a menu entry, a script). A request: the `eme` plugin honours it only when the
/// caller may open EME, and turns it into an [`EmeActive`] flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::message::Message))]
pub struct EmeToggleRequested {
    /// `Some(on)` forces the state (e.g. Tab = `Some(false)`, back to the character); `None` flips.
    pub to: Option<bool>,
}

/// The pointer is over an EME panel this frame (the UI shell's arbiter writes it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::resource::Resource))]
pub struct EmePointerOverUi(pub bool);

/// What the host client tells EME about its own input each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::resource::Resource))]
pub struct EmeHostInput {
    /// A text field of the host has the keyboard (chat edit box): no camera keys, no hotkeys.
    pub typing: bool,
    /// The pointer is over the host's own UI (chat frame): no picking / brushing.
    pub pointer_over_ui: bool,
}

/// An EME text field (a restore-point label, a typed height) has the keyboard this frame (the UI
/// shell writes it): no camera keys, no hotkeys, no F9 / Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::resource::Resource))]
pub struct EmeUiWantsKeyboard(pub bool);
