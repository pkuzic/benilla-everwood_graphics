# eme-core

The engine-free core of **Everwood ME (EME)**, benilla's in-game, server-authoritative map editor.
No Bevy, no UI toolkit, no networking: plain state machines driven by explicit inputs (time
included), unit-tested with fixtures. Licence: MIT OR Apache-2.0, like the rest of the workspace.

| Module | What |
|---|---|
| `tool` | `EmeTool` trait, `EmeToolRegistry`, the input / options / preview models |
| `command` | `EmeCommand` (voxel stroke, overlay batch, service request, object op), `EmeCommandSink` |
| `bus` | `EmeDispatcher`: gates commands by rank, sends them, records history, routes replies |
| `undo` | `EmeUndoStack`: one Ctrl+Z for terrain strokes, overlay strokes and object transactions |
| `stroke` | `EmeStrokeBuilder`: a held brush → one stroke id of ≤ 8 edits/s, each with a nonce |
| `transport` | `EmeTransport` trait, `EmeVoxelEdit`, `EmeInbound`, `EmeLoopbackTransport` for tests |
| `service` | the `.mveme` JSON exactly as CONTRACT v3 §12–§13 (`voxel-v2/CONTRACT.md`): requests, replies, pushes, typed results |
| `perm` | rank → capability: 2 read-only, 3 mutate, 4 revert others |
| `api` | the public integration surface: `EmeEvent(s)`, `EmeHistoryHook`, `EmeServiceClient` |

## The frame loop (what a host does)

```rust
registry.feed(&ctx, dispatcher.caps(), &mut sink);       // active tool → commands
dispatcher.dispatch(now_ms, &mut transport, &mut sink);  // gate, send, record history
let out = dispatcher.pump(now_ms, &mut transport);       // confirms, replies, pushes, timeouts
// Ctrl+Z / Ctrl+Y:
dispatcher.undo(now_ms, &mut transport);
dispatcher.redo(now_ms, &mut transport);
```

Undo is a **server request**, not an inverse edit: the server reverts the stroke's rows and every
client resyncs. Entries therefore move through server-confirmed states (`Open → Committing →
Applied → UndoPending → Undone → RedoPending → Applied`, with `NeedsConfirm` when another GM edited
the area afterwards). See the `undo` module docs for the full machine.

## Projects (CONTRACT v3.4) and the service gate (v3.5)

When `caps` carries `projects_ready` the dispatcher refuses every map edit (voxel strokes, overlay
batches, object ops) with `EmeSendError::NoProject` until a project is active. It tracks the active
project from `caps.project`, the `project_*` replies, `R 7` / `no_project` (cleared, caps re-asked,
`EmePumpOutput::no_project` set so the host prompts) and the `project_changed` push, stamps every
undo entry with it (`EmeUndoEntryMeta::project`) and drops a reverted project's entries. The project
the user opened is re-opened after every fresh `caps` (HELLO / map change / reconnect start a new
server selection). The host must not send any `.mveme` before the server's `K` record said rank ≥ 2
(the `eme` crate's `EmeWire::may_use_service`).

## Adding a tool

1. One file, `crates/eme/src/tools/<name>.rs`, with a type implementing `EmeTool`:
   - `id()` — a stable lowercase id (`"sculpt"`). It is also the server's name for the tool in
     `caps.tools` and the locale key prefix `MONKEY_EME_<ID>_*`.
   - `min_rank()` — `3` for anything that changes the world, `2` for read-only tools.
   - `on_input(ctx, out)` — read the `EmeInputCtx` snapshot, push `EmeCommand`s into `out`. Never
     touch the network, the voxel journal or the UI toolkit; tools never reference each other.
     A brush tool owns an `EmeStrokeBuilder` and emits `EmeCommand::VoxelStroke` with the
     builder's edits; on release it sends `EmeStrokeMark::End { trailing }` and keeps calling
     `builder.tick(now)` on later frames so a rate-limited release edit still goes out. Every tool that
     opens strokes (brushes and overlay tools alike) takes its stroke ids from one shared
     `EmeStrokeIds` counter, since the server numbers a character's strokes in one sequence.
   - `options_ui_model()` / `set_option()` — declarative option fields; the UI renders them.
   - `preview(ctx)` — a ring, plane, outline or cell tint for the viewport to draw.
2. One registration line: `app.register_eme_tool(MyTool::new())` (the Bevy plugin forwards to
   `EmeToolRegistry::register`). Registration order is toolbar order.
3. Locale keys `MONKEY_EME_<ID>_*` in both `enUS` and `ruRU`.
4. Tests: drive the tool with `EmeInputCtx` frames and an injected clock, feed its commands to an
   `EmeDispatcher` over `EmeLoopbackTransport`, and play the server's part by delivering
   `EmeInbound` confirmations / replies (see `tests/bus.rs`, which carries a complete sculpt tool).

The toolbar shows a tool only when the caller's rank reaches `min_rank()` **and** the server lists
the id in `caps.tools`. Client gating is cosmetic; the server enforces every permission.
A tool built purely on another tool's wire (MONKEY v3.6: `pushpull` / `bridge` send sculpt edits,
`waterfill` sends water cells) overrides `server_tool()` to name that one, so it is offered with it.

Optional hooks (MONKEY v3.6): `on_terrain(ctx, &dyn EmeTerrain)` runs on the active tool before
`on_input` when the host passes terrain queries (`feed_with_terrain`; the ground height under a
point), `take_notice()` hands the UI a notice line, and `EmeInputCtx` carries `view_dir` (camera ray)
plus one-shot `cancel` (Esc) / `confirm` (Enter).

## Integrating from another program

Use only `api` (`EmeEvent`, `EmeHistoryHook`, `EmeServiceClient`) and the wire types in `service`.
A host mirrors EME's history into its own journal by registering an `EmeHistoryHook` on the undo
stack; hooks see server-confirmed transitions only (push, commit, undo, redo, discard).

## Tests

`cargo test -p eme-core`. Example wire frames live in `tests/fixtures/` (`requests.json`,
`replies.json`, `pushes.json` are the CONTRACT v3 examples, round-tripped; `replies_lua.json` holds the shapes a Lua
JSON encoder emits — `{}` for an empty list, `0`/`1` for flags — which the types accept).
