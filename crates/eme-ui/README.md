# eme-ui

The UI shell of **Everwood ME (EME)**: egui panels (via `bevy_egui 0.39`) over `eme-core`. It holds
no tool logic, networking or voxel code. It reads resources and writes `EmeUiCommand` messages.
Licence: MIT OR Apache-2.0.

Layout: top tool strip (tools, Undo/Redo, link and rank), left panel = active tool's options,
right panel = History / Restore points / Log tabs, bottom status bar, a "needs confirmation"
modal for `others_edited`, and a read-only banner for rank 2.

## Embedding

```rust
app.add_plugins(eme_ui::EmeUiPlugin)           // adds EguiPlugin itself if absent
   .insert_resource(eme_ui::EmeLayoutPath(Some("benilla-config/eme-layout.toml".into())))
   .insert_resource(eme_ui::EmeLanguage(eme_ui::EmeLang::from_locale("ruRU")));
```

The host (another crate) must:

* spawn the bevy_egui primary context camera (`PrimaryEguiContext`);
* register tools into `ResMut<EmeTools>` (`.0` is the `EmeToolRegistry`);
* keep current: `EmeSession` (perms from the `caps` rank, `caps`, `link`), `EmeStatus`,
  `EmeRestorePoints`, `EmeLogTable`, and `EmeUndo` (`.0` is the `EmeUndoStack`);
* set `EmeActive(true)` to show the shell (or write `EmeToggleRequested`, which flips it, and only
  opens for ranks that may open EME);
* read `MessageReader<EmeUiCommand>` and apply: `ActivateTool` -> registry `activate`, `Undo`/`Redo`
  -> the stack, `ConfirmUndo`/`CancelConfirm` -> `confirm_undo`/`cancel_confirm`, `SetOption` ->
  `tool.set_option`, `Service` -> send the request, `LogPage`/`TabOpened` -> refresh data;
* before the viewport acts on a click, check `EmeInputArbiter` (`pointer_over_ui`,
  `keyboard_captured`).

Everything visible goes through `EmeI18n` (`locales/enUS.json`, `locales/ruRU.json`, keys
`MONKEY_EME_*`). Extra keys: `EmeI18n::extend_json(lang, json)`.

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
