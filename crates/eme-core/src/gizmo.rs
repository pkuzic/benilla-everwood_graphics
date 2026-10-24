//! The maths of the Objects tool: selection rays, gizmo handles, drag solvers and snapping
//! (DESIGN-EME §5). Engine-free and re-implemented from scratch; every vector is WoW axes
//! (+X north, +Y west, +Z up, yards, radians). Nothing here knows about egui, Bevy or the network.
//!
//! - [`EmeObb`] / [`EmeObb::ray`]: the oriented box a streamed object occupies, and the ray test
//!   the viewport uses to click-select it.
//! - [`EmeGizmoFrame`]: where the gizmo sits and how big it is (a constant on-screen size).
//! - [`hit_test`]: which handle of the gizmo the cursor ray touches.
//! - [`EmeGizmoDrag`]: a grabbed handle; [`EmeGizmoDrag::solve`] turns the current cursor ray into
//!   an [`EmeGizmoDelta`] (move / yaw / uniform scale), snapped by [`EmeSnap`].
//! - [`apply_delta`]: the delta applied to a set of object transforms.

use std::f32::consts::{PI, TAU};

/// A WoW-axes vector.
pub type V3 = [f32; 3];

pub(crate) fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub(crate) fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub(crate) fn mul(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
pub(crate) fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub(crate) fn len(a: V3) -> f32 {
    dot(a, a).sqrt()
}
/// `a` scaled to unit length (the zero vector stays zero).
pub fn normalize(a: V3) -> V3 {
    let l = len(a);
    if l > 1.0e-9 {
        mul(a, 1.0 / l)
    } else {
        [0.0; 3]
    }
}

/// An angle wrapped into `[0, 2π)`.
pub fn wrap_tau(a: f32) -> f32 {
    let r = a.rem_euclid(TAU);
    // `rem_euclid` of a tiny negative number can round up to exactly TAU.
    if r >= TAU {
        0.0
    } else {
        r
    }
}

/// An angle wrapped into `(-π, π]`.
pub fn wrap_pi(a: f32) -> f32 {
    let r = wrap_tau(a);
    if r > PI {
        r - TAU
    } else {
        r
    }
}

// ---------------------------------------------------------------------------------------------
// Boxes and selection rays
// ---------------------------------------------------------------------------------------------

/// An oriented box: a centre, three orthonormal axes and the half extents along them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeObb {
    /// Centre, WoW yards.
    pub center: V3,
    /// Orthonormal axes.
    pub axes: [V3; 3],
    /// Half extents along `axes`, yards.
    pub half: V3,
}

/// The smallest half extent a pickable box is given, so a flat or empty model can still be hit.
pub const MIN_PICK_HALF: f32 = 0.3;

impl EmeObb {
    /// An axis-aligned box around `center` with half extents `half`.
    pub fn axis_aligned(center: V3, half: V3) -> Self {
        Self {
            center,
            axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            half,
        }
    }

    /// A small cube at `p` (an object whose model is not known).
    pub fn point(p: V3, half: f32) -> Self {
        Self::axis_aligned(p, [half; 3])
    }

    /// This box with every half extent at least [`MIN_PICK_HALF`].
    pub fn padded(&self) -> Self {
        Self {
            half: [
                self.half[0].max(MIN_PICK_HALF),
                self.half[1].max(MIN_PICK_HALF),
                self.half[2].max(MIN_PICK_HALF),
            ],
            ..*self
        }
    }

    /// The eight corners.
    pub fn corners(&self) -> [V3; 8] {
        let mut out = [[0.0; 3]; 8];
        for (i, c) in out.iter_mut().enumerate() {
            let sx = if i & 1 == 0 { -1.0 } else { 1.0 };
            let sy = if i & 2 == 0 { -1.0 } else { 1.0 };
            let sz = if i & 4 == 0 { -1.0 } else { 1.0 };
            let mut p = self.center;
            p = add(p, mul(self.axes[0], sx * self.half[0]));
            p = add(p, mul(self.axes[1], sy * self.half[1]));
            p = add(p, mul(self.axes[2], sz * self.half[2]));
            *c = p;
        }
        out
    }

    /// The twelve edges as corner index pairs of [`Self::corners`].
    pub const EDGES: [(usize, usize); 12] = [
        (0, 1),
        (2, 3),
        (4, 5),
        (6, 7),
        (0, 2),
        (1, 3),
        (4, 6),
        (5, 7),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ];

    /// Whether `p` lies inside the box.
    pub fn contains(&self, p: V3) -> bool {
        let r = sub(p, self.center);
        (0..3).all(|i| dot(r, self.axes[i]).abs() <= self.half[i])
    }

    /// The distance along the ray `origin + t * dir` (`dir` unit) at which it enters the box, or
    /// `None` when it misses (or the box is wholly behind the origin). A ray that starts inside
    /// the box hits at `0`.
    pub fn ray(&self, origin: V3, dir: V3) -> Option<f32> {
        let rel = sub(origin, self.center);
        let mut t_min = 0.0f32;
        let mut t_max = f32::INFINITY;
        for i in 0..3 {
            let o = dot(rel, self.axes[i]);
            let d = dot(dir, self.axes[i]);
            let h = self.half[i];
            if d.abs() < 1.0e-9 {
                if o.abs() > h {
                    return None;
                }
                continue;
            }
            let mut t0 = (-h - o) / d;
            let mut t1 = (h - o) / d;
            if t0 > t1 {
                std::mem::swap(&mut t0, &mut t1);
            }
            t_min = t_min.max(t0);
            t_max = t_max.min(t1);
            if t_min > t_max {
                return None;
            }
        }
        Some(t_min)
    }

    /// The volume (used to prefer the smaller of two boxes hit at the same distance).
    pub fn volume(&self) -> f32 {
        8.0 * self.half[0] * self.half[1] * self.half[2]
    }

    /// This box after its object moved from `from` to `to` (a ghost preview): rotated about the
    /// vertical through `from.pos` by the yaw change, scaled by the scale ratio, then carried to
    /// `to.pos`.
    pub fn moved(&self, from: &EmeObjXform, to: &EmeObjXform) -> Self {
        let dyaw = to.o - from.o;
        let ratio = if from.scale.abs() > 1.0e-6 {
            to.scale / from.scale
        } else {
            1.0
        };
        let rel = mul(sub(self.center, from.pos), ratio);
        let center = add(to.pos, rot_z(rel, dyaw));
        Self {
            center,
            axes: [
                rot_z(self.axes[0], dyaw),
                rot_z(self.axes[1], dyaw),
                rot_z(self.axes[2], dyaw),
            ],
            half: mul(self.half, ratio),
        }
    }
}

/// `v` rotated by `a` radians about +Z (counter-clockwise seen from above).
pub fn rot_z(v: V3, a: f32) -> V3 {
    let (s, c) = a.sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c, v[2]]
}

/// A streamed object the viewport can pick: its server guid, template entry, origin and box.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeWorldObject {
    /// Spawn guid (low).
    pub guid: u32,
    /// Template entry.
    pub entry: u32,
    /// Display name when the client knows it (its template cache).
    pub name: Option<String>,
    /// The object's origin, WoW yards.
    pub pos: V3,
    /// The model's oriented bounds in the world.
    pub obb: EmeObb,
}

/// The nearest object the ray hits within `max_dist`: `(guid, distance)`. A tie within a few
/// centimetres goes to the smaller box (a chair on a big rug).
pub fn pick_object(objects: &[EmeWorldObject], origin: V3, dir: V3, max_dist: f32) -> Option<(u32, f32)> {
    let mut best: Option<(u32, f32, f32)> = None;
    for o in objects {
        let Some(t) = o.obb.padded().ray(origin, dir) else {
            continue;
        };
        if t > max_dist {
            continue;
        }
        let vol = o.obb.padded().volume();
        let better = match best {
            None => true,
            Some((_, bt, bv)) => t < bt - 0.05 || ((t - bt).abs() <= 0.05 && vol < bv),
        };
        if better {
            best = Some((o.guid, t, vol));
        }
    }
    best.map(|(g, t, _)| (g, t))
}

// ---------------------------------------------------------------------------------------------
// Ray helpers
// ---------------------------------------------------------------------------------------------

/// The distance along the ray at which it crosses the plane through `p` with normal `n`; `None`
/// when parallel or behind.
pub fn ray_plane(o: V3, d: V3, p: V3, n: V3) -> Option<f32> {
    let denom = dot(d, n);
    if denom.abs() < 1.0e-6 {
        return None;
    }
    let t = dot(sub(p, o), n) / denom;
    (t >= 0.0).then_some(t)
}

/// Distance between two segments `p1 + s*d1` (`s` in 0..=1) and `p2 + t*d2` (`t` in 0..=1), and
/// the two parameters (Ericson, Real-Time Collision Detection 5.1.9).
fn segment_segment(p1: V3, d1: V3, p2: V3, d2: V3) -> (f32, f32, f32) {
    let r = sub(p1, p2);
    let a = dot(d1, d1);
    let e = dot(d2, d2);
    let f = dot(d2, r);
    let (mut s, mut t);
    if a <= 1.0e-12 && e <= 1.0e-12 {
        s = 0.0;
        t = 0.0;
    } else if a <= 1.0e-12 {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = dot(d1, r);
        if e <= 1.0e-12 {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = dot(d1, d2);
            let denom = a * e - b * b;
            s = if denom > 1.0e-12 {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            t = (b * s + f) / e;
            if t < 0.0 {
                t = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t > 1.0 {
                t = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            }
        }
    }
    let c1 = add(p1, mul(d1, s));
    let c2 = add(p2, mul(d2, t));
    (len(sub(c1, c2)), s, t)
}

/// Distance from the ray to the segment `a..b`, and where along the ray it is closest.
pub fn ray_segment_distance(o: V3, d: V3, a: V3, b: V3) -> (f32, f32) {
    const REACH: f32 = 4000.0;
    let (dist, s, _t) = segment_segment(o, mul(d, REACH), a, sub(b, a));
    (dist, s * REACH)
}

/// Distance from the point `p` to the ray (clamped to the ray's forward half).
pub fn point_ray_distance(o: V3, d: V3, p: V3) -> f32 {
    let t = dot(sub(p, o), d).max(0.0);
    len(sub(p, add(o, mul(d, t))))
}

// ---------------------------------------------------------------------------------------------
// Snapping
// ---------------------------------------------------------------------------------------------

/// The grid steps the toolbar offers, yards.
pub const SNAP_GRIDS: [f32; 3] = [0.1, 0.5, 1.0];
/// The rotation steps, degrees.
pub const SNAP_ANGLES: [f32; 2] = [5.0, 15.0];
/// The scale step used while snapping.
pub const SNAP_SCALE: f32 = 0.05;

/// Snap settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeSnap {
    /// Snapping is on.
    pub enabled: bool,
    /// Length step, yards.
    pub grid: f32,
    /// Rotation step, degrees.
    pub angle_deg: f32,
}

impl Default for EmeSnap {
    fn default() -> Self {
        Self {
            enabled: false,
            grid: SNAP_GRIDS[1],
            angle_deg: SNAP_ANGLES[0],
        }
    }
}

impl EmeSnap {
    /// A length rounded to the grid (unchanged when snapping is off).
    pub fn length(&self, v: f32) -> f32 {
        if self.enabled && self.grid > 0.0 {
            (v / self.grid).round() * self.grid
        } else {
            v
        }
    }

    /// An angle (radians) rounded to the rotation step.
    pub fn angle(&self, a: f32) -> f32 {
        if self.enabled && self.angle_deg > 0.0 {
            let step = self.angle_deg.to_radians();
            (a / step).round() * step
        } else {
            a
        }
    }

    /// A scale rounded to [`SNAP_SCALE`].
    pub fn scale(&self, s: f32) -> f32 {
        if self.enabled {
            (s / SNAP_SCALE).round() * SNAP_SCALE
        } else {
            s
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The gizmo
// ---------------------------------------------------------------------------------------------

/// What the gizmo does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EmeGizmoMode {
    /// Arrows and plane squares.
    #[default]
    Translate,
    /// A yaw ring (the server stores only the orientation about the vertical).
    Rotate,
    /// A uniform scale grip.
    Scale,
}

/// Whether the arrows follow the world axes or the object's own yaw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EmeGizmoSpace {
    /// World axes.
    #[default]
    World,
    /// The object's yaw.
    Local,
}

/// A grabbable part of the gizmo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmeGizmoHandle {
    /// The first axis arrow.
    AxisX,
    /// The second axis arrow.
    AxisY,
    /// The vertical arrow.
    AxisZ,
    /// The square in the horizontal plane.
    PlaneXY,
    /// The square in the X-Z plane.
    PlaneXZ,
    /// The square in the Y-Z plane.
    PlaneYZ,
    /// The yaw ring.
    Ring,
    /// The uniform scale grip.
    Uniform,
}

/// Arrow length as a fraction of the gizmo size.
pub const AXIS_LEN: f32 = 1.0;
/// Where a plane square starts / ends along its two axes, as a fraction of the size.
pub const PLANE_LO: f32 = 0.25;
/// See [`PLANE_LO`].
pub const PLANE_HI: f32 = 0.55;

/// Where the gizmo stands and how big it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeGizmoFrame {
    /// The pivot, WoW yards.
    pub origin: V3,
    /// The three axes (world or yaw-rotated).
    pub axes: [V3; 3],
    /// Arrow length, yards (a constant on-screen size).
    pub size: f32,
}

impl EmeGizmoFrame {
    /// A frame at `origin`, rotated by `yaw` when `space` is local, sized for a viewer at `eye`.
    pub fn new(origin: V3, yaw: f32, space: EmeGizmoSpace, eye: V3) -> Self {
        let size = (len(sub(origin, eye)) * 0.16).clamp(0.5, 40.0);
        let axes = match space {
            EmeGizmoSpace::World => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            EmeGizmoSpace::Local => {
                let (s, c) = yaw.sin_cos();
                [[c, s, 0.0], [-s, c, 0.0], [0.0, 0.0, 1.0]]
            }
        };
        Self { origin, axes, size }
    }

    /// The tip of axis `i`.
    pub fn tip(&self, i: usize) -> V3 {
        add(self.origin, mul(self.axes[i], self.size * AXIS_LEN))
    }

    /// The four corners of a plane square (`i`, `j` the plane's axes), in loop order.
    pub fn plane_square(&self, i: usize, j: usize) -> [V3; 4] {
        let at = |u: f32, v: f32| {
            add(
                self.origin,
                add(mul(self.axes[i], u * self.size), mul(self.axes[j], v * self.size)),
            )
        };
        [
            at(PLANE_LO, PLANE_LO),
            at(PLANE_HI, PLANE_LO),
            at(PLANE_HI, PLANE_HI),
            at(PLANE_LO, PLANE_HI),
        ]
    }

    /// Points of the yaw ring (closed loop, `n` points).
    pub fn ring_points(&self, n: usize) -> Vec<V3> {
        (0..n)
            .map(|k| {
                let a = k as f32 / n as f32 * TAU;
                add(
                    self.origin,
                    add(
                        mul(self.axes[0], a.cos() * self.size),
                        mul(self.axes[1], a.sin() * self.size),
                    ),
                )
            })
            .collect()
    }
}

/// The plane squares: handle, the two in-plane axis indices, the normal axis index.
const PLANES: [(EmeGizmoHandle, usize, usize, usize); 3] = [
    (EmeGizmoHandle::PlaneXY, 0, 1, 2),
    (EmeGizmoHandle::PlaneXZ, 0, 2, 1),
    (EmeGizmoHandle::PlaneYZ, 1, 2, 0),
];

/// Which handle of the gizmo in `mode` the cursor ray `(o, d)` touches (`d` unit), if any.
pub fn hit_test(mode: EmeGizmoMode, f: &EmeGizmoFrame, o: V3, d: V3) -> Option<EmeGizmoHandle> {
    match mode {
        EmeGizmoMode::Translate => {
            // A plane square wins where it is hit (the squares sit clear of the arrows).
            let mut best_plane: Option<(f32, EmeGizmoHandle)> = None;
            for (h, i, j, k) in PLANES {
                let Some(t) = ray_plane(o, d, f.origin, f.axes[k]) else {
                    continue;
                };
                let rel = sub(add(o, mul(d, t)), f.origin);
                let u = dot(rel, f.axes[i]) / f.size;
                let v = dot(rel, f.axes[j]) / f.size;
                let inside = (PLANE_LO..=PLANE_HI).contains(&u) && (PLANE_LO..=PLANE_HI).contains(&v);
                if inside && best_plane.is_none_or(|(bt, _)| t < bt) {
                    best_plane = Some((t, h));
                }
            }
            if let Some((_, h)) = best_plane {
                return Some(h);
            }
            let tol = f.size * 0.07;
            let mut best: Option<(f32, EmeGizmoHandle)> = None;
            for (h, i) in [
                (EmeGizmoHandle::AxisX, 0),
                (EmeGizmoHandle::AxisY, 1),
                (EmeGizmoHandle::AxisZ, 2),
            ] {
                let (dist, _) = ray_segment_distance(o, d, f.origin, f.tip(i));
                if dist <= tol && best.is_none_or(|(bd, _)| dist < bd) {
                    best = Some((dist, h));
                }
            }
            best.map(|(_, h)| h)
        }
        EmeGizmoMode::Rotate => {
            let pts = f.ring_points(48);
            let tol = f.size * 0.09;
            let near = (0..pts.len()).any(|k| {
                let (dist, _) = ray_segment_distance(o, d, pts[k], pts[(k + 1) % pts.len()]);
                dist <= tol
            });
            near.then_some(EmeGizmoHandle::Ring)
        }
        EmeGizmoMode::Scale => {
            let grip = point_ray_distance(o, d, f.origin) <= f.size * 0.16
                || (0..3).any(|i| point_ray_distance(o, d, f.tip(i)) <= f.size * 0.12);
            grip.then_some(EmeGizmoHandle::Uniform)
        }
    }
}

/// What a drag changes by (relative to where it started).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EmeGizmoDelta {
    /// Move by this vector, yards.
    Move(V3),
    /// Turn by this angle about the vertical, radians.
    Rotate(f32),
    /// Multiply the scale by this factor.
    Scale(f32),
}

impl EmeGizmoDelta {
    /// Whether the delta changes nothing visible.
    pub fn is_noop(&self) -> bool {
        match self {
            Self::Move(v) => len(*v) < 1.0e-4,
            Self::Rotate(a) => a.abs() < 1.0e-4,
            Self::Scale(f) => (f - 1.0).abs() < 1.0e-4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DragStart {
    /// Distance along the axis at the grab.
    Axis { axis: V3, s0: f32 },
    /// The grabbed point on the plane.
    Plane { n: V3, u: V3, v: V3, p0: V3 },
    /// The ring angle at the grab.
    Angle { a0: f32 },
    /// Radius from the pivot on the view-facing plane at the grab.
    Radius { n: V3, r0: f32 },
}

/// A grabbed gizmo handle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeGizmoDrag {
    /// The gizmo mode.
    pub mode: EmeGizmoMode,
    /// The handle.
    pub handle: EmeGizmoHandle,
    /// The frame at the grab (it does not follow the object while dragging).
    pub frame: EmeGizmoFrame,
    start: DragStart,
}

/// The parameter along the line `p0 + s * axis` closest to the ray; `None` when the ray runs
/// parallel to the line.
fn axis_param(p0: V3, axis: V3, o: V3, d: V3) -> Option<f32> {
    let w = sub(p0, o);
    let b = dot(axis, d);
    let denom = 1.0 - b * b;
    if denom < 1.0e-4 {
        return None;
    }
    Some((b * dot(d, w) - dot(axis, w)) / denom)
}

fn ring_angle(f: &EmeGizmoFrame, o: V3, d: V3) -> Option<f32> {
    let t = ray_plane(o, d, f.origin, f.axes[2])?;
    let rel = sub(add(o, mul(d, t)), f.origin);
    Some(dot(rel, f.axes[1]).atan2(dot(rel, f.axes[0])))
}

impl EmeGizmoDrag {
    /// Grab `handle` with the ray `(o, d)`; `None` when the grab point cannot be solved (a ray
    /// parallel to the arrow or the plane).
    pub fn begin(
        mode: EmeGizmoMode,
        handle: EmeGizmoHandle,
        frame: EmeGizmoFrame,
        o: V3,
        d: V3,
    ) -> Option<Self> {
        let start = match handle {
            EmeGizmoHandle::AxisX | EmeGizmoHandle::AxisY | EmeGizmoHandle::AxisZ => {
                let i = match handle {
                    EmeGizmoHandle::AxisX => 0,
                    EmeGizmoHandle::AxisY => 1,
                    _ => 2,
                };
                let axis = frame.axes[i];
                DragStart::Axis {
                    axis,
                    s0: axis_param(frame.origin, axis, o, d)?,
                }
            }
            EmeGizmoHandle::PlaneXY | EmeGizmoHandle::PlaneXZ | EmeGizmoHandle::PlaneYZ => {
                let (_, i, j, k) = *PLANES.iter().find(|p| p.0 == handle)?;
                let t = ray_plane(o, d, frame.origin, frame.axes[k])?;
                DragStart::Plane {
                    n: frame.axes[k],
                    u: frame.axes[i],
                    v: frame.axes[j],
                    p0: add(o, mul(d, t)),
                }
            }
            EmeGizmoHandle::Ring => DragStart::Angle {
                a0: ring_angle(&frame, o, d)?,
            },
            EmeGizmoHandle::Uniform => {
                let n = mul(d, -1.0);
                let t = ray_plane(o, d, frame.origin, n)?;
                let r = len(sub(add(o, mul(d, t)), frame.origin));
                DragStart::Radius {
                    n,
                    r0: r.max(frame.size * 0.2),
                }
            }
        };
        Some(Self {
            mode,
            handle,
            frame,
            start,
        })
    }

    /// The change the current ray `(o, d)` asks for; `None` when the ray cannot be solved this
    /// frame (keep the last delta).
    pub fn solve(&self, o: V3, d: V3, snap: &EmeSnap) -> Option<EmeGizmoDelta> {
        match self.start {
            DragStart::Axis { axis, s0 } => {
                let s = axis_param(self.frame.origin, axis, o, d)?;
                Some(EmeGizmoDelta::Move(mul(axis, snap.length(s - s0))))
            }
            DragStart::Plane { n, u, v, p0 } => {
                let t = ray_plane(o, d, self.frame.origin, n)?;
                let delta = sub(add(o, mul(d, t)), p0);
                let du = snap.length(dot(delta, u));
                let dv = snap.length(dot(delta, v));
                Some(EmeGizmoDelta::Move(add(mul(u, du), mul(v, dv))))
            }
            DragStart::Angle { a0 } => {
                let a = ring_angle(&self.frame, o, d)?;
                Some(EmeGizmoDelta::Rotate(snap.angle(wrap_pi(a - a0))))
            }
            DragStart::Radius { n, r0 } => {
                let t = ray_plane(o, d, self.frame.origin, n)?;
                let r = len(sub(add(o, mul(d, t)), self.frame.origin));
                Some(EmeGizmoDelta::Scale((r / r0).clamp(0.05, 20.0)))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Applying a delta
// ---------------------------------------------------------------------------------------------

/// An object's transform as the server stores it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeObjXform {
    /// Position, WoW yards.
    pub pos: V3,
    /// Orientation about the vertical, radians in `[0, 2π)`.
    pub o: f32,
    /// Uniform scale.
    pub scale: f32,
}

/// Apply `delta` to `xs` about `pivot`: a move shifts every position, a rotation turns every
/// orientation and orbits the positions about the pivot, a scale multiplies every scale (clamped
/// to `scale_range`; rounded when snapping).
pub fn apply_delta(
    delta: EmeGizmoDelta,
    pivot: V3,
    xs: &[EmeObjXform],
    scale_range: [f32; 2],
    snap: &EmeSnap,
) -> Vec<EmeObjXform> {
    xs.iter()
        .map(|x| match delta {
            EmeGizmoDelta::Move(v) => EmeObjXform {
                pos: add(x.pos, v),
                ..*x
            },
            EmeGizmoDelta::Rotate(a) => EmeObjXform {
                pos: add(pivot, rot_z(sub(x.pos, pivot), a)),
                o: wrap_tau(x.o + a),
                scale: x.scale,
            },
            EmeGizmoDelta::Scale(f) => {
                let s = snap.scale(x.scale * f).clamp(scale_range[0], scale_range[1]);
                EmeObjXform { scale: s, ..*x }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0e-3
    }
    fn close3(a: V3, b: V3) -> bool {
        close(a[0], b[0]) && close(a[1], b[1]) && close(a[2], b[2])
    }

    #[test]
    fn wrap_helpers_land_in_their_ranges() {
        assert!(close(wrap_tau(-0.5), TAU - 0.5));
        assert!(close(wrap_tau(TAU + 0.25), 0.25));
        assert!(wrap_tau(-1.0e-9) < TAU);
        assert!(close(wrap_pi(3.5), 3.5 - TAU));
        assert!(close(wrap_pi(-3.5), TAU - 3.5));
    }

    #[test]
    fn ray_hits_a_rotated_box_where_an_aabb_would_miss() {
        // A long thin box rotated 45 degrees about Z: corners of its AABB are empty.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let obb = EmeObb {
            center: [0.0, 0.0, 0.0],
            axes: [[s, s, 0.0], [-s, s, 0.0], [0.0, 0.0, 1.0]],
            half: [3.0, 0.2, 1.0],
        };
        // Straight down the +X axis from the west: hits at x = -2.12.. (the box edge).
        let t = obb.ray([-10.0, 0.0, 0.0], [1.0, 0.0, 0.0]).expect("hit");
        assert!(t > 9.0 && t < 10.5, "t={t}");
        // The empty AABB corner (2, -2): along the long axis' perpendicular, a miss.
        assert!(obb.ray([10.0, -2.5, 0.0], [-1.0, 0.0, 0.0]).is_none());
        // A ray along the long axis hits the box end.
        let t = obb.ray([-10.0, -10.0, 0.0], [s, s, 0.0]).expect("hit");
        assert!(close(t, 10.0 * 2.0f32.sqrt() - 3.0), "t={t}");
    }

    #[test]
    fn ray_from_inside_hits_at_zero_and_behind_misses() {
        let b = EmeObb::axis_aligned([0.0; 3], [1.0; 3]);
        assert_eq!(b.ray([0.0; 3], [1.0, 0.0, 0.0]), Some(0.0));
        assert!(b.ray([5.0, 0.0, 0.0], [1.0, 0.0, 0.0]).is_none());
        // Parallel to a slab and outside it.
        assert!(b.ray([-5.0, 3.0, 0.0], [1.0, 0.0, 0.0]).is_none());
        assert!(b.contains([0.5, -0.5, 0.9]) && !b.contains([1.1, 0.0, 0.0]));
    }

    #[test]
    fn pick_takes_the_nearest_and_prefers_the_smaller_on_a_tie() {
        let mk = |guid, x: f32, half: f32| EmeWorldObject {
            guid,
            entry: 1,
            name: None,
            pos: [x, 0.0, 0.0],
            obb: EmeObb::axis_aligned([x, 0.0, 0.0], [half; 3]),
        };
        let objs = [mk(1, 20.0, 1.0), mk(2, 10.0, 1.0), mk(3, 30.0, 1.0)];
        assert_eq!(pick_object(&objs, [0.0; 3], [1.0, 0.0, 0.0], 100.0).map(|p| p.0), Some(2));
        assert!(pick_object(&objs, [0.0; 3], [1.0, 0.0, 0.0], 5.0).is_none());
        // Same entry plane: the small chair on the big rug wins.
        let rug = EmeWorldObject {
            guid: 10,
            entry: 1,
            name: None,
            pos: [10.0, 0.0, 0.0],
            obb: EmeObb::axis_aligned([10.0, 0.0, 0.0], [5.0, 5.0, 0.5]),
        };
        let chair = mk(11, 10.0, 0.5);
        // Ray from above-ish so both start at about the same distance.
        let o = [10.0, 0.0, 6.0];
        let d = [0.0, 0.0, -1.0];
        let hit = pick_object(&[rug.clone(), chair.clone()], o, d, 50.0).unwrap();
        assert_eq!(hit.0, 11);
        // A flat model (zero half height) is still pickable thanks to the padding.
        let flat = EmeWorldObject {
            obb: EmeObb::axis_aligned([0.0, 0.0, 0.0], [1.0, 1.0, 0.0]),
            ..rug
        };
        assert!(pick_object(&[flat], [0.0, 0.0, 5.0], [0.0, 0.0, -1.0], 50.0).is_some());
    }

    #[test]
    fn snapping_rounds_lengths_angles_and_scales() {
        let s = EmeSnap {
            enabled: true,
            grid: 0.5,
            angle_deg: 15.0,
        };
        assert!(close(s.length(1.26), 1.5) && close(s.length(-0.2), 0.0));
        assert!(close(s.angle(16.0f32.to_radians()), 15.0f32.to_radians()));
        assert!(close(s.scale(1.03), 1.05));
        let off = EmeSnap::default();
        assert_eq!(off.length(1.26), 1.26);
        assert_eq!(off.scale(1.03), 1.03);
    }

    fn frame(space: EmeGizmoSpace, yaw: f32) -> EmeGizmoFrame {
        EmeGizmoFrame::new([0.0, 0.0, 0.0], yaw, space, [-14.0, -12.0, 9.0])
    }

    #[test]
    fn gizmo_size_follows_distance_and_clamps() {
        let near = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, [-1.0, 0.0, 0.0]);
        let far = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, [-1000.0, 0.0, 0.0]);
        assert!(close(near.size, 0.5) && close(far.size, 40.0));
        let mid = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, [-50.0, 0.0, 0.0]);
        assert!(close(mid.size, 8.0));
    }

    #[test]
    fn local_frame_follows_the_yaw() {
        let f = frame(EmeGizmoSpace::Local, std::f32::consts::FRAC_PI_2);
        assert!(close3(f.axes[0], [0.0, 1.0, 0.0]));
        assert!(close3(f.axes[1], [-1.0, 0.0, 0.0]));
        assert!(close3(f.axes[2], [0.0, 0.0, 1.0]));
    }

    /// A ray from the camera through `target`.
    fn ray_to(eye: V3, target: V3) -> (V3, V3) {
        (eye, normalize(sub(target, eye)))
    }

    #[test]
    fn translate_handles_are_found_by_arrow_and_square() {
        let eye = [-14.0, -12.0, 9.0];
        let f = frame(EmeGizmoSpace::World, 0.0);
        // Aim at the middle of the X arrow.
        let (o, d) = ray_to(eye, mul(f.axes[0], f.size * 0.7));
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), Some(EmeGizmoHandle::AxisX));
        let (o, d) = ray_to(eye, mul(f.axes[2], f.size * 0.7));
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), Some(EmeGizmoHandle::AxisZ));
        let (o, d) = ray_to(eye, mul(f.axes[1], f.size * 0.7));
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), Some(EmeGizmoHandle::AxisY));
        // The horizontal square.
        let p = add(mul(f.axes[0], f.size * 0.4), mul(f.axes[1], f.size * 0.4));
        let (o, d) = ray_to(eye, p);
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), Some(EmeGizmoHandle::PlaneXY));
        // Empty space.
        let (o, d) = ray_to(eye, [0.0, 0.0, 30.0]);
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), None);
    }

    #[test]
    fn axis_drag_solves_the_distance_along_the_arrow_and_snaps_it() {
        let eye = [-14.0, -12.0, 9.0];
        let f = frame(EmeGizmoSpace::World, 0.0);
        let grab = mul(f.axes[1], f.size * 0.6); // the Y (west) arrow
        let (o, d) = ray_to(eye, grab);
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Translate, EmeGizmoHandle::AxisY, f, o, d).unwrap();
        // Move the cursor to the point 3.27 yd further along Y.
        let (o2, d2) = ray_to(eye, add(grab, [0.0, 3.27, 0.0]));
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Move(v) => assert!(close3(v, [0.0, 3.27, 0.0]), "{v:?}"),
            other => panic!("{other:?}"),
        }
        let snap = EmeSnap {
            enabled: true,
            grid: 0.5,
            angle_deg: 5.0,
        };
        match drag.solve(o2, d2, &snap).unwrap() {
            EmeGizmoDelta::Move(v) => assert!(close3(v, [0.0, 3.5, 0.0]), "{v:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn local_axis_drag_moves_along_the_object_heading() {
        let yaw = std::f32::consts::FRAC_PI_4;
        let f = EmeGizmoFrame::new([5.0, 5.0, 0.0], yaw, EmeGizmoSpace::Local, [-20.0, 5.0, 10.0]);
        let grab = add(f.origin, mul(f.axes[0], f.size * 0.6));
        let eye = [-20.0, 5.0, 10.0];
        let (o, d) = ray_to(eye, grab);
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Translate, EmeGizmoHandle::AxisX, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, add(grab, mul(f.axes[0], 2.0)));
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Move(v) => {
                assert!(close3(v, mul(f.axes[0], 2.0)), "{v:?}");
                assert!(close(v[0], v[1]), "heading is diagonal");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plane_drag_moves_in_the_plane_only() {
        let eye = [-1.0, 0.0, 40.0]; // looking almost straight down
        let f = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, eye);
        let grab = add(mul(f.axes[0], f.size * 0.4), mul(f.axes[1], f.size * 0.4));
        let (o, d) = ray_to(eye, grab);
        assert_eq!(hit_test(EmeGizmoMode::Translate, &f, o, d), Some(EmeGizmoHandle::PlaneXY));
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Translate, EmeGizmoHandle::PlaneXY, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, add(grab, [2.0, -3.0, 0.0]));
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Move(v) => assert!(close3(v, [2.0, -3.0, 0.0]), "{v:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_ray_parallel_to_the_arrow_cannot_start_a_drag() {
        let f = frame(EmeGizmoSpace::World, 0.0);
        // Looking straight along X from -X: the X arrow is end-on.
        assert!(EmeGizmoDrag::begin(
            EmeGizmoMode::Translate,
            EmeGizmoHandle::AxisX,
            f,
            [-20.0, 0.0, 0.0],
            [1.0, 0.0, 0.0]
        )
        .is_none());
    }

    #[test]
    fn rotate_ring_is_hit_on_the_ring_and_turns_by_the_swept_angle() {
        let eye = [0.0, -0.001, 40.0];
        let f = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, eye);
        let on_ring = |a: f32| [f.size * a.cos(), f.size * a.sin(), 0.0];
        let (o, d) = ray_to(eye, on_ring(0.3));
        assert_eq!(hit_test(EmeGizmoMode::Rotate, &f, o, d), Some(EmeGizmoHandle::Ring));
        let (o, d) = ray_to(eye, mul(on_ring(0.3), 0.3)); // well inside the ring
        assert_eq!(hit_test(EmeGizmoMode::Rotate, &f, o, d), None);
        let (o, d) = ray_to(eye, on_ring(0.3));
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Rotate, EmeGizmoHandle::Ring, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, on_ring(0.3 + 0.9));
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Rotate(a) => assert!(close(a, 0.9), "{a}"),
            other => panic!("{other:?}"),
        }
        // Sweeping across the +/-PI seam is the short way round.
        let (o, d) = ray_to(eye, on_ring(3.0));
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Rotate, EmeGizmoHandle::Ring, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, on_ring(3.0 + 0.5));
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Rotate(a) => assert!(close(a, 0.5), "{a}"),
            other => panic!("{other:?}"),
        }
        // 15-degree snapping.
        let snap = EmeSnap {
            enabled: true,
            grid: 0.5,
            angle_deg: 15.0,
        };
        let (o, d) = ray_to(eye, on_ring(0.0));
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Rotate, EmeGizmoHandle::Ring, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, on_ring(40.0f32.to_radians()));
        match drag.solve(o2, d2, &snap).unwrap() {
            EmeGizmoDelta::Rotate(a) => assert!(close(a, 45.0f32.to_radians()), "{a}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn uniform_scale_follows_the_distance_from_the_pivot() {
        let eye = [0.0, 0.0, 40.0];
        let f = EmeGizmoFrame::new([0.0; 3], 0.0, EmeGizmoSpace::World, eye);
        let grab = [f.size, 0.0, 0.0];
        let (o, d) = ray_to(eye, grab);
        assert_eq!(hit_test(EmeGizmoMode::Scale, &f, o, d), Some(EmeGizmoHandle::Uniform));
        let drag = EmeGizmoDrag::begin(EmeGizmoMode::Scale, EmeGizmoHandle::Uniform, f, o, d).unwrap();
        let (o2, d2) = ray_to(eye, [f.size * 2.0, 0.0, 0.0]);
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Scale(s) => assert!((s - 2.0).abs() < 0.15, "{s}"),
            other => panic!("{other:?}"),
        }
        let (o2, d2) = ray_to(eye, [0.0, f.size * 0.5, 0.0]);
        match drag.solve(o2, d2, &EmeSnap::default()).unwrap() {
            EmeGizmoDelta::Scale(s) => assert!((s - 0.5).abs() < 0.1, "{s}"),
            other => panic!("{other:?}"),
        }
    }

    fn xf(pos: V3, o: f32, scale: f32) -> EmeObjXform {
        EmeObjXform { pos, o, scale }
    }

    #[test]
    fn apply_delta_moves_rotates_about_the_pivot_and_clamps_scale() {
        let xs = [xf([1.0, 0.0, 0.0], 0.0, 1.0), xf([-1.0, 0.0, 0.0], 1.0, 2.0)];
        let snap = EmeSnap::default();
        let m = apply_delta(EmeGizmoDelta::Move([0.0, 2.0, 1.0]), [0.0; 3], &xs, [0.1, 20.0], &snap);
        assert!(close3(m[0].pos, [1.0, 2.0, 1.0]) && close(m[0].o, 0.0));
        let r = apply_delta(EmeGizmoDelta::Rotate(PI / 2.0), [0.0; 3], &xs, [0.1, 20.0], &snap);
        assert!(close3(r[0].pos, [0.0, 1.0, 0.0]) && close(r[0].o, PI / 2.0));
        assert!(close3(r[1].pos, [0.0, -1.0, 0.0]) && close(r[1].o, 1.0 + PI / 2.0));
        // A single object rotates in place.
        let one = apply_delta(EmeGizmoDelta::Rotate(1.0), xs[0].pos, &xs[..1], [0.1, 20.0], &snap);
        assert!(close3(one[0].pos, xs[0].pos));
        // Orientation wraps into [0, 2 pi).
        let w = apply_delta(EmeGizmoDelta::Rotate(-1.0), [0.0; 3], &xs[..1], [0.1, 20.0], &snap);
        assert!(close(w[0].o, TAU - 1.0));
        let s = apply_delta(EmeGizmoDelta::Scale(100.0), [0.0; 3], &xs, [0.1, 20.0], &snap);
        assert!(close(s[0].scale, 20.0) && close(s[1].scale, 20.0));
        let s = apply_delta(EmeGizmoDelta::Scale(0.001), [0.0; 3], &xs, [0.1, 20.0], &snap);
        assert!(close(s[0].scale, 0.1));
    }

    #[test]
    fn a_ghost_box_follows_the_transform_change() {
        let from = xf([10.0, 0.0, 0.0], 0.0, 1.0);
        // A box 2 yd in front of the origin (along +X).
        let obb = EmeObb::axis_aligned([12.0, 0.0, 1.0], [1.0, 0.5, 1.0]);
        let to = xf([0.0, 5.0, 0.0], PI / 2.0, 2.0);
        let g = obb.moved(&from, &to);
        // Rotated a quarter turn and doubled: the box sits 4 yd along +Y from the new origin.
        assert!(close3(g.center, [0.0, 9.0, 2.0]), "{:?}", g.center);
        assert!(close3(g.half, [2.0, 1.0, 2.0]));
        assert!(close3(g.axes[0], [0.0, 1.0, 0.0]));
    }

    #[test]
    fn noop_deltas_are_recognised() {
        assert!(EmeGizmoDelta::Move([0.0; 3]).is_noop());
        assert!(EmeGizmoDelta::Rotate(0.0).is_noop());
        assert!(EmeGizmoDelta::Scale(1.0).is_noop());
        assert!(!EmeGizmoDelta::Move([0.1, 0.0, 0.0]).is_noop());
    }
}
