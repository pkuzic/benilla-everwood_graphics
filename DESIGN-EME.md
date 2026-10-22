# DESIGN — Everwood ME (EME), the in-game GM map editor (open source)

**Status:** design only, rev 2 (2026-10-22). Nothing here is implemented. · **Owner decisions folded in:**
EME is an **independent, OPEN-SOURCE** project; Monkey Studio is CLOSED SOURCE and is not a dependency.
EME and the voxel terrain are both FINISHED before the voxel line merges into the main Everwood client.
**Binding rules:** `G:\mangos\tortoise-wow\CLAUDE.md` (Monkey naming, EN+RU, COMMANDS.md / LOGS.md /
changelog.txt, rank-hiding convention, SQL applied by the conductor only, chat = plain system text).
**Inputs read:** `DESIGN-VoxelTerrain.md`, `WATER.md`, `G:\mangos\disposable\voxel-v2\CONTRACT.md`,
`voxtest-server\lua_scripts\MonkeyVoxelServer.lua`, `tortoise-wow\bin\Debug\lua_scripts\BuildMode.lua`,
benilla `crates/benilla-world/src/{voxel.rs,liquid/}` and `crates/benilla-app/Cargo.toml`.

---

## 0. Ground rules (the licence firewall)

1. **No Studio code, ever.** EME must not depend on, link to, vendor, or copy from the `monkey-studio`
   crate (benilla branch `everwood-studio`) or the server branch `monkey-studio` / `gmide`. Studio was
   read only to learn what a good editor needs; EME re-implements every idea from a blank file. Workers
   must not open Studio sources while writing EME code (brief them with this doc, not with Studio paths).
2. **Interop is one-directional:** Studio (closed) MAY integrate with EME through EME's **public API**
   (§2.5: `EmeHistoryHook`, `EmeEvents`, the service wire). EME never imports, detects or names Studio.
3. **Branching:** EME lives on the open voxel/EME line — branch `everwood-eme` cut from
   `everwood-voxel_world` (worktree `G:\mangos\disposable\benilla-voxel-world`). The earlier plan
   `everwood-eme = everwood-studio + voxel_world` is **dropped**. This line has no MonkeyNet and no
   `ext/`; EME must build and run here with the chat transport alone.
4. **Server side is Everwood's, but EME's protocol is public.** The server scripts that implement the
   EME wire (`MonkeyVoxelServer.lua`, new `MonkeyEme*.lua`) are published WITH the EME repo as the
   reference server (they contain no secrets: no keys, no gate digests, no OVH details). Everwood-only
   policy (fief rules, `FiefBuildGuard`, `BuildMode.lua` internals) stays in the Everwood repo and is
   reached through a thin documented adapter (§5.4).
5. Third-party credit rules already in the fork stay (WarcraftXL water code: author + project named,
   listed in `THIRD-PARTY.md`); EME adds no new third-party code without a `THIRD-PARTY.md` row.

## 1. Scope — pieces, and the EME tool set

### 1.1 Existing pieces EME builds on (never duplicate)

| Piece | Where | EME treatment |
|---|---|---|
| Voxel world layer v2 (seed, carve, journal, mesher, collider, material) | `crates/benilla-voxel/src/{world,wire,lib}.rs`, `crates/benilla-world/src/voxel.rs`, `voxel/{material,terrain_hide}.rs` | Terrain back end, reached only through `MonkeyVoxelApi` (§2.3). The key brush `[ ] \ - =` becomes the `voxel_dev_brush` fallback. |
| Voxel authority | `MonkeyVoxelServer.lua` (`.mvox` HELLO/SYNC/EDIT, `.voxel log/snapshot/snapshots/revert/undo`), `sql/custom/voxel_server.sql` | Extended (v3 EDIT, strokes, service ops) — not replaced. |
| Liquid renderer + queries | `crates/benilla-world/src/liquid/{surface,query,waves,drift,mirror,lod,real_data,...}.rs` (5.3k lines), `WATER.md` | Rendering/swim queries reused; EME adds an **override layer** in front of ADT liquid (§4). |
| Legacy Build Mode (gossip + MonkeyBuildUI/ObjectBrowser addons, `gameobject_placed_by`, `gameobject_log`, `FiefBuildGuard`) | Everwood repo `BuildMode.lua`, `addons/Monkey{BuildUI,ObjectBrowser}` | Server truth for object placement (§5.4). Not ported. |
| Noggit Red (`G:\noggit-red-master`) | offline | Still the editor for ADT heightmap, splat, holes, WDT, minimap and the **baseline** liquid/area bytes. EME never writes ADTs; everything it adds is an overlay on top of them. |

### 1.2 Tool set by priority (rank column: all mutating tools = GM 3+)

| # | Tool | Tier | What it does | Needs |
|---|---|---|---|---|
| T1 | **Sculpt** dig / build / smooth | MVP | Brush on real ground; radius 0.5–16 yd, strength, falloff ring | exists (modes 0–2); EME UI + stroke ids (v3 EDIT) |
| T2 | **Flatten / level** | MVP | Pull density toward a horizontal plane (click-sampled or typed z) | mode 3 in `benilla-voxel` (fixed-point, deterministic) |
| T3 | **Undo / redo** (own strokes) | MVP | Ctrl+Z / Ctrl+Y per stroke | v3 `stroke`, `voxel_stroke`, service ops (§3.2) |
| T4 | **History** panel | MVP | My strokes + others' nearby, live | service LOG / STROKES |
| T5 | **Restore points** | MVP | Create / list / revert-to / preview outline | exists as `.voxel snapshot…`; EME needs machine-readable ops |
| T6 | **Log browser** (anti-grief) | MVP | Filter player/time/radius/tool, highlight in world, "undo this player here" | service LOG, UNDO_PLAYER |
| T7 | **Objects** (place / move / rotate / scale / delete NPC+GO) | MVP | EME's own selection + gizmo + placement, server-authoritative (§5) | EME object wire → Build Mode adapter |
| T16 | **Water** edit (raise/lower level, paint/erase pool, set liquid kind) | MVP-lite (§4.4) | Pools in dug pits, rivers' level | liquid overlay (§4) |
| T17 | **Area/zone** paint (area id brush, flag preview) | MVP-lite | Re-zone a polygon/brush region, read-only flag viewer | area overlay (§4) |
| T8 | Brush primitives (box, cylinder, ramp) | Later | Hard-edged builds | `shape` in `param`, 2-point EDIT2 |
| T9 | **Paint material** on voxels | Later | Per-voxel material id; **default (Q8): palette = ADT tileset layers** of the tile | sparse `u8` grid per chunk, `monkey_voxel.wgsl` layer pick, mode 4 |
| T10 | Noise / roughen, erode | Later | Natural cliffs | mode 5, seeded hash |
| T11 | Copy / paste / stamp | Later | Clone a feature | client expands to bounded EDIT list |
| T12 | Vertex tint (MCCV-like) | Later | Darken cave mouths | shares T9 storage |
| T13 | Server collision + NPC navmesh over voxels | Merge prerequisite, not an EME tool | `DynamicMapTree` model per chunk, mmap regen | DESIGN-VoxelTerrain "Server half" |

## 2. Modular architecture

### 2.1 Crates (all open, on the voxel line)

```
benilla-voxel          (exists, engine-free)  lattice, VoxelEditOp, MapJournal, mesher, wire v2 (+v3)
eme-core               (NEW, engine-free)     tools, command bus, history, transport trait, service JSON,
                                              overlay data (liquid/area), undo stack, selection model
eme-ui                 (NEW, Bevy+egui)       window framework, theme, panels, i18n loader   [feature eme]
eme                    (NEW, Bevy plugin)     EmePlugin: F9 toggle, free camera, picking, gizmo, brush preview,
  └─ src/tools/<tool>.rs   one module per tool, registered, never referencing each other
benilla-world::voxel   (exists)               MonkeyVoxelPlugin + MonkeyVoxelApi seam (§2.3)
benilla-world::liquid  (exists)               + `LiquidOverlay` resource hook (§4.2)
benilla-app            (exists)               feature `eme`; net/eme_relay.rs (chat transport)
```
`eme-core` has no Bevy/egui (unit-testable with fixtures). Crate names stay `Monkey`-prefixed in code
symbols (`MonkeyEmePlugin`, `MONKEY_EME_*` locale keys, `MONKEYEME` addon prefix); crate/directory names
may be plain because the repo is public and reusable.

### 2.2 Core types (`eme-core`)

```rust
pub trait EmeTool: Send + Sync + 'static {
    fn id(&self) -> EmeToolId;                 // "sculpt","flatten","water","area","objects",…
    fn min_rank(&self) -> u8;                  // 3 for every mutating tool; 2 for read-only ones
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink);
    fn options_ui_model(&self) -> EmeOptions;  // declarative fields; eme-ui renders them
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview>;   // ring, plane, outline, area tint
}
pub enum EmeCommand {
    VoxelStroke { stroke: u16, ops: Vec<VoxelEditOp> },   // hot path
    Overlay(EmeOverlayEdit),                              // liquid / area (§4), versioned like strokes
    Service(EmeServiceRequest),                           // cold path, JSON request/reply
    Object(EmeObjectRequest),                             // §5
}
pub trait EmeTransport { fn send_edit(&mut self,&VoxelEditOp); fn send_service(&mut self,&EmeServiceRequest)
    -> EmeRequestId; fn poll(&mut self) -> Vec<EmeInbound>; fn state(&self) -> EmeLink; }
pub struct EmeUndoStack { entries: Vec<EmeUndoEntry>, cursor: usize }  // §3.3, ONE stack for everything
```
- **Command bus:** tools only emit `EmeCommand`s; one dispatcher routes them. A tool never touches the
  network, the journal or egui.
- **Adding a tool** = one file `eme/src/tools/<name>.rs` + one `app.register_eme_tool(..)` line + locale
  keys `MONKEY_EME_<NAME>_*`. The toolbar is built from the registry, filtered by `caps` (§3.4).
- **Strokes:** `EmeStrokeBuilder` (core) owns CONTRACT §3 coalescing (≤ 8 EDIT/s, move ≥ r/2 or 125 ms),
  lifted out of `monkey_voxel_brush_edit`.

### 2.3 The voxel seam (`benilla-world::voxel`)

`MonkeyVoxelApi` SystemParam: `aim(ray)` (existing raymarch), `submit(op)` (local prediction + push to
`MonkeyVoxelNet.outbox`), `journal(map)`, `region_state(map,rx,ry)`, `brush_preview(&EmePreview)`.
`voxel_dev_brush` stays default ON only without feature `eme`, so brushes never fight over input.
`MonkeyVoxelNet` remains the queue boundary: any transport only fills `inbox/confirms/resets/region_done`
and drains `outbox`.

### 2.4 Transport abstraction

| Impl | Where | Edit (hot) | Service/overlay/object (cold) |
|---|---|---|---|
| `EmeChatTransport` (**this line**) | `benilla-app/src/net/eme_relay.rs` | `.mvox 02…` SAY (v3) | `.mveme <base64url JSON>` ↔ `MONKEYEME` addon whispers, chunked `seq/part/total` |
| `EmeMonkeyNetTransport` | added **at merge time** in mainline `ext/`, behind cargo feature `eme-monkeynet`, written from scratch against the MonkeyNet API — **no Studio code** | `.mvox` via MonkeyNet `cmd` | JSON on a MonkeyNet service named `eme` |

Selected once per session from `EmeLink`. JSON schema identical on both, so the server has ONE handler
(`MonkeyEme.lua`). The EME repo ships only the chat impl and the trait; the MonkeyNet adapter is an
Everwood-repo file.

### 2.5 Public integration API (what Studio, or anyone, may use)

`eme-core` exports: `EmeEvents` (stroke committed/undone, overlay edited, selection changed — Bevy
events), `EmeHistoryHook` (a trait: `fn on_push(&self, &EmeUndoEntryMeta)`, `fn on_undo(..)` so a host
may mirror EME actions into its own journal / action log), and `EmeServiceClient` (send a service
request, get the reply). Stable, semver-tagged, documented in `docs/EME-API.md`. Nothing in EME
references a host. The optional shared-history behaviour (host Ctrl+Z arbitration) is entirely the
host's job through `EmeHistoryHook`.

### 2.6 Distribution and feature flag

```
benilla : eme = ["benilla-app/eme", "voxel"]          # default ON for the Everwood client
benilla-app: eme = ["dep:bevy_egui", "dep:eme", "dep:eme-ui"]
```
EME is **compiled into the normal client exe** and opens with **F9** (Studio, when present, is F10).
For a non-GM it is invisible and inert: no key handler action, no window, no toolbar; the F9 handler
returns until the server `caps` reply says rank ≥ 2 (§3.4). **The server enforces every permission
regardless** — client gating is cosmetic. The cargo feature exists so a fork can compile EME out
(`--no-default-features`); `gates.sh` checks `eme` is absent from `cargo tree -e features` then. A
client that fails/lacks the HELLO-ACK v3 shows nothing even for a GM.

## 3. Wire, server, permissions

### 3.1 EDIT v3 (`.mvox`), additive — v2 stays accepted

`0x02 EDIT3: op u8, nonce u16, stroke u16, map u16, x,y,z,radius f32, mode u8, strength u8, param u16`
= 27 B. Downlink `'e' (0x65)`: `t u8, seq u32, nonce u16, stroke u16, map u16, x,y,z,radius f32, mode u8,
strength u8, param u16, actor_guid u32` = 35 B. HELLO `client_version = 3` → ACK `server_version = 3`
selects EDIT3; a v2 server stays on v2 (no strokes). Modes: 0 dig, 1 build, 2 smooth, **3 flatten**
(plane z = pos.z + param/100), 4 paint (param = material), 5 roughen (param = seed). Unknown mode →
`R 4`. Every mode bit-identical on all clients (fixed-point, vectors in `benilla-voxel/tests/`).

### 3.2 Undo/redo under server authority

- Voxel state = **seed + every non-reverted edit in seq order** (CONTRACT §1), so undo is NOT an inverse
  edit (CSG is lossy, smooth not invertible). **Undo = revert the stroke's rows** via the existing
  revert machinery: set `reverted_at/by/revert_id`, bump touched region epochs, send `X`, clients
  resync. **Redo** = clear that `revert_id`, bump epochs, `X`. A new stroke clears the redo list.
- **Q4 default (assumed, owner may change): WARN, don't block.** If another GM edited the area after my
  stroke, Ctrl+Z asks "Geoff edited here after you — undo anyway?"; replay stays well-defined.
- Cost: every undo resyncs whole 64-yd regions (≤ 200 E per SYNC page); measured in Phase 1.

### 3.3 One shared Ctrl+Z (Q6 default, assumed)

EME owns **one** `EmeUndoStack` for terrain strokes, liquid/area edits and object operations. Each entry
carries a backend tag (`Stroke{id}`, `Overlay{edit_id}`, `Object{tx_id}`) and knows how to undo/redo
itself through the server (stroke revert; overlay revert; object inverse transaction, §5.3). Ctrl+Z/Y act
on the newest entry of the current GM only. The stack is session-local; durable history is server-side
(logs), so a relog loses redo but never data.

### 3.4 Permissions (rank model, server-enforced)

| Rank | May do |
|---|---|
| 0–1 | nothing; `.mveme`/`.voxel` = "no such command" (nil return), `.mvox` mutations → `R 1` + `voxel_log` |
| **2** (Mod) | **read-only**: open EME (`caps`), log browser, history, restore-point list, overlay viewer (area/liquid tint) |
| **3+** (GM) | **every mutating tool**: sculpt, flatten, smooth, paint, water, area, objects, undo own strokes, create restore points |
| 4 (Admin) | revert others' work (`undo_player`, `revert snapshot`), delete others' objects, area flag overrides |

- **Change needed in `MonkeyVoxelServer.lua`:** `M.RANK = 2` (line 19; gate at the EDIT check, `rank <
  M.RANK → reason 1`, line ~396) becomes two constants: `M.RANK_VIEW = 2` (log/snapshots listing,
  SYNC is unchanged and open to all) and **`M.RANK_EDIT = 3`** for EDIT/EDIT3 and every mutating service
  op; `M.RANK_REVERT = 4` for revert-others. The `.voxel` command floor stays rank 2 for read subcommands
  (`log`, `snapshots`) and rises to 3 for `snapshot`, `undo` (own) and 4 for `revert`, `undo <player>`.
  Below the floor `return` nil (hidden); never send "access denied". Same constants in `MonkeyEme.lua`.
- `caps{}` returns `{rank, tools:[…], limits}`; the client builds the toolbar from it. A hand-edited
  client sending mutations still gets `R 1`.
- Audit: every mutation → `voxel_log` (four W's, `notified`) plus overlay/object rows in their own
  tables (§4.3, §5.3).

### 3.5 Service ops (cold path, `MonkeyEme.lua`)

Thin module over `MonkeyVoxelServer.lua`'s `M` (revert functions made public). Request `{v:1,id,op,…}`,
reply `{v:1,id,ok,err?,rows?}`, ≤ 2800 B/frame. Ops: `caps`, `strokes`, `undo`, `redo`, `log`,
`snapshot_create`, `snapshot_list`, `revert`, `undo_player`, `ov_get`, `ov_edit`, `ov_undo`, `obj_*`
(§5). Labels UTF-8 (EN/RU); reply text keys localised client-side.

### 3.6 SQL (`sql/custom/voxel_eme.sql`, idempotent, MySQL 5.7+8; conductor applies after backup proposal)

`ALTER voxel_edit ADD stroke SMALLINT UNSIGNED NULL, ADD param SMALLINT UNSIGNED NOT NULL DEFAULT 0`,
index `(guid,stroke)`; `voxel_stroke(id,guid,account,stroke,map,rx_min..ry_max,first_seq,last_seq,edits,
created_at,reverted_id NULL)`; overlay tables in §4.3; object tables in §5.3.

## 4. Water and area/zone metadata

Both are in scope (not Noggit-only). They are **overlays on the immutable ADT baseline**, using the same
model as voxels: server-ordered `seq`, per-region `epoch`, audited rows that are reverted (flagged, never
deleted), and clients rebuilt as `baseline + live overlay rows sorted by seq`.

### 4.1 What the core reads today (verify during Phase 0 — all read from map files, not the DB)

| Fact | Source in the core | Overridable? |
|---|---|---|
| Area id of a point (zone/subzone name, `.where`, chat zone, GM tools, PvP/sanctuary lookups) | `GridMap::getArea` → ADT MCNK area id; flags then come from `AreaTable.dbc` (`GetAreaEntryByAreaID`, `AreaFlags`: sanctuary, PvP arena, capital, no-flying…) | Needs a C++ hook (below) |
| Liquid type/level at a point (swimming, drowning, lava/slime damage, fishing, fatigue) | `GridMap::getLiquidStatus` over MCLQ (vanilla) cells: liquid kind + height + "no water" bits; `Map::GetTerrain()->IsInWater/IsUnderWater` | Needs a C++ hook (below) |
| Terrain height / LOS | ADT height + VMAP/DynamicMapTree | Voxels: Phase-4 item T13 |

The Lua API cannot override these. Therefore: **server C++ hooks under `#ifdef ENABLE_ELUNA`**
(changelog PART 2): `TerrainInfo::GetAreaId()`/`GetLiquidStatus()` consult a small per-map
`MonkeyOverlayIndex` (rect-keyed, loaded from the overlay tables, refreshed on `.reload eluna`-style
reload command + on every accepted overlay edit) **before** the ADT answer. The overlay answers only
for cells it covers; everywhere else behaviour is byte-identical to today. If the C++ hook is not built
(Phase 0 spike fails or is deferred), area/liquid edits still work **visually and in the tool UI**, the
server marks them `enforced=0` in `caps`, and the client labels them "visual only".

### 4.2 Liquid model and client rendering

- **Representation:** a `LiquidPatch` = one of the baseline ADT liquid cells' semantics, expressed on the
  same grid the renderer already consumes: the ADT liquid grid is 8×8 cells per MCNK (4.1667 yd cells,
  9×9 height samples; vanilla MCLQ). The overlay stores **sparse cell edits keyed by (map, cx, cy)**
  (cx,cy = global liquid-cell index = floor(world/4.1667)), each `{kind u8 (water/ocean/magma/slime/none),
  level f32, flags u8 (dark/hidden/fatigue), depth_fade u8}`. `kind = none` means "erase baseline
  liquid here". A patch is a *flat* surface at `level` (sloped rivers stay a Noggit job).
- **Client:** `benilla-world/src/liquid` already builds `WaterChunkInfo` grids per chunk (`liquid/query.rs`,
  `surface.rs`, `real_data.rs`) and answers swim/wade/camera queries through `liquid_at`. EME adds a
  `LiquidOverlay` resource (map → sparse cell table, versioned by region epoch). Hook points:
  (1) `real_data` merges overlay cells over the decoded ADT grid when a chunk's surface is (re)built;
  (2) `liquid_at` and siblings consult the overlay first so swimming/wading/underwater camera agree;
  (3) an overlay change marks the touched chunks dirty → surface rebuild (reusing the existing
  spawn/despawn path used for streaming), mesh/waves/foam/mirror pick it up unchanged.
  Pits dug by voxels: a cell patch above carved ground works because liquid is evaluated against the
  voxel-adjusted terrain height (`terrain_height_under`), so shore/foam follow the pit.
- **Tool (T16) UI:** Raise/Lower (±0.25 yd steps or typed level, "match brush-click height"), Paint
  pool (brush stamps cells at the level; a flood-fill that stops at terrain above level), Erase, Kind
  picker, "sample" eyedropper. Preview: cyan translucent quad over the cells, level number, red cells
  where terrain is above the level.

### 4.3 Server storage, sync, audit, revert

```
liquid_overlay_edit (seq PK auto, map, cx, cy, kind, level, flags, guid, account, created_at,
                     reverted_at NULL, reverted_by NULL, revert_id NULL, stroke SMALLINT NULL)
area_overlay_edit   (seq PK auto, map, cx, cy, area_id, guid, account, created_at, reverted_*, stroke)
overlay_region      (kind, map, rx, ry PK, epoch, last_seq)        -- shared 64-yd region/epoch model
overlay_log         audit of every attempt (four W's, notified flag), like voxel_log
```
- Wire: `.mveme` service ops (`ov_edit{layer,cells:[…],stroke}`, `ov_get{layer,regions}`, `ov_undo{stroke}`),
  downlink `MONKEYEME` records `{layer,seq,stroke,map,cx,cy,…,actor}`; SYNC by (kind,region,epoch,
  after_seq) exactly like voxels (reuse the epoch/`X` reset semantics). Fan-out radius 300 yd.
- Edits are **last-writer-wins per cell in seq order**; revert = flag rows, bump region epoch, `X`.
  Snapshots/restore points are shared with voxels: one `voxel_snapshot` row covers voxel + liquid + area
  rows in its radius (`at_seq` is per layer, stored as three columns). Undo/redo via §3.3.
- Rank: write = 3+, view = 2+, revert-others = 4. Area flag overrides (§4.5) = 4.
- Docs: LOGS.md (new tables), COMMANDS.md, changelog PART 2 (C++ hooks) + PART 4 (SQL).

### 4.4 Area/zone (T17) — what is edited, what the server honours

- **Edited:** the **area id per cell.** The ADT stores one area id per MCNK (33.3 yd), which is too coarse for
  sculpted features, so the overlay keys areas on a **16×16 yd aligned cell** (`ax = floor(x/16)`) for
  the MVP, refinable to 4.17 yd later. A brush/polygon stamps an
  existing `AreaTable.dbc` area id (picked from a searchable list, EN/RU names from the DBC) onto cells.
- **Client:** area id drives zone/subzone name text and music/light lookups (`Light.dbc` zone, `AreaTable`
  sounds). Client DBC is immutable, so EME can only choose **existing** area ids; creating brand-new
  zones/subzones remains a DBC + MPQ patch job (changelog PART 5). The overlay is consulted wherever the
  client resolves "area at point" (the one function that currently reads the MCNK area id).
- **Server honours (with the §4.1 C++ hook):** zone/subzone for `GetZoneId/GetAreaId`, `.where`, chat
  tags, hearthstone/binding, resting, `AreaTable` flags that derive from the area id (sanctuary,
  capital city, PvP/arena, no-mount). Flag edits themselves ("make this subzone sanctuary") live in
  `monkey_area_flag_override(area_id, add_flags, remove_flags)`, rank 4, applied inside the same hook —
  no client DBC change needed because the client only needs the area id for names. Flags that the
  CLIENT also reads (e.g. showing the sanctuary/PvP warning text) stay at the DBC value: **known
  mismatch**, documented in `caps.enforced`.
- **Open risks:** (a) mmaps/vmaps do not encode area, so nothing else caches it; (b) quest-area triggers
  and `AreaTrigger.dbc` are positional, not area-id based — unaffected; (c) zone change events
  (`OnUpdateZone`) fire from the hooked lookup, so Eluna scripts see edited zones.

### 4.5 MVP vs later (water/area)

- **MVP (Phase 3):** flat-level water cells (water kind only) + erase, area-id paint at 16-yd cells,
  viewer overlays for rank 2, undo/restore, visual+logged. Server enforcement only if the Phase-0 C++
  hook spike lands; otherwise labelled "visual only".
- **Later:** all liquid kinds incl. magma/slime damage, 4.17-yd area resolution, polygon fill, sloped
  patches, flood-fill from a source, area flag overrides (rank 4), WMO-interior liquid (not editable),
  ocean/bounds levels.

## 5. UI shell and object placement

### 5.1 UI technology and window framework (EME's own)

`bevy_egui 0.39` already exists in `crates/benilla-app/Cargo.toml` (`optional`, currently pulled only by
`dev`). **EME adds its own feature `eme` that pulls `bevy_egui`** (no dependence on the dev tooling).
`eme-ui` provides: a `EmeShell` (menu bar + tool strip + dockable side panels via `egui::SidePanel`/
`Window`, layout persisted to `benilla-config/eme-layout.toml`), `EmeTheme` (dark, high-contrast, shape +
text with colour), an input arbiter (`EguiPointerOver` → viewport vs panel), and an i18n loader reading
`eme/locales/{enUS,ruRU}.json` (keys `MONKEY_EME_*`, RU shown to the owner before baking). Rejected: FrameXML
Lua addon (no raycast/brush-preview bindings, Lua 5.0 limits, gate-digest churn) and Forever-atlas Bevy UI
(glue-screen oriented, no tables/trees).

- **F9** toggles EME for GMs. On: stock UI hidden except chat, free camera engaged (WASD + RMB look, Shift
  fast, Ctrl slow, wheel = speed, **F** = frame selection, **Tab** = return to character), the world
  keeps streaming and the character is held still (`.gm fly` not required). Off: camera and UI restored.
- **Selection:** click / Ctrl+click / box-drag over objects (CPU ray vs bounding boxes for GOs, NPCs,
  WMOs), selection set in `eme-core`, outliner list with search, inspector with numeric X/Y/Z/yaw/pitch/
  roll/scale fields.
- **Gizmo (re-implemented):** translate arrows + plane handles, rotate rings (yaw/pitch/roll), uniform
  scale; world/local space toggle; snap grid (0.1/0.5/1 yd, 5/15° ) ; drawn as a Bevy overlay pass, axis
  handles picked by ray vs capsule/ring math. Ground-snap uses terrain + voxel colliders.
- **Layout sketch:** top tool strip `[Sculpt][Flatten][Water][Area][Objects] | [Restore][Log] | Undo ◄ ► Redo
  | Link: OK  rank 3`; left = tool options + brush profile; right = history / restore points / outliner;
  bottom = status (map, region, epoch, pending, remesh ms, rate).

### 5.2 Own undo/redo and shared history

The `EmeUndoStack` (§3.3). Object entries store the pre-image (full object row) so an undo is an inverse
transaction sent to the server, which re-checks permission and fief rules.

### 5.3 Object placement wire (public, minimal)

EME does **not** speak `BuildMode.lua`'s gossip menu or `BUILD` addon protocol. It speaks its own small
documented service ops over `.mveme`: `obj_nearby{radius}`, `obj_get{guid}`, `obj_spawn{kind:"go"|"npc",
entry,pos,ori,scale,tx}`, `obj_move{guid,pos,ori,scale,tx}`, `obj_delete{guid,tx}`, `obj_undo{tx}`; every
mutation carries a client `tx` id for idempotency and returns the authoritative object row. Server table
`eme_object_tx(tx PK,guid,account,action,before_json,after_json,created_at,reverted_at)` backs undo and
audit (four W's; also writes `gameobject_log` for GOs so existing audit queries keep working). Rank: 3+
to place/move own or others' objects (Build Mode today is rank 2 — raised for EME ops only; legacy
gossip Build Mode keeps its own rank), 4 to delete objects placed by others.

### 5.4 How the open client reaches the Everwood Build Mode path without shipping secrets

- The **wire above is the only contract** the open client knows. It is published in `docs/EME-API.md`.
- On Everwood's server, `MonkeyEmeObjects.lua` (Everwood repo, closed to the EME repo) implements `obj_*`
  by calling Build Mode's server functions in-process: GO creation/placement/scale setters used by
  `BuildMode.lua`, `gameobject_placed_by`, and `FiefBuildGuard` (fief permission check on the target
  position). `BuildMode.lua` exposes a small `MonkeyBuildAPI` table (`place`, `move`, `remove`, `canBuildAt`)
  — a refactor of existing code, no behaviour change — which the adapter consumes.
- The **reference server** published with EME ships a plain `MonkeyEmeObjects.lua` with a default policy
  (rank check only, no fief logic) so third parties can run EME; Everwood swaps in its adapter by file
  replacement. No keys, digests, hostnames, fief rules or asset lists are in the open repo.
- NPC spawning goes through the same adapter (`.npc add`-equivalent core calls); Studio's `monkey_studio_*`
  tables are not touched or shared.

## 6. Phased plan and lanes

Lane files are disjoint within a phase; workers = Opus/Sonnet subagents (alternate lanes), conductor audits
each diff. Workers never apply SQL, back up or reload; the conductor applies SQL first, then
`.reload eluna`. Build target dir: private `CARGO_TARGET_DIR` per active lane. Test stack:
`G:\mangos\disposable\voxtest-server` (`voxtest.ps1 start|stop`, realmd 3725 / world 8086).

### Phase 0 — contracts and spikes (≈1 round)
- **Conductor:** cut `everwood-eme` from `everwood-voxel_world`; write CONTRACT v3 (§3.1, §3.5, §4.3,
  §5.3) into `voxel-v2/CONTRACT.md`; decide the §4.1 C++ hook go/no-go from Lane S.
- **A (voxel data)** `crates/benilla-voxel/src/{world,wire}.rs`, `tests/`: EDIT3/`e` codec, `stroke`/`param`,
  journal v3, flatten, test vectors.
- **B (server voxel)** `MonkeyVoxelServer.lua`, new `MonkeyEme.lua`, `sql/custom/voxel_eme.sql`: rank
  constants (§3.4), v3 accept, strokes, undo/redo/log/snapshot services, audit.
- **C (core)** new `crates/eme-core/**`: tool trait, registry, bus, `EmeUndoStack`, stroke builder,
  transport trait, service JSON types, public API (§2.5), fixtures.
- **S (spike, read-only + prototype in a scratch branch)** core C++: where `GetAreaId` / `getLiquidStatus`
  are answered and how to inject `MonkeyOverlayIndex`; report effort and risks. Output: a note, no merge.
- *Accept:* `cargo test -p benilla-voxel -p eme-core`; determinism test (same EDIT3 list → identical
  density hash twice and after out-of-order rebuild); extended `server_harness/run_harness.py` +
  `e2e/e2e_server_test.mjs`: EDIT3 → `e` to 2 clients; stroke undo → `X`+resync without it; redo → back;
  **rank-2 EDIT3 → `R 1` + `voxel_log`; rank-3 accepted**; rank-1 `.mveme` → "no such command".

### Phase 1 — EME MVP, terrain (≈2 rounds)
- **D (seam+viewport)** `benilla-world/src/voxel.rs` (`MonkeyVoxelApi`, `voxel_dev_brush` cfg),
  `crates/eme/src/{lib,viewport,dispatch,camera}.rs`: plugin, F9, free camera, picking, brush preview.
- **E (tools)** `crates/eme/src/tools/{sculpt,flatten,history,restore,log}.rs` + locales.
- **F (UI shell)** `crates/eme-ui/**`: shell, theme, i18n, arbiter, panels (no tool logic).
- **G (transport)** `benilla-app/src/net/eme_relay.rs`, `benilla-app/Cargo.toml` feature `eme`.
- *Accept (offline):* capture hook `WOW_EME_SCRIPT=<json>` (select tool, stroke at x,y, undo, screenshot):
  crater appears, undo removes it, no seam/sky slit; EN and RU render. *Live (voxtest, 2 clients):* GM A
  strokes → B sees within 1 s; Ctrl+Z → both lose it, journals hash-equal; restore point → 3 strokes →
  revert → equal to pre-snapshot hash; log browser lists A's rows; **rank-2 sees panels read-only and
  every mutating control disabled, server rejects a forged EDIT3**; non-GM: F9 does nothing, `.voxel`/
  `.mveme` = "no such command"; undo of a 20-edit stroke ≤ 200 ms visible on B.

### Phase 2 — objects (≈1–2 rounds)
- **H (client)** `crates/eme/src/{selection,gizmo}.rs`, `tools/objects.rs`, `eme-ui` outliner/inspector.
- **I (server)** `MonkeyEmeObjects.lua` (reference policy), `sql/custom/eme_objects.sql`.
- **J (Everwood adapter)** Everwood repo only: `BuildMode.lua` → `MonkeyBuildAPI` refactor + the Everwood
  `MonkeyEmeObjects.lua` adapter with `FiefBuildGuard`.
- *Accept:* place a GO on a build mound → persists across relog at voxel height; move/rotate/scale via
  gizmo; Ctrl+Z order matches action order across terrain+object; fief-guarded position rejected with
  adapter, allowed with reference policy; legacy Build Mode gossip still works (no regression).

### Phase 3 — water and area (≈2 rounds, parallel with Phase 2 where disjoint)
- **K (client liquid)** `benilla-world/src/liquid/{real_data,query}.rs` overlay hook, `eme/src/tools/water.rs`.
- **L (client area)** `eme/src/tools/area.rs`, area resolver hook in `benilla-world`.
- **M (server overlay)** `MonkeyEmeOverlay.lua`, `sql/custom/eme_overlay.sql`.
- **N (core C++, if Phase-0 go)** `src/game` overlay index hooks under `#ifdef ENABLE_ELUNA`, changelog PART 2.
- *Accept:* dig a pit, place water at level L: swimming/underwater camera work in-pit, relog persists, 2nd
  client sees it live; undo/revert restores the baseline; area paint renames the zone text for all
  clients; with N: `.where`/`GetAreaId` and drowning/swim agree with the overlay, otherwise `caps.enforced=0`
  and UI shows "visual only".

### Phase 4 — later tools and merge prerequisites
Per-tool lanes: T8 primitives, T9 paint (ADT tileset palette, shader), T10 roughen, T11 copy/paste. Merge
prerequisites: server voxel collision (T13), `OVERSPEED_Z` smoke test, LOD, journal compaction,
`eme-monkeynet` adapter in mainline `ext/`. **Merge gate:** Phases 1–3 acceptance green live + owner
sign-off; then `everwood-eme` → mainline as one merge; server voxel/EME scripts → `eluna`. Studio, if
it wants EME, integrates after, via §2.5, on its own branch.

## 7. Docs the lanes must update
COMMANDS.md (`.mvox` v3, `.mveme`, `.voxel` rank changes), LOGS.md (`voxel_log` actions, `voxel_stroke`,
overlay and object tables), changelog.txt PART 2 (C++ overlay hooks) / PART 4 (SQL) / PART 6 (script deps),
`DESIGN-VoxelTerrain.md` status, CONTRACT.md v3, new `docs/EME-API.md` (public wire + API), `THIRD-PARTY.md`.

## 8. Owner decisions and defaults
Settled by the owner: independent open EME; F9 in the normal exe (default-on feature); sculpt = GM 3+;
water + area in scope. **Defaults assumed (change anytime):** Q4 = WARN on undo over others' work;
Q6 = one shared Ctrl+Z inside EME; Q8 = paint palette from the ADT tileset layers for MVP.
**New questions that need the owner:**
- **N1** Licence for the open repo (Apache-2.0/MIT like benilla, or GPL-compatible?) and whether the
  reference server scripts ship in the same repo.
- **N2** Is a rank-3+ C++ core hook for area/liquid enforcement acceptable (Phase 0 spike decides cost), or
  should water/area stay "visual only" until the voxel server-collision work (T13)?
- **N3** Object edit rank: EME object ops at 3+ (Build Mode today is rank 2) — ok to raise for EME only?
- **N4** Area resolution MVP at 16-yd cells (coarse subzone edges) acceptable?
