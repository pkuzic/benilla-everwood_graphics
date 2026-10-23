//! Brush stroke coalescing (CONTRACT §3, DESIGN-EME §2.2).
//!
//! A held brush produces a sample every frame; the uplink must stay at ≤ 8 EDIT/s. The builder
//! turns the sample stream into one **stroke id** plus a sparse list of edits, each with a fresh
//! nonce. Rules, per the contract: send when the centre moved ≥ radius/2 or 125 ms passed, never
//! faster than the rate limit. Time is injected (`now_ms` on every call), so the builder is a
//! pure state machine.
//!
//! One deliberate refinement: for idempotent modes (dig, build, paint) a brush held perfectly still
//! does not resend the same sphere every 125 ms — the result would be identical and every resend
//! is a server row. Smooth / flatten / roughen keep the time trigger (holding them still keeps
//! working the ground).

use crate::transport::{mode, EmeVoxelEdit};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

/// Coalescing parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeStrokeConfig {
    /// Minimum spacing between two uplinked edits, ms (125 = 8/s).
    pub min_interval_ms: u64,
    /// Movement that makes a sample due, as a fraction of the radius (0.5).
    pub move_fraction: f32,
    /// Below this movement (fraction of radius) an idempotent brush counts as stationary.
    pub stationary_fraction: f32,
    /// Split into a new stroke after this many edits (host policy; the contract sets no cap).
    pub max_edits: Option<u32>,
}

impl Default for EmeStrokeConfig {
    fn default() -> Self {
        Self {
            min_interval_ms: 125,
            move_fraction: 0.5,
            stationary_fraction: 0.05,
            max_edits: None,
        }
    }
}

/// One frame's brush state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeBrushSample {
    /// Map id.
    pub map: u16,
    /// Brush centre, WoW yards.
    pub pos: [f32; 3],
    /// Radius, yards.
    pub radius: f32,
    /// Voxel mode ([`crate::transport::mode`]).
    pub mode: u8,
    /// Strength /255.
    pub strength: u8,
    /// Mode parameter.
    pub param: u16,
}

/// What [`EmeStrokeBuilder::end`] reports.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeStrokeEnd {
    /// The stroke that ended (the last one, if the stroke was split).
    pub stroke: u16,
    /// An edit to send now (the release position), if the rate allowed it.
    pub edit: Option<EmeVoxelEdit>,
    /// A final edit is still waiting for the rate limit; [`EmeStrokeBuilder::tick`] emits it.
    pub trailing: bool,
}

#[derive(Debug, Clone)]
struct Active {
    stroke: u16,
    edits: u32,
    last_sent: Option<EmeBrushSample>,
    pending: Option<EmeBrushSample>,
}

/// The character's stroke-id counter, shared by every tool that opens strokes (voxel brushes and
/// overlay tools number strokes from one sequence, CONTRACT §9.4). Ids run 1..=65535 and wrap to 1
/// (0 = "no stroke"). Cloning shares the counter.
#[derive(Debug, Clone)]
pub struct EmeStrokeIds(Arc<AtomicU16>);

impl Default for EmeStrokeIds {
    fn default() -> Self {
        Self::starting_at(1)
    }
}

impl EmeStrokeIds {
    /// A counter whose next id is `next` (0 is taken as 1). After a relog, seed it past the newest
    /// own stroke the server knows (`strokes` with scope `mine`), or the server answers `R 6`.
    pub fn starting_at(next: u16) -> Self {
        Self(Arc::new(AtomicU16::new(next.max(1))))
    }

    /// Take the next id.
    pub fn next_id(&self) -> u16 {
        let prev = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
                Some(if s == u16::MAX { 1 } else { s + 1 })
            })
            .expect("closure always returns Some");
        prev.max(1)
    }

    /// The id the next call returns.
    pub fn peek(&self) -> u16 {
        self.0.load(Ordering::Relaxed).max(1)
    }
}

/// Coalesces a held brush into strokes of rate-limited edits.
#[derive(Debug, Clone)]
pub struct EmeStrokeBuilder {
    config: EmeStrokeConfig,
    ids: EmeStrokeIds,
    next_nonce: u16,
    last_sent_ms: Option<u64>,
    active: Option<Active>,
    /// Release samples of ended strokes that the rate limit has not let out yet, oldest first.
    trailing: VecDeque<(u16, EmeBrushSample)>,
}

impl Default for EmeStrokeBuilder {
    fn default() -> Self {
        Self::new(EmeStrokeConfig::default())
    }
}

impl EmeStrokeBuilder {
    /// A builder with `config` and its own stroke-id counter (starting at 1); nonces start at 0.
    pub fn new(config: EmeStrokeConfig) -> Self {
        Self {
            config,
            ids: EmeStrokeIds::default(),
            next_nonce: 0,
            last_sent_ms: None,
            active: None,
            trailing: VecDeque::new(),
        }
    }

    /// Continue numbering after a previous session's ids (stroke ids are per character on the
    /// server, so a client that knows its last stroke id resumes after it).
    pub fn with_start(mut self, next_stroke: u16, next_nonce: u16) -> Self {
        self.ids = EmeStrokeIds::starting_at(next_stroke);
        self.next_nonce = next_nonce;
        self
    }

    /// Share a stroke-id counter with other tools.
    pub fn with_ids(mut self, ids: EmeStrokeIds) -> Self {
        self.ids = ids;
        self
    }

    /// Current configuration.
    pub fn config(&self) -> &EmeStrokeConfig {
        &self.config
    }

    /// Replace the configuration (e.g. after a `caps` reply).
    pub fn set_config(&mut self, config: EmeStrokeConfig) {
        self.config = config;
    }

    /// The stroke being built, if a brush is held.
    pub fn active_stroke(&self) -> Option<u16> {
        self.active.as_ref().map(|a| a.stroke)
    }

    /// Whether anything (an active stroke or a trailing edit) still needs [`Self::tick`].
    pub fn is_idle(&self) -> bool {
        self.active.is_none() && self.trailing.is_empty()
    }

    fn alloc_stroke(&mut self) -> u16 {
        self.ids.next_id()
    }

    fn alloc_nonce(&mut self) -> u16 {
        let n = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        n
    }

    fn rate_allows(&self, now_ms: u64) -> bool {
        match self.last_sent_ms {
            None => true,
            Some(t) => now_ms.saturating_sub(t) >= self.config.min_interval_ms,
        }
    }

    fn make_edit(&mut self, now_ms: u64, stroke: u16, s: &EmeBrushSample) -> EmeVoxelEdit {
        self.last_sent_ms = Some(now_ms);
        EmeVoxelEdit {
            nonce: self.alloc_nonce(),
            stroke,
            map: s.map,
            pos: s.pos,
            radius: s.radius,
            mode: s.mode,
            strength: s.strength,
            param: s.param,
        }
    }

    /// Whether `s` is worth sending given the last sent sample of its stroke.
    fn due(&self, last: Option<&EmeBrushSample>, s: &EmeBrushSample) -> bool {
        let Some(last) = last else { return true };
        if last.map != s.map
            || last.mode != s.mode
            || last.strength != s.strength
            || last.param != s.param
            || last.radius != s.radius
        {
            return true;
        }
        let d = dist(last.pos, s.pos);
        let r = s.radius.max(f32::EPSILON);
        if d >= self.config.move_fraction * r {
            return true;
        }
        // The time trigger (≥ min interval, already required by the rate check).
        !(mode::is_idempotent_edit(s.mode, s.param) && d < self.config.stationary_fraction * r)
    }

    /// Try to send the trailing release edit of an ended stroke.
    fn flush_trailing(&mut self, now_ms: u64) -> Option<EmeVoxelEdit> {
        if !self.rate_allows(now_ms) {
            return None;
        }
        let (stroke, s) = self.trailing.pop_front()?;
        Some(self.make_edit(now_ms, stroke, &s))
    }

    /// Press: start a stroke. Returns its id and the first edit if the rate allows it now (else it
    /// is held and a later [`Self::update`] / [`Self::tick`] sends it). An already active stroke is
    /// ended first (its release edit is dropped: the press supersedes it).
    pub fn begin(&mut self, now_ms: u64, sample: EmeBrushSample) -> (u16, Option<EmeVoxelEdit>) {
        self.active = None;
        let stroke = self.alloc_stroke();
        self.active = Some(Active {
            stroke,
            edits: 0,
            last_sent: None,
            pending: Some(sample),
        });
        let edit = self.tick(now_ms);
        (stroke, edit)
    }

    /// Hold: a new frame's sample. Returns an edit to send, if one is due and allowed.
    pub fn update(&mut self, now_ms: u64, sample: EmeBrushSample) -> Option<EmeVoxelEdit> {
        if let Some(a) = self.active.as_mut() {
            a.pending = Some(sample);
        } else {
            return self.flush_trailing(now_ms);
        }
        self.tick(now_ms)
    }

    /// Time passes with no new sample: emits a held edit once the rate limit allows it.
    pub fn tick(&mut self, now_ms: u64) -> Option<EmeVoxelEdit> {
        if !self.trailing.is_empty() {
            return self.flush_trailing(now_ms);
        }
        if !self.rate_allows(now_ms) {
            return None;
        }
        let a = self.active.as_ref()?;
        let s = a.pending?;
        if !self.due(a.last_sent.as_ref(), &s) {
            return None;
        }
        // Split a stroke that reached the server's per-stroke limit.
        if let Some(max) = self.config.max_edits {
            if a.edits >= max {
                let stroke = self.alloc_stroke();
                let a = self.active.as_mut().expect("active");
                a.stroke = stroke;
                a.edits = 0;
            }
        }
        let stroke = self.active.as_ref().expect("active").stroke;
        let edit = self.make_edit(now_ms, stroke, &s);
        let a = self.active.as_mut().expect("active");
        a.edits += 1;
        a.last_sent = Some(s);
        a.pending = None;
        Some(edit)
    }

    /// Release. `sample` is the release position (or `None` to use the last held sample). The
    /// release edit is sent now if due and allowed, held as a trailing edit if due but rate
    /// limited, or dropped if it adds nothing.
    pub fn end(&mut self, now_ms: u64, sample: Option<EmeBrushSample>) -> Option<EmeStrokeEnd> {
        if let Some(s) = sample {
            self.active.as_mut()?.pending = Some(s);
        }
        let edit = self.tick(now_ms);
        let a = self.active.take()?;
        let mut trailing = false;
        if let Some(s) = a.pending {
            // Not sent: the rate limit held it (if it were not due, it adds nothing).
            if self.due(a.last_sent.as_ref(), &s) {
                self.trailing.push_back((a.stroke, s));
                trailing = true;
            }
        }
        Some(EmeStrokeEnd {
            stroke: a.stroke,
            edit,
            trailing,
        })
    }

    /// Abandon the active stroke and any trailing edit (e.g. the link dropped).
    pub fn cancel(&mut self) {
        self.active = None;
        self.trailing.clear();
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}
