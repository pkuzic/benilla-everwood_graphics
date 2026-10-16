# DESIGN — Voxel Terrain (Everwood)

**Status:** design / not started · **Branch:** `everwood-voxel_terrain` (this repo, `pkuzic/benilla-everwood_graphics`)
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
- **Meshing:** **surface nets / dual contouring** for the body (smooth, few verts), moving to
  **transvoxel** at LOD boundaries so chunk LOD levels stitch without cracks. Candidate crates:
  `fast-surface-nets`, `block-mesh`, or a vendored transvoxel — evaluate in P0.
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

## 11. Open decisions (owner)

1. **Voxel size / chunk size** — fidelity vs memory/perf (e.g. 0.33 yd @ 32³). Pick in P0 with a
   benchmark.
2. **Who can sculpt live** — GM-only at first? Players in sanctioned zones later? (permission + rate
   limits).
3. **Meshing crate** — `fast-surface-nets` / `block-mesh` / vendored transvoxel — decide in P0.
4. **Material model** — reuse the existing terrain splat set, or a dedicated voxel material palette?
5. **Volume authoring** — in-game brush only at first, or stand up the `benilla-editor` offline mode
   early?

## 12. Risks

- **Server collision/nav rebuild latency** (P3) is the dominant risk — must stay within the map-tick
  budget. Mitigation: incremental, background, voxel-volumes-only, tiled Recast.
- **Client/server mesh determinism** — mismatched surface = fall-through/rubber-band. Mitigation:
  fixed-point density + one shared meshing algorithm.
- **Scope creep into "rebuild Noggit"** — mitigated by the voxel-first rule and the migration ladder.
- **The ADT-writer gap** — benilla has no write path; budget for it explicitly.

---

*Next: P0 — stand up the `benilla-voxel` format + `benilla-world` mesher behind a feature, render one
static volume. No gameplay/server work until the renderer is proven.*
