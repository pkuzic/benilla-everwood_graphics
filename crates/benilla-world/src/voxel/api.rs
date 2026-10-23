//! MONKEY (voxel / EME seam): [`MonkeyVoxelApi`], the ONE public door into the world voxel layer
//! for editors (DESIGN-EME §2.3). An editor never touches [`MonkeyVoxelWorld`]'s queues or the
//! journal directly; it goes through this system param:
//!
//! - **aim**: a ray (WoW axes) → the first surface hit, on an active voxel chunk or on the raw ADT
//!   ground anywhere else, with the surface normal ([`MonkeyVoxelApi::aim`]).
//! - **submit**: one brush application (stroke / param / mode 0–5) as a LOCAL prediction through
//!   the canonical path — journal, incremental apply, hot remesh — and onto
//!   [`MonkeyVoxelNet::outbox`], which the transport (benilla-app's relay) uplinks
//!   ([`MonkeyVoxelApi::submit`]). The caller may pin the nonce (EME's stroke builder owns its
//!   nonces, so the server echo matches its undo stack).
//! - **preview**: the chunks a brush would touch and the surface ring under it
//!   ([`MonkeyVoxelApi::brush_preview`]).
//! - **queries**: journal / region state, pending predictions, settled.
//! - **local undo** (offline / no server): drop a stroke's still-pending predictions
//!   ([`MonkeyVoxelApi::drop_stroke_local`]); with a server, undo is a server revert (`X` resync).
//!
//! What happens to submitted edits comes back as [`MonkeyVoxelEvent`] messages (confirmed by the
//! server, dropped / rejected, region reset), transport-agnostic.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use super::{
    monkey_voxel_aim_density, ChunkKey, MonkeyVoxelInputClaimed, MonkeyVoxelNet, MonkeyVoxelWorld,
    RegionSync, VoxelEditOp,
};
use crate::terrain_stream::TerrainStreamer;
use benilla_assets::AdtTile;
use benilla_voxel::world::raymarch;

/// What happened to edits on the voxel layer (emitted by the inbox system, in arrival order).
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub enum MonkeyVoxelEvent {
    /// The server accepted OUR prediction `nonce` as `seq` (it stays applied, now canonical).
    Confirmed {
        map: u16,
        nonce: u16,
        seq: u32,
        stroke: u16,
    },
    /// Our prediction `nonce` was dropped: `reason` = the server's `R` code (1 rank, 2 range, 3
    /// rate, 4 invalid, 5 map, 6 stroke), or 0 when it was dropped client-side (stale after a
    /// relog, or a mode the negotiated wire cannot carry).
    Dropped {
        map: u16,
        nonce: u16,
        stroke: u16,
        reason: u8,
    },
    /// `X` REGION-RESET: the region's journal was dropped and is being re-synced (an undo, a
    /// revert).
    RegionReset { map: u16, rx: i16, ry: i16, epoch: u16 },
    /// A LIVE stroke edit by ANOTHER character (`actor` = their low GUID), arriving as it
    /// happens (never a SYNC replay). EME lists these as read-only history rows.
    Remote {
        map: u16,
        actor: u32,
        stroke: u16,
        mode: u8,
        pos: [f32; 3],
        radius: f32,
        seq: u32,
    },
}

/// A surface hit of [`MonkeyVoxelApi::aim`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonkeyVoxelHit {
    /// Hit point, WoW yards.
    pub pos: [f32; 3],
    /// Outward unit surface normal (WoW axes).
    pub normal: [f32; 3],
    /// The hit is on an active voxel chunk (else on the raw ADT ground).
    pub on_voxel: bool,
    /// Distance along the ray, yards.
    pub dist: f32,
}

/// One brush application for [`MonkeyVoxelApi::submit`] (the EDIT3 fields minus map / seq).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonkeyVoxelSubmit {
    /// Brush centre, WoW yards.
    pub pos: [f32; 3],
    /// Radius, yards (0.5–16).
    pub radius: f32,
    /// 0 dig, 1 build, 2 smooth, 3 flatten, 4 paint, 5 roughen.
    pub mode: u8,
    /// Strength /255 (smooth / flatten).
    pub strength: u8,
    /// v3 stroke id (0 = none).
    pub stroke: u16,
    /// v3 mode parameter (flatten = plane offset cm as i16).
    pub param: u16,
    /// Pin the nonce (EME's stroke builder) or let the layer allocate one (`None`).
    pub nonce: Option<u16>,
}

/// What a brush of `radius` at `center` would affect, for a preview.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MonkeyVoxelBrushPreview {
    /// The chunks the edit would activate / change.
    pub chunks: Vec<ChunkKey>,
    /// Points of the brush rim projected onto the surface (WoW yards), closed loop order. A point
    /// with no surface under it in reach keeps the centre's height.
    pub ring: Vec<[f32; 3]>,
    /// Of `chunks`, how many are active (built) now.
    pub active: usize,
}

/// A region's state for history / sync panels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonkeyVoxelRegionState {
    /// Persisted sync point (epoch, last seq).
    pub sync: RegionSync,
    /// Journalled edits whose centre lies in the region (confirmed + pending).
    pub edits: usize,
    /// Of those, local predictions still waiting for the server.
    pub pending: usize,
}

/// The editor seam (see the module docs). Read-mostly; `submit` / `drop_stroke_local` mutate.
#[derive(SystemParam)]
pub struct MonkeyVoxelApi<'w> {
    world: ResMut<'w, MonkeyVoxelWorld>,
    net: ResMut<'w, MonkeyVoxelNet>,
    claim: ResMut<'w, MonkeyVoxelInputClaimed>,
    streamer: Res<'w, TerrainStreamer>,
    adts: Res<'w, Assets<AdtTile>>,
}

impl MonkeyVoxelApi<'_> {
    /// The map the layer mirrors (`None` before the first world entry).
    pub fn map(&self) -> Option<u16> {
        self.world.current_map_u16()
    }

    /// Claim (or release) the brush input: while claimed, the dev key brush (`voxel_dev_brush`)
    /// stays inert so two brushes never fight.
    pub fn claim_input(&mut self, claimed: bool) {
        if self.claim.0 != claimed {
            self.claim.0 = claimed;
        }
    }

    /// The density the aim reads at `p` (negative = solid): the active voxels, else the raw ADT
    /// ground (+ the seed lift).
    pub fn density(&self, p: [f32; 3]) -> f32 {
        monkey_voxel_aim_density(&self.world, &self.streamer, &self.adts, p)
    }

    /// March a ray (WoW axes; `dir` need not be normalised) up to `max_dist` yards: the first
    /// surface hit, voxel or ADT ground, with its normal. `None` = sky / ground not resident.
    pub fn aim(&self, origin: [f32; 3], dir: [f32; 3], max_dist: f32) -> Option<MonkeyVoxelHit> {
        let pos = raymarch(origin, dir, max_dist, 0.25, |p| self.density(p))?;
        let dist = ((pos[0] - origin[0]).powi(2)
            + (pos[1] - origin[1]).powi(2)
            + (pos[2] - origin[2]).powi(2))
        .sqrt();
        Some(MonkeyVoxelHit {
            pos,
            normal: self.normal_at(pos),
            on_voxel: self
                .world
                .data
                .is_active(benilla_voxel::world::chunk_key_at(pos)),
            dist,
        })
    }

    /// The outward surface normal at `p` (central differences of the aim density).
    pub fn normal_at(&self, p: [f32; 3]) -> [f32; 3] {
        let h = 0.25;
        let d = |dx: f32, dy: f32, dz: f32| self.density([p[0] + dx, p[1] + dy, p[2] + dz]);
        let g = [
            d(h, 0.0, 0.0) - d(-h, 0.0, 0.0),
            d(0.0, h, 0.0) - d(0.0, -h, 0.0),
            d(0.0, 0.0, h) - d(0.0, 0.0, -h),
        ];
        let len = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt();
        if len < 1.0e-6 || !len.is_finite() {
            [0.0, 0.0, 1.0]
        } else {
            [g[0] / len, g[1] / len, g[2] / len]
        }
    }

    /// The surface height over `(x, y)` nearest `near_z` within ±`reach` (probed downward from
    /// `near_z + reach`), voxel or ground.
    pub fn surface_z(&self, x: f32, y: f32, near_z: f32, reach: f32) -> Option<f32> {
        raymarch([x, y, near_z + reach], [0.0, 0.0, -1.0], 2.0 * reach, 0.25, |p| {
            self.density(p)
        })
        .map(|p| p[2])
    }

    /// The chunks a brush at `center` with `radius` would touch, and its rim projected onto the
    /// surface (`segments` points).
    pub fn brush_preview(
        &self,
        center: [f32; 3],
        radius: f32,
        segments: usize,
    ) -> MonkeyVoxelBrushPreview {
        let probe = VoxelEditOp {
            pos: center,
            radius: radius.clamp(0.1, 16.0),
            ..Default::default()
        };
        let chunks = probe.touched_chunks();
        let active = chunks
            .iter()
            .filter(|k| self.world.data.is_active(**k))
            .count();
        let reach = radius.max(2.0) + 2.0;
        let ring = (0..segments.max(3))
            .map(|i| {
                let a = i as f32 / segments.max(3) as f32 * std::f32::consts::TAU;
                let (x, y) = (center[0] + radius * a.cos(), center[1] + radius * a.sin());
                let z = self.surface_z(x, y, center[2], reach).unwrap_or(center[2]);
                [x, y, z]
            })
            .collect();
        MonkeyVoxelBrushPreview {
            chunks,
            ring,
            active,
        }
    }

    /// The nonce the layer would allocate next (an editor that numbers its own edits starts here,
    /// so its nonces never collide with this session's earlier predictions).
    pub fn next_nonce(&self) -> u16 {
        self.world.next_nonce
    }

    /// Submit one brush application as a local prediction + uplink candidate. Returns the nonce,
    /// or `None` when no map is loaded or the edit is invalid (radius 0.5–16, finite, mode ≤ 5).
    pub fn submit(&mut self, s: MonkeyVoxelSubmit) -> Option<u16> {
        let e = VoxelEditOp {
            map: 0,
            pos: s.pos,
            radius: s.radius,
            mode: s.mode,
            strength: s.strength,
            nonce: 0,
            stroke: s.stroke,
            param: s.param,
            seq: None,
            local: true,
        };
        let world = &mut *self.world;
        world.local_edit_nonce(&mut self.net, e, s.nonce)
    }

    /// OFFLINE undo: drop every still-pending local prediction of `stroke` on `map` and rebuild
    /// what they touched. Returns the dropped edits (oldest first) so a redo can resubmit them.
    /// With a server, undo is a server revert instead (its `X` resync removes the rows).
    pub fn drop_stroke_local(&mut self, map: u16, stroke: u16) -> Vec<VoxelEditOp> {
        self.world.drop_stroke_local(map, stroke)
    }

    /// Every journalled edit of `map` in canonical order (confirmed by seq, then pending).
    pub fn journal(&self, map: u16) -> &[VoxelEditOp] {
        self.world
            .journal
            .maps
            .get(&map)
            .map_or(&[][..], |j| j.edits())
    }

    /// Local predictions on `map` still waiting for a server seq.
    pub fn pending_local(&self, map: u16) -> Vec<VoxelEditOp> {
        self.world.pending_local(map)
    }

    /// One region's sync point and edit counts.
    pub fn region_state(&self, map: u16, rx: i16, ry: i16) -> MonkeyVoxelRegionState {
        let edits = self.journal(map);
        let mut st = MonkeyVoxelRegionState {
            sync: self.world.region_sync(map, rx, ry),
            ..Default::default()
        };
        for e in edits.iter().filter(|e| e.region() == (rx, ry)) {
            st.edits += 1;
            st.pending += usize::from(e.seq.is_none() && e.local);
        }
        st
    }

    /// Nothing queued or in flight: every wanted chunk built and meshed.
    pub fn settled(&self) -> bool {
        self.world.settled()
    }

    /// Active (built) chunks.
    pub fn active_chunks(&self) -> usize {
        self.world.active_chunks()
    }

    /// The last stream pass (seed, build, mesh, carve), ms — an editor's status-bar "remesh" figure.
    pub fn stream_ms(&self) -> f32 {
        self.world.stream_ms()
    }
}
