# MONKEY voxel v2: server-side voxel awareness (height, LOS, NPC navmesh)

Status: DESIGN (2026-10-22). Nothing is implemented. Inputs: `CONTRACT.md` (this folder), benilla
`DESIGN-VoxelTerrain.md` §"Server half — GROUNDED", `crates/benilla-voxel/src/world.rs` (branch
`everwood-voxel_world`, worktree `G:\mangos\disposable\benilla-voxel-world`), the tortoise-wow core.
House rules apply: every C++ edit sits in `#ifdef ENABLE_ELUNA`, changelog PART 2 (core), PART 3 (Eluna
bindings), PART 6 (Lua that depends on them), COMMANDS.md for any new `.voxel` subcommand.

**The problem.** The server never sees voxel geometry. NPCs path and stand on the old ADT ground: they walk
through mounds and hover over pits. `Map::GetHeight`, LOS, hit-position and the Detour navmesh all ignore
voxels. The owner wants NPC navigation and server collision on the voxel surface.

---------------------------------------------------------------------------------------------------
## 1. How the core does it today (anchors = tortoise-wow `eluna` HEAD de00b4d6)

### 1.1 Height
- `Map::GetHeight` `src/game/Maps/Map.cpp:3348` = `max(TerrainInfo::GetHeightStatic(..), GetDynamicTreeHeight(..))`.
  Because of the **max**, a dynamic model can RAISE the ground (a mound) but can never LOWER it (a pit).
- `TerrainInfo::GetHeightStatic` `GridMap.cpp:776`: `GridMap::getHeight` (the `.map` heightfield,
  `GridMap.cpp:785`) against the vmap down-ray (`VMapManager2::getHeight` `vmap/VMapManager2.cpp:218`), with
  the "under the surface, or the vmap is higher" arbitration at `:813-828`. **This is the choke point.** It is
  also called directly, without going through Map, by water/area code (`GridMap.cpp:986, 1125, 1207`),
  `Creature.cpp:2268` (spawn z, vmaps off), `WaypointManager.cpp:138` and `QueryHandler.cpp:307`. Raw
  `gmap->getHeight` is also read by `getLiquidStatus` (`GridMap.cpp:567`) and `GetAreaInfo` (`:910`).
- `GridMap` (`GridMap.h:50`): V9/V8 grids, `MAP_RESOLUTION 128` per 533.33-yd grid (4.17-yd cells, 4-triangle
  fan around the V8 centre, `getHeightFromFloat` `GridMap.cpp:250`). Heights can be stored as **uint8/uint16**:
  the extractor's `CONF_allow_float_to_int = true` (`tools/extractor/System.cpp:94, 578-588`, steps up to
  `diff/65535`). **Holes are written to the `.map` (`System.cpp:837-854`), but `GridMap::loadData`
  (`GridMap.cpp:71`) never loads them.** Only the mmap generator reads them (`tools/mmap/src/TerrainBuilder.cpp:168, 415`).
- Grids load in `TerrainInfo::Load` (`GridMap.cpp:687`); vmaps and mmaps are unloaded per grid at `:742/:745`.
  `TerrainInfo` is per **map id** and shared by every instance of that map.

### 1.2 LOS / hit position / collision
- `Map::isInLineOfSight` (`Map.cpp:3069`) = `VMapManager2::isInLineOfSight` (`VMapManager2.cpp:145`, WMO/M2
  models only) && `CheckDynamicTreeLoS`. **Terrain never blocks LOS in this core.** vmaps hold no ADT ground.
- `Map::GetLosHitPosition` (`:3078`) = static vmap hit, then `GetDynamicObjectHitPos` (`:3383`).
- Dynamic models: `DynamicMapTree` (`vmap/DynamicTree.cpp`: insert `:123`, `getObjectHitPos` `:209/226`,
  `isInLineOfSight` `:263`, `getHeight` `:280`), one per `Map` instance (`Map::_dynamicTree`, updated in
  `Map::Update` `:896`, guarded by `_dynamicTree_lock`, `Map::InsertGameObjectModel` `:3368` rebalances the
  whole tree on every insert). Entries are `GameObjectModel` (`vmap/GameObjectModel.h:36`, private ctor; it
  is built only from a GameObject's display id: `construct` `GameObjectModel.cpp:156`, `intersectRay` `:180`).
  The geometry is a `VMAP::WorldModel` → `GroupModel::setMeshData(verts, tris)` (`vmap/WorldModel.h:77-90`),
  which builds a BIH. That is the in-memory building block a voxel collider can reuse.
- Players: the client is authoritative for its own movement, so players already stand on voxels. Anticheat
  only matters for abrupt vertical moves (`Anticheat/Movement/Movement.cpp:593` OVERSPEED_Z, see DESIGN-VoxelTerrain).

### 1.3 Pathfinding (mmaps)
- `MMapManager` (`Maps/MoveMap.h`, `.cpp`): one `dtNavMesh` per map id (`MMapData`), **one Detour tile per
  533.33-yd ADT grid** (`navMeshParams.tileWidth = GRID_SIZE`, `tools/mmap/src/MapBuilder.cpp:377`). Tiles
  load with the grid: `loadMap` (`MoveMap.cpp:121`, `addTile` `:191`) and `unloadMap(x,y)` (`:208`,
  `removeTile` `:232`). Files: `data/mmaps/MMMYYXX.mmtile` (2295 files, 2.4 GB), header `MMAP_VERSION 6`
  (`MoveMapSharedDefines.h`). Each thread gets its own `dtNavMeshQuery` (`GetNavMeshQuery` `:314`), but the
  `dtNavMesh` is shared. **No lock guards queries against `addTile/removeTile`.** That is safe today only
  because tiles change on grid load.
- Query users: `PathFinder` (`PathFinder.cpp:88` acquire, `findNearestPoly :169`, `findPath :350/:384`,
  `findStraightPath :421`, `getPolyHeight :823`), `Map::GetWalkHitPosition` (`Map.cpp:3128`, navmesh
  raycast), `Map::GetWalkRandomPosition` (`:3260`), GM `.mmap near/path` (`Commands.cpp:13847/13919`).
  Callers run on map threads, `MapUpdate.ObjectsUpdate.MaxThreads = 2` sub-threads and the instanced map pool.
- **Generator is in tree:** `tools/mmap/src` (MoveMapGen, built only with `USE_EXTRACTORS`, compiled with
  `-DMMAP_GENERATOR`). `TileBuilder::buildTile` (`TileBuilder.cpp:253`) = `TerrainBuilder::loadMap` (.map +
  holes) + `loadVMap` (`TerrainBuilder.cpp:588`) + off-mesh, then `buildMoveMapTile` (`:294`). Each ADT is
  cut into **25×25 sub-tiles of 80 cells × 0.2667 yd = 21.33 yd** (`:322-327`). Continents use `ch 0.25`,
  `agentRadius 0.2`, `agentHeight 1.5`, climb 1.8 terrain / 1.2 model, slope 75°. The custom ledge and water
  filters are at `:117-200` and `:466-500`. The sub-tiles go through `rcMergePolyMeshes` (≈`:606`;
  `dep/recastnavigation/Recast/Source/RecastMesh.cpp:1311` **recomputes adjacency** across sub-tile seams) and
  are written as ONE dtMeshTile (≈`:756`). The generator calls `exit(0)` on vertex overflow, so it cannot be
  linked into mangosd as-is.
- `MMAP_GENERATOR` only adds NON-virtual accessors (`WorldModel.h:70,107,132`, `MapTree.h:93`,
  `VMapManager2.h:110`) and logging macros (`VMapDefinitions.h:38`). It does not change any class layout.
  Exposing those accessors to mangosd is therefore ODR-safe.

### 1.4 Edits today
`bin/Debug/lua_scripts/MonkeyVoxelServer.lua`: `opEdit` (`:391`) INSERTs `voxel_edit` (`:412`; x/y/z/radius are
DOUBLE holding exact float32). `applyRevert` (`:476`) flags rows and bumps region epochs. The core has no voxel code.

---------------------------------------------------------------------------------------------------
## 2. Options

### 2.1 Geometry source (height + LOS)
| | What | Effort | Runtime cost | Risk |
|---|---|---|---|---|
| **a. C++ density replica** | Port `world.rs` seed + CSG + smooth + surface-nets to C++, one store per map id, rebuilt from `voxel_edit` | M (≈1.2k lines, mostly mechanical) | 32 KB per active chunk (i8 32³) + about 2k tris per surface chunk; replay ≈ ms per edit | Determinism (§3); smooth closure hazard |
| b. Synthesized models in `DynamicTree` | Per-chunk `WorldModel` wrapped in a new `GameObjectModel` factory, inserted into each `Map::_dynamicTree` | M | Full tree `balance()` on every insert, per instance | Cannot express pits (`max` in `Map::GetHeight`), per-instance duplication, misses `GetHeightStatic` direct callers |
| a+mesh (recommended) | (a) produces chunk triangles (surface-nets, same algorithm as the client), stored in OUR per-map BIH (`VMAP::GroupModel::setMeshData`). Queried from `GetHeightStatic` / LOS hooks, NOT through DynamicTree | M+ | BIH ray per query only when the ray's AABB meets an active chunk (a hash lookup otherwise) | Low once golden vectors pass |

Why not DynamicTree: a pit needs the **carve** (ignore `.map` ground in carved columns). That can only
happen inside `GetHeightStatic`, the choke point that every height path reaches. Once we hook there, the same
store answers LOS too.

**Height rule (server mirror of the client carve, CONTRACT §1):**
`terrainZ = carved(x,y) ? voxelDown(x,y,z+2) : max(grid(x,y), voxelDown(x,y,z+2))`.
`terrainZ` replaces `mapHeight` in `GetHeightStatic` (`GridMap.cpp:785`), and the existing vmap
arbitration (`:813-828`) stays unchanged. A column is carved when it is a surface column of the active set,
i.e. the same activation rule as the client (edit bbox chunks, plus the column's surface chunks when within ±1).
Because the seed sits 0.15 yd above the ADT, unedited voxel ground and the `.map` ground meet within 0.15 yd at the carve edge.

**LOS rule:** vanilla terrain never blocks LOS. If the whole voxel mesh were opaque, a hill crest inside an
active chunk would block LOS, while the same crest one chunk away would not. So a voxel hit counts only
if the hit point is more than 0.3 yd above the raw seeded ground at that column. In practice only **built
material** blocks LOS. A mob in a pit therefore still sees out (consistent with vanilla). This is an owner question (§6).

### 2.2 Navmesh
| | What | Effort | Latency | Risk |
|---|---|---|---|---|
| **c0. Avoid stopgap** | On edit, `dtNavMesh::setPolyFlags(poly, 0)` for polys overlapping carved columns (re-applied on tile load). NPCs route around the region, and one standing inside gets NOPATH (it idles / evades) | S | instant | Polys are up to 12 yd (`maxEdgeLen`), so the blocked area exceeds the edit. Needs the query lock too |
| **c1. Full-ADT rebuild, in-process, background** | Shared Recast pipeline (refactored from `TileBuilder::buildMoveMapTile`) runs on a worker thread. The terrain triangles are clipped by the carve rects, voxel tris are flagged as terrain, plus vmaps + off-mesh. The result hot-swaps the ADT's tile | M-L | debounce 5 s + build of one ADT (unknown: **measure in P0**, guess 3-30 s) | Seams with the 8 neighbour tiles: OK if the pipeline is byte-identical to the baked one (P0 check) |
| c1-out. Same, out-of-process | MoveMapGen gets `--voxelInput file --outDir dir --tile`, and mangosd spawns it, then loads the `.mmtile` | M | + process start | Crash-isolated, but needs the generator binary in the OVH Docker image + process spawn on Linux |
| c2. Sub-tile cache | c1, plus keep the 625 sub-tile `rcPolyMesh`/`rcPolyMeshDetail` of an edited ADT in memory. Re-run only the affected 21.33-yd sub-tiles (an r≤16 edit touches ≤ 2×2 plus border), then re-merge and `dtCreateNavMeshData` | +M | ≈0.3-1 s | Memory ≈10-30 MB per edited ADT. First touch = full build |
| c3. Re-bake mmaps with 21.33-yd Detour tiles | `tileWidth = 80·cs`, one dtMeshTile per sub-tile; runtime rebuilds 1-4 small tiles and Detour links neighbours natively | L (re-bake all maps, MoveMap loader, OVH redeploy 2.4 GB) | ≈50-150 ms | Format change on live, hours of generation |
| c4. Off-mesh links only | — | | | Cannot represent a new walkable surface (mound top, pit floor). **Rejected** |
| c5. `dtTileCache` obstacles | — | | | Needs tile-cache layers (re-bake). Obstacles only cut, never add a surface. **Rejected** |

Common requirement for c0-c3: a per-map `std::shared_mutex navLock` in `MMapData`. Queries take it shared
(PathFinder ctor/calculate, `Map.cpp:3128/3260`, `.mmap` commands), and the tile swap or flag edit takes it
exclusive for the few ms of `removeTile` + `addTile`. Stale `dtPolyRef`s die by Detour salt, so Detour
returns failure and `PathFinder` handles that as NOPATH.

**Persistence:** a rebuilt tile is written to `<MonkeyVoxel.Nav.Dir>/MMMYYXX.mmtile` plus a `.sig` sidecar
(hash of the base tile's size/mtime + the ordered seq list of non-reverted edits touching the ADT + pipeline
version). `MMapManager::loadMap` prefers the override when its sig matches. Otherwise it loads the base tile,
applies c0 flags, and queues a rebuild. The dir is **per server cwd**: the voxtest stack and the main
server share `DataDir`, so nothing may ever be written into `data/mmaps`.

### 2.3 How edits reach the core
| | |
|---|---|
| Core reads `voxel_edit` itself | Authoritative, survives `.reload eluna`. At startup (or lazily per region when its grid loads): `SELECT seq,map,x,y,z,radius,mode,strength FROM voxel_edit WHERE reverted_at IS NULL AND map IN (..) ORDER BY seq`. It must check `information_schema` first and **never abort** when the table is missing (a missing table aborts mangosd, CLAUDE.md). |
| Eluna push (new bindings, PART 3) | `MonkeyVoxelApply(map, seq, x, y, z, radius, mode, strength)` from `opEdit` after the INSERT. `MonkeyVoxelReloadRegion(map, rx, ry)` from `applyRevert` per touched region. `MonkeyVoxelStatus(map)` → active chunks, pending builds, nav queue (for tests and `.voxel nav`). Calls only enqueue (mutex queue), never compute on the world thread. |
| Core polls `max(seq)` | No Lua coupling but DB polling every N s. Fallback only. |

Recommended: **DB load at start + Eluna push for live edits**. Seq dedupe makes both idempotent: a seq ≤ the
region's applied max is ignored, and a gap triggers a region reload.

---------------------------------------------------------------------------------------------------
## 3. Determinism (client ⇄ server)

What must be shared **exactly** (constants and op order from `world.rs` / `lib.rs`): `VOXEL_SIZE 0.5`,
`CHUNK 32`, `DENSITY_SCALE 64` (i8 SDF in 1/64 voxel units, ±2 voxels saturating), `SEED_LIFT 0.15`, lattice
`I = round(wow/0.5)`, owner rule `chunk = I.div_euclid(32)`, `lattice_bbox` (centre ± r ± 1, floor/ceil),
`seed_density` (holes = `AIR 127`), dig `max(old, -brush)`, build `min(old, brush)`, smooth (Jacobi over the
pre-edit gather, `((((a+b)+c)+d)+e)+f)/6`, `old*(1-s) + avg*s`), quantize `roundf` (half away from zero = Rust
`round`) then clamp then cast. In C++, every op is **float, never double**, with `x*x` for `powi(2)` and `sqrtf`.

Hazards found while reading the code:
1. **Seed heights differ by source.** The client uses ADT MCVT positions with barycentric `triangle_z_at`
   (`benilla-formats/src/terrain.rs:132`). The server has `.map` V9/V8 data, possibly uint16/uint8-quantized
   (error up to ≈0.03 yd ≈ 4 density units), fed through a differently-ordered plane formula. Mitigation: (i)
   port `height_at` (same 4-fan triangles, same corner order) over the V9/V8 arrays instead of calling
   `GridMap::getHeight`; (ii) re-extract `maps/` with `-f 0` (float) for voxel-enabled maps if P0 shows
   quantized grids under build areas; (iii) load holes. Goal: exact, with tolerance ≤ 1 density unit
   (0.008 yd) as the floor.
2. **Smooth is history/partition dependent on the client.** `build_job` (`world.rs:545`) reads chunks OUTSIDE the
   rebuilt set at their *current* density, not their density at that seq. Rebuilding a subset can therefore
   differ from a full replay. The server must use the canonical definition **replay over the transitive closure
   of chunks linked by smooth edits**, and the client lane should adopt the same rule (benilla fix).
   Dig/build are unaffected.
3. **FMA contraction.** The live Linux build uses `-march=native -O3` (`CMakeLists.txt:435`), and GCC
   contracts `a*b+c` into FMA by default. The voxel TUs need `-ffp-contract=off` (MSVC: keep `/fp:precise`,
   no `/fp:contract`). Rust never contracts.
4. Mesh vs isosurface: the server's height must come from the same surface-nets triangles the client
   collides with (a port of `fast_surface_nets`: vertex = mean of edge crossings, quads per sign-changing
   edge), not from a trilinear march. Tolerance for height parity: ≤ 0.05 yd.

**Test strategy (golden vectors):**
- G1 (exact): add a Rust test/example in `benilla-voxel`, `export_golden`. It takes the test `ground(x,y)`
  plus `synth_edits(seed=fixed)` (dig/build/smooth, r 0.5-16, chunk-straddling, negative coords) and writes
  `golden/edits.bin` (wire bodies + seq), `golden/heights.bin` (per-column seeded f32) and
  `golden/chunks.bin` (key + 32 768 i8 per chunk, canonical closure order) plus a SHA-256 manifest. A C++
  test exe (`tools/MonkeyVoxelGolden/`) built in the server build dir feeds the SAME heights and must
  reproduce `chunks.bin` **byte-for-byte**. It also emits surface-nets meshes and compares vertices within 1e-5.
- G2 (source parity): benilla dumps `terrain_height_at` at every lattice column of 4 sample ADTs (Elwynn,
  Durotar, a hole-bearing cave mouth, an ADT edge). The C++ height port over `data/maps/*.map` must match
  within 1 density unit. The report lists quantized grids.
- G3 (query parity): 10k random down-rays and segment LOS through the G1 world. Client `raymarch`/collider
  vs server BIH: |Δz| ≤ 0.05 yd, LOS agreement ≥ 99.9 % (disagreements only on grazing rays).
- All three run in CI-less mode as `py -3 disposable\voxel-v2\server_harness\run_golden.py` (builds nothing,
  runs the two exes, diffs).

---------------------------------------------------------------------------------------------------
## 4. Recommended plan

New code lives in `src/game/Maps/MonkeyVoxel/` (new files, whole-file `#ifdef ENABLE_ELUNA`), gated by
`mangosd.conf`: `MonkeyVoxel.Enable = 0`, `MonkeyVoxel.Maps = "0,1"`, `MonkeyVoxel.Nav.Enable = 0`,
`MonkeyVoxel.Nav.Dir = "voxnav"`, `MonkeyVoxel.Nav.DebounceMs = 5000`, `MonkeyVoxel.MaxChunks = 8192`.
Off = byte-for-byte today's behaviour. v1 targets **non-instanced continents only** (one `Map` per map id).

### Build/test isolation (never disturb the shared server)
- A git worktree on branch `everwood-voxel_servernav` off `eluna` (the main tree is dirty with other lanes).
- Its own CMake build dir `G:\mangos\disposable\voxnav-build` (`-DSCRIPT_LIB_ELUNA=ON -DALLOW_TURTLE_ADDONS=ON
  -DUSE_EXTRACTORS=ON` for MoveMapGen + golden exe). **Never** build into or copy over `bin/Debug/mangosd.exe`.
- Deploy = copy `mangosd.exe` → `G:\mangos\disposable\voxtest-server\mangosd_vox.exe`. Start/stop only via
  `voxtest.ps1` (ports 3725/8086, DBs `voxtest_*`, own `lua_scripts`, `voxnav/` under its cwd). Stop only the
  PIDs in `voxtest.pids`. The conductor applies `sql/custom/voxel_server.sql` to `voxtest_world` before any start.
- Low PC load while the owner plays: run the builds and the P0 MoveMapGen timing at below-normal priority (`/MP2`).

### Phases and lanes (disjoint files; the conductor writes the API header `MonkeyVoxelMgr.h` FIRST)
**P0: measure (1 lane, no core edits)**
- L0a (benilla repo): G1/G2 exporters + the smooth-closure fix (`world.rs` `build_job`), with tests.
- L0b (tools only, scratch dir): time `MoveMapGen --tile` for 3 continent ADTs. Verify that a rebuilt tile is
  byte-identical to the shipped `data/mmaps` one (proves the pipeline equals the bake, so seams are safe).
  List the quantized `.map` grids in build areas.
- Exit: G1 C++ skeleton passes byte-exact, timings known → choose c1 vs c2 for P2.

**P1: replica + height + LOS + avoid (3 lanes in parallel)**
- L1 (new files only): `MonkeyVoxelField.{h,cpp}` (seed from V9/V8 port + holes, CSG, smooth closure,
  surface-nets) and `MonkeyVoxelMgr.{h,cpp}` (per-map store, activation/carve set, DB load, enqueue API,
  1 worker thread, immutable per-chunk `shared_ptr<const ChunkGeom>` with BIH, `GetHeight/IsInLOS/GetHitPos/
  IsCarved`). Plus `tools/MonkeyVoxelGolden/`. Per-file `-ffp-contract=off` in `src/game/CMakeLists.txt` (conductor).
- L2 (existing core files): `GridMap.cpp/.h` (load holes; `GetHeightStatic` hook; `getLiquidStatus`/
  `GetAreaInfo` read `terrainZ`), `Map.cpp` (`isInLineOfSight`, `GetLosHitPosition` add voxel test;
  `:3128/:3260` take the nav lock), `MoveMap.h/.cpp` (`navLock`, `SwapTile(map,x,y,data)`,
  `SetPolyFlagsInRect` for c0, re-apply on `loadMap`), `PathFinder.cpp` (shared lock), `Commands.cpp`
  `HandleGPSCommand :6826` (print `voxel z / carved`), `World.cpp` config.
- L3 (Eluna + Lua + docs): `methods/VMangos/GlobalMethods.h` bindings (§2.3); `MonkeyVoxelServer.lua` calls
  them in `opEdit`/`applyRevert`, adds `.voxel probe` (map:GetHeight + `IsWithinLoS` at the GM) and `.voxel nav`
  (status) under the rank ≥ 2 nil-return gate; COMMANDS.md; changelog PART 2/3/6; e2e scripts.

**P2: navmesh rebuild (2 lanes)**
- L4 (tools/mmap + vmap headers): extract `BuildNavTileData(MeshData&, cfg) → bytes` from
  `TileBuilder::buildMoveMapTile` into `tools/mmap/src/NavPipeline.{h,cpp}` (no printf/exit; errors returned).
  MoveMapGen uses it, and the output must be unchanged (re-run the L0b byte check). Widen the accessors in
  `WorldModel.h/MapTree.h/VMapManager2.h` to `#if defined(MMAP_GENERATOR) || defined(ENABLE_ELUNA)`.
- L5 (new files): `MonkeyVoxelNav.{h,cpp}`. Debounced queue per ADT. The worker builds MeshData via
  TerrainBuilder (own VMapManager2 instance) + carve-clipped terrain (port of benilla `voxel_clip.rs`
  Sutherland-Hodgman; drop-by-centroid leaves 4-yd bridges over pits) + voxel tris (terrain area). It calls
  `NavPipeline` and writes the override + sig, then `MMapManager::SwapTile` and clears c0 flags for that ADT. c2
  sub-tile cache only if P0 timings > 10 s.

**P3: hardening.** Instanced maps (a swap under the lock covers all instances sharing the navmesh), memory cap
eviction, Linux build on the OVH stack (`-ffp-contract=off` check), live rollout behind `MonkeyVoxel.Enable`.

### Acceptance tests (voxtest stack, scripted via `disposable/voxel-v2/e2e` node client + `.voxel probe`)
1. **Height/mound:** GM `.mvox` build r=4 at P (flat Elwynn). `.gps` / `.voxel probe` at P returns ADT+≈4 yd
   (±0.1). 3 yd off the mound it returns ADT+0.15 (±0.05). With `MonkeyVoxel.Enable=0`: ADT (unchanged).
2. **Height/pit:** dig r=5 at Q. `GetHeight(Q, z=old+2)` returns the pit floor (≈ old−5, ±0.1), never the old ground.
3. **LOS:** a GM and an NPC 20 yd apart with an r=4 mound (top ≥ 2 yd above both heads) between them →
   `IsWithinLoS` false. Revert (`.voxel undo`) → true within 2 s. A dug pit between them → still true (vanilla rule).
4. **NPC around a mound:** spawn an NPC on one side, `creature:MoveTo(id, far side, genPath=true)`, then sample
   its position every 250 ms. P1: no sample lies inside the mound footprint (c0: it walks around the blocked
   polys, or reports NOPATH, never through). P2: after the rebuild (≤ debounce + measured build time) it walks
   OVER a low mound (r=2, slope < 75°) and AROUND a steep one. Its z is always within 0.3 yd of `GetHeight`.
5. **NPC into a pit:** a ramp dig (3 overlapping digs descending) next to an NPC, with the target on the pit
   floor. P2: the path ends on the pit floor, and every sample z is within 0.3 yd of the voxel height (no hovering).
6. **Restart persistence:** restart the voxtest mangosd. Tests 1-5 still pass with **no** rebuild logged (sig hit).
   Add an edit → exactly one rebuild of that ADT.
7. **Seams:** a path that crosses the ADT border next to a rebuilt tile succeeds (`.mmap path`).
8. **Off switch:** `MonkeyVoxel.Enable = 0` → golden `.gps` values identical to the main server at the same points.

---------------------------------------------------------------------------------------------------
## 5. Risks
- **Navmesh swap races.** Today nothing locks Detour against tile changes. Missing one query site means
  crashes under load. Mitigation: grep every `GetNavMeshQuery`/`GetNavMesh` user (5 sites today) and
  assert the lock is held in debug builds.
- **Rebuild latency / CPU.** One ADT = 625 Recast passes. On the live VPS, a busy GM could keep a core
  busy. Mitigation: debounce, 1 worker at low priority, c2 if needed, c0 always in front.
- **Seam mismatch** if the runtime pipeline differs from the bake (generator version drift vs the shipped
  tiles). Mitigated by the P0 byte-identity check. If it fails, re-bake the continents once with the refactored pipeline.
- **Determinism drift** client↔server (seed source, smooth partition, FMA). Mitigated by G1-G3 in the harness, re-run on every voxel PR.
- **Height-hook blast radius.** `GetHeightStatic` is hot (every creature move). The fast path is one
  hash lookup of `(map, column)` → "no voxels". Behind `MonkeyVoxel.Enable`.
- **Startup replay** of a large `voxel_edit` table (10k+ edits). Mitigation: lazy per-region build on grid
  load, with c0 flags until ready.
- **Memory** (8192 chunks × 32 KB ≈ 256 MB worst case) → cap + evict regions with no loaded grid.
- Players are unaffected (client-authoritative). The OVERSPEED_Z exposure is unchanged.

## 6. Open questions for the owner
1. NPC behaviour before the rebuild lands: avoid the region (c0, NPCs may idle or evade inside it), or ignore voxels (walk through) until rebuilt?
2. Latency target for "NPCs follow new terrain": is ~5-30 s after the last brush stroke OK (c1), or near-instant (c2/c3, much more work, c3 = re-bake all mmaps)?
3. LOS: should only **built** material block LOS (a mob in a pit sees out, like vanilla hills), or the whole voxel surface?
4. Scope: continents only (maps 0/1 + custom) for v1, or dungeons/instances too?
5. May we re-extract `maps/*.map` as float (`-f 0`) for voxel-enabled maps if P0 finds quantized grids (≈+100 MB, OVH redeploy)?
6. Live rollout: OK to ship the core behind `MonkeyVoxel.Enable = 0` on OVH main first and enable after the voxtest acceptance?
7. Steep slopes: voxel walls default to the terrain rule (walkable ≤ 75°, climb 1.8 yd). Should built walls be stricter (model rule 61° / 1.2 yd) so NPCs can't scale GM-built ramparts?
