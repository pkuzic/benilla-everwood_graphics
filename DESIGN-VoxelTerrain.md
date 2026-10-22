# DESIGN — Voxel Terrain (Everwood)

**Status:** client voxel system DONE + verified + live-safe (brush, guild multiplayer sync, shared GM build site, persistence, grass/dirt/rock material, walkable collision, GM-gating); server-side area-wide/arbitrary-collision/navmesh scoped as enhancements · **Branch:** `everwood-voxel_terrain` (this repo, `pkuzic/benilla-everwood_graphics`)
**Decided 2026-10-16:** Option **C** (voxel / SDF) for live networked terraforming. Editor is a benilla
mod, **voxel-first**, NOT a Noggit rewrite.

This spec covers all three codebases it touches: the **benilla client** (this repo), the **mangosd
server** (`tortoise-wow`, private — VMaNGOS lineage), and the **editor mod** (new, in this repo).

---

## 1. Goal

Let designated regions of the world hold **true 3D terrain** — overhangs, caves, arches, floating
islands — and let **GMs/players reshape them live**, synchronised across clients and persisted. The
WoW heightmap cannot express any of this: terrain is single-valued `z = f(x,y)` on **both** the
benilla client and mangosd (`GridMap::getHeight`), and the engine's only real 3D collision is vmaps
built from WMO/M2. Voxels add the missing representation.

## 2. Core model — voxel volumes layered on the heightmap

**Do not voxelise the world.** Keep the existing heightmap ADT terrain for ~99% of the map. Voxels
are opt-in **voxel volumes**: axis-aligned regions (a box of voxel chunks) placed over the heightmap
where terraforming is enabled. This single decision bounds the cost of *everything* downstream —
collision, navmesh, streaming, persistence, memory — to just those volumes.

- A volume is a grid of **voxel chunks** (e.g. 32³ voxels, voxel size ≈ 0.33–0.5 yd — TBD by
  perf/fidelity tradeoff).
- Inside a volume, the heightmap is suppressed (hole-masked) and the voxel surface is authoritative;
  at the volume boundary the voxel mesh skirts down to meet the heightmap so there is no seam/cliff.
- Noggit never edits a volume's contents. It keeps owning the base heightmap, texture splat, object
  placement, etc., and simply **round-trips / ignores** the voxel data files it doesn't understand.

## 3. Representation

- **Density field (SDF):** each voxel stores a signed density (surface at 0; inside > 0, air < 0),
  stored as `i8`/`i16` fixed-point for determinism and compactness.
- **Material:** per-voxel material id (which ground texture / splat set the surface uses). Triplanar
  texturing (no UVs) because voxel surfaces aren't a heightmap.
- **Resolution (settled):** **0.5 yd voxels, 32³ chunks** (= 16 yd chunks). Beats native heightmap
  (~4.17 yd) by ~8×; ~8 MB dense per 100×100×50 yd volume (sparse/near-surface storage cuts that).
  Voxel size is **sticky** (baked into stored data) — confirm with a P0 benchmark before mass use.
- **Meshing (settled):** **`fast-surface-nets`** for the body (smooth, few verts, deterministic),
  moving to **transvoxel** at LOD boundaries in P4 so chunk LOD levels stitch without cracks. The
  mesher sits behind the density field, so it is swappable with zero content cost — **dual contouring
  is a potential later test** if crisp sharp edges (cliff lips) become a must.
- Meshing is **deterministic**: same density field → same mesh, so the client (render) and server
  (collision) agree on the surface. Fixed-point density + a fixed algorithm guarantee this.

## 4. Edit operations (the wire + the editor share one op)

One compact op drives both the in-game GM brush and gameplay terraforming:

```
VoxelEdit {
  volume_id: u32,
  op:        u8,     // 0 add, 1 subtract, 2 smooth, 3 flatten, 4 paint-material
  center:    [f32;3],
  radius:    f32,
  strength:  f32,
  material:  u16,     // for paint / add
}
```

~24 bytes. Bandwidth is a non-issue even under heavy sculpting; the work is authority + remeshing,
not transport. The op is applied to the density field, which dirties the ≤ 8 chunks it touches; only
those remesh (client render + server collision).

## 5. Architecture across the three codebases

| Layer | Crate / module | Responsibility |
|---|---|---|
| **Format** | `benilla-adt` (+ new `benilla-voxel` format types) | Parse/serialise voxel-volume files + the edit op |
| **Client render** | `benilla-world` (renderer, "no game attached") | Mesh voxel chunks (Bevy 0.18 `Mesh::new(TriangleList)` + `insert_attribute`), triplanar terrain material, LOD/streaming |
| **Client session/wire/UI** | `benilla-app` | Receive/apply/broadcast edit ops; GM brush input; egui panels (under `dev`/`editor` feature) |
| **Editor mod** | **new `benilla-editor` crate + `editor` feature** | Voxel sculpt UI (egui), volume authoring, the ADT/voxel **writer** |
| **Server authority** | mangosd (`tortoise-wow`) | Validate + apply + persist + broadcast edits; collision + navmesh rebuild |
| **Transport** | MonkeyNet (relay/cmd channels) | Carry edit ops + late-join snapshots |

## 6. Editor mod (`benilla-editor`)

- A **Bevy plugin crate** gated behind an **`editor` feature** — a build variant mappers run, never
  shipped to players. (benilla "mods" are compiled-in crates/features, like monkey-anims/mapi/phys —
  not runtime-loaded addons.)
- **UI:** `bevy_egui` is already in the workspace under the `dev` feature (benilla-app). Editor
  panels/brushes reuse it; no new UI stack.
- **WYSIWYG:** the editor renders through benilla's own voxel mesher, so what you sculpt is
  pixel-identical to what players see — the class of parity bugs like *"Noggit paints MCCV, benilla
  doesn't render it"* (shipped fix 78c297b) **cannot occur** in an in-engine editor.
- **The real cost = the writer.** benilla is **read-only today** — it has no ADT/MPQ write path
  (only test-only chunk builders). The editor must build the serialise side (voxel-volume files, and
  later any ADT write-back). This is the bulk of "editor" work, done piecemeal.

### Noggit migration ladder (NOT a big-bang rewrite)

Keep Noggit as the heightmap/texture/object editor. Let `benilla-editor` **absorb** Noggit functions
over time, each one requiring its own write path:

1. **Voxel sculpt** (no Noggit equivalent — the reason we start here).
2. **MCCV vertex shading** (benilla renders it natively in-editor → fixes the parity gap).
3. Texture / alpha-layer paint.
4. Heightmap raise/lower/flatten/smooth.
5. Object placement, holes, water, areas, minimap bake…

End state (long horizon): `benilla-editor` *can* fully replace Noggit with perfect parity — reached
by migration, not rewrite.

## 7. Server — the hard 60%

The server is authoritative for movement; a voxel edit the server can't see = players walk on air /
fall through. What our server **already has** de-risks this (it is not greenfield):

- `vmap/DynamicTree.cpp` + `vmap/GameObjectModel.cpp` — **runtime-addable collision models** (today
  used for doors/destructible GOs/transports).
- `dep/recastnavigation` — vendored **tiled Detour navmesh**.

**Plan:**
- **Collision:** on each dirtied voxel chunk, mesh it to a collision mesh and inject it into
  `DynamicMapTree` as a `GameObjectModel`-like dynamic model (replace the previous one for that
  chunk). `Map::GetHeight`/LoS then see the voxel surface. Inside a volume, suppress the heightmap
  contribution.
- **Navmesh:** rebuild the affected **Recast tile(s)** incrementally from the voxel collision mesh so
  NPCs/pathing know the new walkable surface. Tiled navmesh = rebuild one tile, not the world.
- **Authority:** server is the single source of truth. Client sends an edit *request*; server
  validates (rank/permission, within a volume, rate-limited — cf. the 2026-10-09 per-command
  limiters), applies to its density field, persists, and **broadcasts** the authoritative op to all
  clients in range. On any desync, server snapshot wins.
- **Budget:** incremental, background-threaded remesh of only the touched chunks; a dirty queue
  drains over ticks so the map update loop never stalls.

## 8. Networking & late-join

- Transport = **MonkeyNet** (our existing addon-traffic relay/cmd channels; phase 2 live 2026-10-10).
- **Live edits:** server fans out each authoritative `VoxelEdit` to clients whose loaded volumes
  include it.
- **Late-join / stream-in:** when a client loads a volume, server sends either the accumulated edit
  journal for that volume or a baked density snapshot (snapshot once the journal grows large —
  compaction policy TBD).

## 9. Persistence

A per-map **voxel edit journal** (same spirit as BuildMode / `gameobject_placed_by` persistence):
the base volume (authored in the editor, shipped in client data) + an append-only journal of live
edits stored server-side (DB table). On load, base + replay journal = current state. Periodic
compaction bakes the journal into a new base snapshot to cap replay cost.

## 10. Phased roadmap

- **P0 — Render a static voxel volume (client only).** `benilla-voxel` format + `benilla-world`
  mesher (pick the meshing crate), triplanar terrain material, one hand-made volume renders in-game.
  No edits, no collision. *Proves the renderer.*
- **P1 — In-game GM brush + server-authoritative sync.** The `VoxelEdit` op end-to-end over
  MonkeyNet; server validates/applies/broadcasts; clients remesh. Visual only (no collision yet).
  *Proves the loop.*
- **P2 — Persistence.** Edit journal + late-join snapshot. Edits survive restart and reach
  late-joiners.
- **P3 — Live collision + navmesh.** Inject voxel collision into `DynamicTree`; rebuild Recast
  tiles. **Walkable in real time.** *The hard milestone.*
- **P4 — Polish.** LOD + transvoxel seams, streaming, memory, `benilla-editor` offline editor-mode
  UX (free camera, big brushes, undo), and the first Noggit-ladder pickup (MCCV).

## 11. Settled decisions (owner, 2026-10-16)

1. **Voxel / chunk size** → **0.5 yd voxels, 32³ chunks.** Sticky — verify with a P0 benchmark.
2. **Who can sculpt live** → **GM-only.** Built as a per-volume capability flag so "players in
   sanctioned zones" can be enabled later by config, not a rearchitecture.
3. **Meshing** → **`fast-surface-nets`**, transvoxel at LOD boundaries in P4. **Dual contouring is a
   potential later test** (crisp sharp edges) — reversible, the mesher is isolated behind the density
   field.
4. **Material** → **reuse the existing terrain splat set** (triplanar, no new art), with a
   **dedicated voxel palette as a potential later test** for vertical/overhang faces (P4+).
5. **Authoring** → **both, phased:** in-game GM brush first (P1), stand up the `benilla-editor`
   offline mode later (P4) for heavy sculpting.

## 12. Risks

- **Server collision/nav rebuild latency** (P3) is the dominant risk — must stay within the map-tick
  budget. Mitigation: incremental, background, voxel-volumes-only, tiled Recast.
- **Client/server mesh determinism** — mismatched surface = fall-through/rubber-band. Mitigation:
  fixed-point density + one shared meshing algorithm.
- **Scope creep into "rebuild Noggit"** — mitigated by the voxel-first rule and the migration ladder.
- **The ADT-writer gap** — benilla has no write path; budget for it explicitly.

---

## P0 — DONE (2026-10-16)

The renderer is proven: a `benilla-voxel` SDF → surface-nets `VoxelMesh` → Bevy mesh renders in-engine.

**Run it live (GPU box):**
```
CARGO_TARGET_DIR=G:\mangos\disposable\voxel-target cargo run -p benilla --features voxel
```
Enter the world with a character; a bright voxel **torus** spawns ~22 yd in front of the camera on
the first live frame and stays fixed in the world. Its WoW coords are logged (`MONKEY (voxel):
spawned P0 torus …`). The `voxel` feature is **off by default**, so the normal client is unchanged.

**Offline screenshot (server-less capture):**
```
WOW_DATA=G:/tw_experiment/beverwood/Data BENILLA_HOME=<scratch> \
WOW_CAPTURE=felwood-noon WOW_CAPTURE_OUT=<out>.png \
  <voxel-target>/debug/benilla.exe
```
Scenes: `WOW_CAPTURE=list` (felwood-noon/-night, water-noon/-night). In capture the scenario camera
settles after the one-shot spawn, so a capture-only system keeps the torus framed (no-op in live play).

## P1 — DONE + VERIFIED (2026-10-17)

In-game terraforming sandbox, multiplayer sync, a shared GM-placed build site, and persistence — all
client-side (the server-authoritative half is still the "hard 60%", below).

- **P1a brush:** a multi-chunk flat-top ground slab spawns; hold **`[`** carve · **`]`** build · **`\`**
  smooth, **`-`/`=`** brush size. Live re-mesh. Earthy unlit shading (a lit material is black under
  benilla's custom world light).
- **P1b multiplayer sync:** each edit broadcasts as a **guild addon message** `MV:<hex>`
  (`ClientCommand::AddonMessage { distribution: Guild }`) — the core's `Guild::BroadcastToGuild` relays
  `LANG_ADDON` guild chat to every online member, **no server change** (a raw custom WoW channel was NOT
  relayed on this server). Edits are volume-LOCAL coords; remote edits that arrive before the local volume
  exists are buffered, not dropped.
- **P1c shared build site:** **Ctrl+P** drops a site at the GM's feet, persists it
  (`monkey_voxel_site.bin`), and broadcasts the WORLD anchor as `VS:<hex>` (`MonkeyVoxelSpawn`). Every client
  adopts that exact anchor and re-anchors its single volume there, so a guild's local-coord edits all land on
  the same world spot. Survives relog. One system (`monkey_voxel_manage_volume`) owns create/place/adopt so
  exactly one volume ever exists (a split let deferred `Commands` make two).
- **Persistence:** every local + remote edit appends to `$BENILLA_HOME/monkey_voxel_journal.bin`, replayed
  on spawn; the placed site anchor persists alongside.
- **Real terrain material:** vertex colours blend grass / dirt / rock by SLOPE (flat = grass, cliffs = rock),
  shaded by a fixed sun + ambient + position mottle, rendered unlit. Looks like ground that fits the world
  (capture-verified). A true ADT-splat triplanar material is still a later option (owner decision 4).
- **Walkable collision:** the voxel entity carries an avian `Collider::trimesh` (+ `RigidBody::Static`,
  default layer) rebuilt from the density on every edit by `monkey_voxel_sync_collider` (one
  `Changed<MonkeyVoxelData>` system). benilla's mover sweeps the player capsule against avian colliders (not
  the heightmap), so you stand and walk on the voxel surface — including overhangs/walls the heightmap can't
  express. **Live-safe** (see server findings): no geometric fly/height anticheat check rejects an above-ground
  stander.
- **GM-gating** (owner decision 2): the KEY-driven brush + Ctrl+P placement require the local GM flag
  (`PLAYER_FLAGS & 0x8`, `.gm on`), bridged from benilla-app into benilla-world's `MonkeyLocalGm` (the
  `FfxDeathFade` pattern). Viewing/adopting a site and applying others' edits are NOT gated; the headless
  `WOW_VOXEL_AUTO*` hooks bypass the gate for tests.

**2-client test:** `disposable/voxmp_test.ps1` (both chars in one guild — the relay is guild-scoped). Verified
A placed `[-8963.5,-132.5,84.4]`, B adopted the identical anchor, both journals 17 B after A's edit. Material
capture-verified; collision smoke-verified (collider attaches + rebuilds on edit, no avian panic).

## Server half — GROUNDED (research 2026-10-17)

Ground-truthed against the actual tortoise-wow core. The client feature above is complete and **live-safe
on its own**; these are enhancements, now precisely scoped:

- **Anticheat is NOT a blocker for walking on voxels.** Fly detection is movement-FLAG based (normal walk
  flags pass); there is NO geometric "player N yd above GridMap = fly/teleport" reject. Undermap only fires
  ~100 yd *below* ground; wall-climb check is dormant. The one live exposure is `CHEAT_TYPE_OVERSPEED_Z`
  (`Anticheat/Movement/Movement.cpp:593`) on ABRUPT vertical transitions (raising a mound under a stander, or
  a near-vertical ascent) — smoke-test on live-config before shipping; likely fine for normal walking.
- **Area-wide distribution (non-guild nearby players) = drag-n-drop Eluna, NO core rebuild.** Eluna can't see
  inbound addon chat (hooks defined but core never calls them; guild relay is pure C++ `BroadcastToGuild`).
  Instead: GM client uplinks each edit on the existing MonkeyNet **`build`** channel (→ Eluna `OnMonkeyNet`
  event 55, MODERATOR-gated, binary-safe ≤16 KB, rate-limited) or as a `.`-command; a `lua_scripts` script
  fans it out with `anchor:GetPlayersInRange(R)` + `p:SendAddonMessage("VOXEL", payload, 6, p)` (exactly how
  `lua_scripts/BuildMode.lua` already distributes build state). **Client cost:** a MonkeyNet-send uplink (needs
  benilla CMSG_MONKEY support — the sticking point) and an inbound `VOXEL` whisper-addon handler. The guild
  relay already covers a build team, so this is an enhancement, not a blocker.
- **Server-authoritative collision** (so non-voxel clients / server mechanics see the height): `Map::GetHeight`
  already maxes in `_dynamicTree` (GameObject models). A placed/scaled collidable prefab GO works today via
  Lua (BuildMode.lua pattern). TRUE arbitrary-voxel collision needs synthesizing an in-memory vmap `WorldModel`
  from the density and `Map::InsertGameObjectModel` — a medium C++ piece, not a one-liner.
- **NPC navmesh over voxels = genuinely large / descoped for v1.** Recast is vendored but the BUILD side is
  `#ifdef MMAP_GENERATOR` only; there is NO runtime tile rebuild. NPCs will path under a mound until an offline
  MMAP regen. Players stand on mounds fine without it.

*Next (scoped): the Eluna area-relay + MonkeyNet client uplink (area-wide); optional `OVERSPEED_Z` exemption if
live testing flags it; multi-site; arbitrary-voxel `_dynamicTree` collision; P4 LOD/transvoxel + offline editor.*

## Real-terrain mode — DONE + VERIFIED (2026-10-18, branch `everwood-voxel_realterrain`)

Owner redirect (2026-10-17): "normal terrain editable, new and old terrain still look Blizzlike" →
a build site is **voxel-augmented real ground**, client-side first (GM previews). Each piece is
verified in offline captures and/or in-world on the local server:

| Commit | Piece |
|---|---|
| 7dc07b0a | **Seed** — `monkey_voxel_sample_terrain` samples the ADT under the footprint → `VoxelVolume::from_heightmap`; surface `SEED_LIFT` 0.15 yd above the ADT (no z-fight). |
| 660aa4bf / 95ddfeba | **Look** — `voxel/material.rs` + `shaders/monkey_voxel.wgsl` = a copy of `terrain.wgsl`: the tile's own `layer_array`/`alpha_array`/`shadow_array`, exact planar chunk UV + blend chain + MCSH on tops, per-fragment chunk pick from a 3×3 neighbourhood, the shared `wow_light` buffer (ambient/diffuse/sun shadow/point lights/fog/wet). Walls: sharp triplanar (|n|^6) of the least-saturated layer + low-frequency tone. The voxel edge is invisible at noon and dusk. |
| 7f1e65d8 → a8c5215d | **Hiding** — `voxel/terrain_hide.rs` re-meshes the overlapped render cells with the ground **clipped exactly** to the footprint inset 0.5 yd (`terrain_stream/voxel_clip.rs`: Sutherland–Hodgman against the 4 outside regions, attributes interpolated, shared edges split bit-identically). Replaced the first 8.33-yd MCNK-hole-block carve, which left a rim or a sky void. |
| e1a3034b / a8c5215d | **Dig-through collision** — the tile's terrain trimesh is rebuilt from the same clipped chunks (`PendingCollider` swap on the tile root via `finish_colliders`, so the old one stays until the new one attaches and `ColliderEpoch` bumps); restored when the site leaves, re-applied after stream-out/in (race-guarded). `terrain_height_under` returns None inside the carve; the seed sampler and placement use the raw path. Probe `WOW_VOXEL_PROBE_RAY=1`: a dug pit's floor is hit 6.3 yd below the old ground. |
| bfd070b5 | **Re-seed** — a persisted/adopted site that spawns before its tile is resident re-seeds once the tile streams in; the hide plan re-plans until resident. |
| f83580ee / 69eacb5a | Dev/compat: `WOW_WINPOS` (test clients born on another monitor); Turtle-chain FrameXML shims (no Lua error popups on Everwood's client). |

Dev hooks: `WOW_VOXEL_AUTOPLACE`, `WOW_VOXEL_AUTOEDIT`, `WOW_VOXEL_NO_HIDE`, `WOW_VOXEL_NO_FOLLOW`,
`WOW_VOXEL_PROBE_RAY`, `WOW_VOXEL_TEST_MOVE_AT`. Known limits: a site straddling an ADT tile border wears
the nearest in-tile chunk's textures on the far side; the outer 0.5 yd of the footprint keeps the real
ground (the overlap that prevents a sky slit); NPC navmesh and server-side collision are not touched
(client-side first). Everwood login needs the local-only AUTH_BUILD 7272 change in benilla-protocol
(never committed here), and the Everwood addon gate needs a GM-rank test account (this branch has no
native gate report).

## World layer v2 — client side (2026-10-18, branch `everwood-voxel_world`)

Replaces the single Ctrl+P "site" with the CONTRACT §1–2 world voxel layer (`disposable/voxel-v2/CONTRACT.md`).

- **Data** (`benilla-voxel/src/world.rs`, engine-free): world lattice `I = round(wow/0.5)` in WoW axes; chunk
  `k = floor(wow/16)` OWNS lattice `32k..32k+31` (32³ i8). Meshing reads a 34³ window `32k-1..32k+32`; the
  apron comes from the owning neighbour (its density if active, else the seed), so every sample has exactly
  one owner — seams cannot disagree. `ColumnSeed` = raw ADT height (+0.15 `SEED_LIFT`) per lattice column over
  a 36² window (apron + smooth read ring); holes = air. `VoxelWorldData::build(keys, edits)` is the canonical
  "seed + every edit touching the set, in order", applied to the whole set in lockstep (smooth is Jacobi over
  the gathered pre-edit field, so a multi-chunk smooth equals the one-grid result); `apply_incremental` is the
  in-order shortcut. `MapJournal`: confirmed edits by seq, then local predictions in arrival order;
  `insert` reports Appended / OutOfOrder (→ rebuild) / Duplicate; `confirm(nonce, seq)`, `reset_region`.
  `VoxelJournal::encode/decode` = `monkey_voxel_world.bin` ("MVXW", v2). `VoxelEditOp::to_wire/to_hex` = the
  23-byte CONTRACT §3 EDIT body.
- **Bevy side** (`benilla-world/src/voxel.rs`): `MonkeyVoxelWorld` resource (wanted chunks per column, seed /
  build / rebuild / hot+dirty remesh queues, entities, carve). Activation = the edit bbox's chunks; once a
  column's seed loads (tiles resident), any wanted chunk within ±1 of the surface pulls in the column's
  surface chunks. One entity per non-empty chunk: `Mesh3d` + per-tile `MonkeyVoxelMaterial` + Static trimesh
  `Collider`. Brush chunks are meshed the same frame; journal replay uses a 4 ms/frame budget. Cap 4096 chunks.
- **Carve**: `terrain_stream::voxel_clip::VoxelCarve` = union of fully-meshed surface columns, one rect per
  column inset 0.5 yd only on sides without a carved neighbour; cells/tiles carry the signature of the rects
  reaching THEM, so a new column re-clips only its cells and tile colliders.
- **Material**: one material per ADT tile, the shader picks the MCNK chunk per fragment from the tile's 16×16
  table (holes filled with the nearest drawing chunk; across a tile border the nearest in-tile chunk).
- **Brush**: GM-gated; raymarch (0.25 yd steps, bisection) through active voxels else the raw ADT ground;
  edits emitted at ≤ 8/s (moved ≥ r/2 or 125 ms), exactly those are applied, journalled and sent.
- **Relay**: guild addon `MV2:` + hex EDIT body (no seq, appended in arrival order) — the CONTRACT §4 fallback.
- Removed: Ctrl+P / `WOW_VOXEL_AUTOPLACE`, `VS:` site messages, `monkey_voxel_site.bin`, the v1 journal
  (`monkey_voxel_journal.bin`, ignored with one log line), the capture follow/demo hooks, `WOW_VOXEL_TEST_MOVE_AT`.
- Dev hooks: `WOW_VOXEL_AUTOEDIT=1|x,y` (scripted craters / chunk-corner dig / mound + overhang / 8-yd + 2-yd
  digs), `WOW_VOXEL_PROBE_RAY=1` (physics ray under the first dig), `WOW_VOXEL_NO_HIDE=1`.
  After the script, a held-brush perf stroke (16 × 2-yd digs at 8/s across a chunk border + one live 8-yd dig),
  each logged as `perf edit r=… ingest … + build/mesh/collider … ms`.
- **Local flag**: every journal edit carries `local` (journal flag bit 1, not on the wire); only LOCAL unconfirmed
  edits are `pending_local()` uplink candidates and nonce-confirmable, so guild-relay edits from others never
  get re-sent as ours. Carve `contains_strict` is inclusive on internal column edges (a point exactly on a
  shared edge is carved — no zero-width height slit). During a bulk replay the carve republishes once the mesh
  queue drains (one tile-collider rebuild instead of one per finished column).
- **Status (2026-10-22)**: builds; offline captures in Elwynn + Westfall — craters, chunk-corner dig, mound with
  overhang, brush trench all seamless on the real tileset, no sky slits; probe PASS (voxel hit below the old
  ground, height query `None`). Perf (this box, debug-opt build): 2-yd live edit 2–4 ms (1–2 chunks remeshed
  + trimesh colliders), 8-yd live edit ~15 ms (8 chunks); journal replay of 16 chunks ~14 ms in one frame
  (`BUILDS_PER_FRAME` 16).
- **Sky "wounds" fix (2026-10-22)**: two causes, both reproduced offline from the owner's Northshire journal.
  (1) Where the layer ENDS, its surface is a lip `SEED_LIFT` above the real ground it overlaps; a grazing ray
  slipped into that 0.15-yd slit, passed the terrain's carved edge and saw the void under the voxel surface — a
  long thin sky line along the union's outer edge, wavy with the ground. Fix: `add_skirts` hangs a skirt (0.6-yd
  outward ledge, then straight down to 1 yd) from every open boundary edge on a side whose neighbour chunk is
  inactive (`skirt_sides`; a neighbour activating remeshes this chunk and drops it). (2) At a CONCAVE corner of
  the union (both side neighbours carved, the diagonal not) the carve removed ground the mesh does not cover
  (the corner quad belongs to the missing diagonal chunk): the carve now notches inset × inset there (up to three
  bands per column, seams inclusive). Dev audit `WOW_VOXEL_COVERAGE=1` logs carved points no voxel triangle
  covers (0 at the owner's site after the fix, 4 before).
- **Ground effects on the voxel layer (2026-10-22)**: a carved MCNK's clutter tufts are re-placed onto the voxel
  TOP nearest their ADT height (`top_near`, normal z ≥ 0.65 = where the shader draws the ground layers) or dropped
  (pit walls, buried, air); redone per MCNK when the carve or a column's meshes under it change, restored when
  the carve leaves (`monkey_voxel_clutter`, 2 ms/frame budget). Offline captures need `WOW_CLUTTER_DENSITY=2`:
  the capture harness has no clutter otherwise.
- **Server-lane hooks** (`benilla_world::voxel`): push into `MonkeyVoxelNet` — `inbox` (E records, `seq: Some`),
  `confirms` `(nonce, seq)`, `resets` `(map, rx, ry, epoch)`, `region_done` `(map, rx, ry, epoch, last_seq)`;
  drain `outbox` for uplink; read `MonkeyVoxelWorld::pending_local(map)` (resend) and `region_sync(map, rx, ry)`
  (SYNC entries). Encoders: `VoxelEditOp::{to_wire, from_wire, to_hex, from_hex}`, `region_of`.
