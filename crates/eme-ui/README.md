# eme-ui

The UI shell of **Everwood ME (EME)**: egui panels (via `bevy_egui 0.39`) over `eme-core`. It holds
no tool logic, networking or voxel code. It reads resources and writes `EmeUiCommand` messages.
Licence: MIT OR Apache-2.0.

Layout: top tool strip (tools, Undo/Redo, link and rank), left panel = active tool's options,
right panel = History / Restore points / Log tabs, bottom status bar, a "needs confirmation"
modal for `others_edited`, and a read-only banner for rank 2.

## Embedding

With the `eme` plugin (the normal case) nothing is embedded by hand: `eme::EmePlugin` adds
`EmeUiPlugin::<eme::EmeHost>`, so the shell reads THE tool registry and THE undo stack of
`EmeHost` (via the `EmeShellSource` trait), and `eme::ui` turns every `EmeUiCommand` into an
`eme::EmeRequest` / service request and fills the restore-point, log, status and session
resources. The host only:

* adds `EguiPlugin` itself if it wants its own egui camera (benilla does: one overlay camera,
  `PrimaryEguiContext`, shared with its dev panel) — else `EmeUiPlugin` adds the plugin;
* keeps the primary egui context running while `EmeActive` is on;
* sets `EmeLanguage` (benilla: the client locale, `ruRU` → Russian) and `EmeLayoutPath`.

Without the `eme` plugin (tests, a demo), use `EmeUiPlugin::<EmeStandaloneSource>` and keep the
registry / undo stack in that resource, `EmeSession`, `EmeStatus`, `EmeRestorePoints` and
`EmeLogTable` current yourself, and apply the `EmeUiCommand`s.

The shared names are eme-core's (`eme_core::host`, feature `bevy`): `EmeActive` (the shell draws
while it is on; whoever owns it applies the rank gate — `EmeToggleRequested` is a request to that
owner), and the two input flags the shell writes, `EmePointerOverUi` (the viewport must not pick or
brush) and `EmeUiWantsKeyboard` (no hotkeys or camera keys).

The toolbar is `EmeToolRegistry::visible(perms, caps)`: a tool the rank may not use, or the server
does not offer, is not shown at all.

Everything visible goes through `EmeI18n` (`locales/enUS.json`, `locales/ruRU.json`, keys
`MONKEY_EME_*`). Extra keys: `EmeI18n::extend_json(lang, json)`.

## Panels the host fills (restore points, log, notices, other GMs)

* `EmeRestorePoints { rows, selected }`: the Restore tab lists `rows`; clicking a row writes
  `EmeUiCommand::SelectRestore(id)` and the host outlines that point's radius on the ground (the
  `eme` plugin does: `EmeRestoreOutline`). "Go to" = `SelectRestore` + `ShowInWorld`. "Revert" does
  NOT send anything: it opens a confirmation (`EmeShell::pending`), and only its "Revert" button
  writes `Service(Revert)`.
* Log tab: the filter fields (`EmeShell::filter`, turned into an `EmeLogQuery` by
  `EmeLogFilterUi::query`) emit `EmeUiCommand::LogQuery(q)`; the host sends page 1 of `q` and keeps
  `q` for paging. Clicking a row's name fills the player filter; "Undo this player's edits" (rank 4)
  confirms, then writes `Service(UndoPlayer { name, minutes, radius })`. A row's "Go to" writes
  `ShowInWorld { x, y, z }` (the host jumps the camera and marks the spot).
* `EmeNotice { key, args, error }`: one line under the top strip (a refused service call shows
  the server's reason; a revert shows how many rows it undid). The host sets it, the user's
  "Dismiss" writes `DismissNotice`, the host expires it.
* `EmeOthersHistory`: strokes other GMs made (from the live `e` records), shown read-only under the
  History tab, each with a "Go to".

## Contributing a tool's options panel

You do not write egui. Implement `EmeTool::options_ui_model()` returning `EmeOptionField`s
(`Slider`, `Number`, `Toggle`, `Choice`, `Text`, `Button`) and `set_option(key, value)`. The shell
draws them in the left panel when the tool is active, and emits `SetOption`. Rules:

* `label` (and each `Choice` label) is a locale key, `MONKEY_EME_<TOOL>_<KEY>`; add it to both
  locale files via `extend_json`. The toolbar button text is `MONKEY_EME_<TOOL>_NAME` (falls back
  to the tool id).
* `min_rank() >= 3` marks a mutating tool: its controls are disabled for read-only ranks.
* `disabled: true` greys out a single field.

## Tests

`cargo test -p eme-ui`: headless layout tests run the shell on a bare `egui::Context` with
`EmeShell::probe = true`, which records every drawn control (id, enabled, rect, tooltip) and lets
tests synthesize clicks. Locale tests check both files have identical keys and placeholders and
that every key literal in `src/` exists.
