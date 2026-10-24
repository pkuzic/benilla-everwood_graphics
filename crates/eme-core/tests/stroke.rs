//! Stroke coalescing with an injected clock.

use eme_core::stroke::*;
use eme_core::transport::mode;

fn sample(x: f32, mode: u8) -> EmeBrushSample {
    EmeBrushSample {
        map: 0,
        pos: [x, 0.0, 10.0],
        radius: 4.0,
        mode,
        strength: 255,
        param: 0,
    }
}

/// Drive a held brush at 60 fps for `ms`, moving `speed` yd/s; returns the emission times.
fn run(
    b: &mut EmeStrokeBuilder,
    start: u64,
    ms: u64,
    speed: f32,
    mode: u8,
) -> Vec<(u64, u16, u16)> {
    let mut out = Vec::new();
    let (stroke, first) = b.begin(start, sample(0.0, mode));
    if let Some(e) = first {
        out.push((start, e.stroke, e.nonce));
    }
    let mut t = start;
    while t < start + ms {
        t += 16;
        let x = speed * (t - start) as f32 / 1000.0;
        if let Some(e) = b.update(t, sample(x, mode)) {
            assert_eq!(e.stroke, stroke);
            out.push((t, e.stroke, e.nonce));
        }
    }
    out
}

#[test]
fn first_edit_is_immediate_and_rate_never_exceeds_8_per_second() {
    let mut b = EmeStrokeBuilder::default();
    let sent = run(&mut b, 1000, 2000, 30.0, mode::DIG);
    assert_eq!(sent[0].0, 1000, "press sends at once");
    for w in sent.windows(2) {
        assert!(w[1].0 - w[0].0 >= 125, "spacing {:?}", w);
    }
    // Any 1-second window holds at most 8.
    for (i, (t0, ..)) in sent.iter().enumerate() {
        let n = sent[i..]
            .iter()
            .take_while(|(t, ..)| *t < t0 + 1000)
            .count();
        assert!(n <= 8, "{n} edits in the second after {t0}");
    }
    // A fast brush is limited by the rate, so it runs near 8/s.
    assert!(sent.len() >= 14, "{}", sent.len());
}

#[test]
fn nonces_are_unique_and_one_stroke_id_covers_the_hold() {
    let mut b = EmeStrokeBuilder::default();
    let sent = run(&mut b, 0, 1000, 20.0, mode::BUILD);
    let mut nonces: Vec<u16> = sent.iter().map(|s| s.2).collect();
    nonces.dedup();
    assert_eq!(nonces.len(), sent.len());
    assert!(sent.iter().all(|s| s.1 == sent[0].1));
    let end = b.end(1010, None).unwrap();
    assert_eq!(end.stroke, sent[0].1);
    // The next stroke gets the next id.
    let (next, _) = b.begin(5000, sample(0.0, mode::DIG));
    assert_eq!(next, end.stroke + 1);
}

#[test]
fn a_still_idempotent_brush_sends_once() {
    let mut b = EmeStrokeBuilder::default();
    let sent = run(&mut b, 0, 2000, 0.0, mode::DIG);
    assert_eq!(sent.len(), 1, "{sent:?}");
}

#[test]
fn a_still_smooth_brush_keeps_working_at_the_rate() {
    let mut b = EmeStrokeBuilder::default();
    let sent = run(&mut b, 0, 1000, 0.0, mode::SMOOTH);
    assert!((8..=9).contains(&sent.len()), "{}", sent.len());
}

#[test]
fn a_slow_brush_sends_when_it_moved_half_a_radius() {
    let mut b = EmeStrokeBuilder::default();
    // 1 yd/s with radius 4: half a radius = 2 yd = every 2 s; well below the rate limit, but the
    // idempotent time trigger still applies once the brush moved past the stationary threshold.
    let sent = run(&mut b, 0, 1000, 1.0, mode::DIG);
    assert!(sent.len() >= 2, "{sent:?}");
}

#[test]
fn changing_brush_parameters_makes_a_sample_due() {
    let mut b = EmeStrokeBuilder::default();
    b.begin(0, sample(0.0, mode::DIG));
    assert!(b.update(200, sample(0.0, mode::DIG)).is_none(), "still");
    let mut s = sample(0.0, mode::DIG);
    s.radius = 6.0;
    let e = b.update(216, s).expect("radius change is due");
    assert_eq!(e.radius, 6.0);
}

#[test]
fn release_inside_the_rate_window_leaves_a_trailing_edit() {
    let mut b = EmeStrokeBuilder::default();
    let (stroke, first) = b.begin(0, sample(0.0, mode::DIG));
    assert!(first.is_some());
    let end = b.end(50, Some(sample(10.0, mode::DIG))).unwrap();
    assert_eq!(
        end,
        EmeStrokeEnd {
            stroke,
            edit: None,
            trailing: true
        }
    );
    assert!(!b.is_idle());
    assert!(b.tick(100).is_none(), "rate still closed");
    let e = b.tick(125).expect("trailing goes out at 125 ms");
    assert_eq!((e.stroke, e.pos[0]), (stroke, 10.0));
    assert!(b.is_idle());
}

#[test]
fn release_outside_the_window_sends_at_once() {
    let mut b = EmeStrokeBuilder::default();
    b.begin(0, sample(0.0, mode::DIG));
    let end = b.end(500, Some(sample(10.0, mode::DIG))).unwrap();
    assert!(end.edit.is_some() && !end.trailing);
}

#[test]
fn release_without_movement_adds_nothing() {
    let mut b = EmeStrokeBuilder::default();
    b.begin(0, sample(0.0, mode::DIG));
    let end = b.end(50, None).unwrap();
    assert!(end.edit.is_none() && !end.trailing);
    assert!(b.is_idle());
}

#[test]
fn a_quick_restroke_queues_behind_the_trailing_edit() {
    let mut b = EmeStrokeBuilder::default();
    let (s1, _) = b.begin(0, sample(0.0, mode::DIG));
    b.end(40, Some(sample(10.0, mode::DIG)));
    let (s2, first) = b.begin(60, sample(20.0, mode::DIG));
    assert!(first.is_none());
    let e = b.tick(125).unwrap();
    assert_eq!(e.stroke, s1, "the older stroke's release goes first");
    assert!(b.tick(200).is_none());
    let e = b.tick(250).unwrap();
    assert_eq!((e.stroke, e.pos[0]), (s2, 20.0));
}

#[test]
fn a_capped_config_splits_long_strokes() {
    let cfg = EmeStrokeConfig {
        min_interval_ms: 250,
        max_edits: Some(3),
        ..Default::default()
    };
    let mut b = EmeStrokeBuilder::new(cfg);
    let (s1, _) = b.begin(0, sample(0.0, mode::DIG));
    let mut strokes = vec![s1];
    for i in 1..=5u64 {
        let e = b
            .update(i * 250, sample(i as f32 * 5.0, mode::DIG))
            .unwrap();
        strokes.push(e.stroke);
    }
    assert_eq!(strokes, vec![s1, s1, s1, s1 + 1, s1 + 1, s1 + 1]);
    assert_eq!(b.active_stroke(), Some(s1 + 1));
}

#[test]
fn ids_wrap_and_skip_stroke_zero() {
    let mut b = EmeStrokeBuilder::default().with_start(u16::MAX, u16::MAX);
    let (s, e) = b.begin(0, sample(0.0, mode::DIG));
    assert_eq!((s, e.unwrap().nonce), (u16::MAX, u16::MAX));
    b.end(10, None);
    let (s, e) = b.begin(1000, sample(0.0, mode::DIG));
    assert_eq!((s, e.unwrap().nonce), (1, 0));
}

#[test]
fn cancel_drops_everything() {
    let mut b = EmeStrokeBuilder::default();
    b.begin(0, sample(0.0, mode::DIG));
    b.end(10, Some(sample(9.0, mode::DIG)));
    b.cancel();
    assert!(b.is_idle());
    assert!(b.tick(1000).is_none());
}

#[test]
fn tools_share_one_stroke_id_sequence() {
    let ids = EmeStrokeIds::starting_at(40);
    let mut brush = EmeStrokeBuilder::default().with_ids(ids.clone());
    let (s1, _) = brush.begin(0, sample(0.0, mode::DIG));
    // An overlay tool takes the next id from the same counter.
    let overlay_stroke = ids.next_id();
    brush.end(10, None);
    let (s2, _) = brush.begin(1000, sample(0.0, mode::DIG));
    assert_eq!((s1, overlay_stroke, s2), (40, 41, 42));
    assert_eq!(ids.peek(), 43);
    let wrap = EmeStrokeIds::starting_at(u16::MAX);
    assert_eq!((wrap.next_id(), wrap.next_id()), (u16::MAX, 1));
    assert_eq!(EmeStrokeIds::starting_at(0).next_id(), 1);
}

/// MONKEY (EME, live bug 2026-10-24): a hard dig held still while the cursor ray follows the pit
/// floor down (the centre sinks ~a radius per edit) used to cut a 64-yd shaft in 2 s. One sphere
/// per brush position: a centre that only sank is not due again.
#[test]
fn a_held_hard_dig_that_sinks_into_its_own_pit_sends_once() {
    let mut b = EmeStrokeBuilder::default();
    let (_, first) = b.begin(0, sample(0.0, mode::DIG));
    assert!(first.is_some());
    let mut sent = 0;
    let mut t = 0;
    let mut z = 10.0;
    while t < 2000 {
        t += 16;
        z -= 0.5; // the aim hits the freshly dug floor
        let mut s = sample(0.0, mode::DIG);
        s.pos[2] = z;
        if b.update(t, s).is_some() {
            sent += 1;
        }
    }
    assert_eq!(sent, 0, "no repeat in place");
}

/// Dragging a hard dig across its own pit never reaches deeper than one radius below the stroke's
/// first centre (build: higher).
#[test]
fn a_hard_stroke_is_capped_at_one_radius_from_its_start() {
    for (m, sign) in [(mode::DIG, -1.0f32), (mode::BUILD, 1.0)] {
        let mut b = EmeStrokeBuilder::default();
        b.begin(0, sample(0.0, m));
        let mut extreme = 10.0f32;
        let mut t = 0;
        for i in 1..40 {
            t += 130;
            let mut s = sample(i as f32 * 2.5, m);
            s.pos[2] = 10.0 + sign * 3.0 * i as f32; // the aim chases the ground it just changed
            if let Some(e) = b.update(t, s) {
                extreme = if sign < 0.0 { extreme.min(e.pos[2]) } else { extreme.max(e.pos[2]) };
            }
        }
        assert!((extreme - (10.0 + sign * 4.0)).abs() < 1e-4, "mode {m}: {extreme}");
    }
    // Soft brushes and smooth are not capped (they move the ground by small steps).
    let mut b = EmeStrokeBuilder::default();
    b.begin(0, sample(0.0, mode::SMOOTH));
    let mut s = sample(10.0, mode::SMOOTH);
    s.pos[2] = -50.0;
    assert_eq!(b.update(200, s).unwrap().pos[2], -50.0);
}
