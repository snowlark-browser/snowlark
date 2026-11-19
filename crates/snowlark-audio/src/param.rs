// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The AudioParam automation timeline (§1.6 "The AudioParam Interface"): its events, the rules
//! for inserting and cancelling them, and the intrinsic value they give at each sample-frame
//! (§1.6.3 "Computation of Value").
//!
//! The control thread keeps a copy of each timeline only to raise the spec's NotSupportedError for
//! overlapping events at the call ([`Timeline::check`]); the rendering thread's copy computes the
//! values. Both apply the same insertions and cancellations in the same order.

use std::sync::Arc;

/// `AutomationRate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// One value per sample-frame.
    A,
    /// One value per render quantum, the first frame's.
    K,
}

/// An AudioParam's fixed description: `defaultValue`, the nominal range, its automation rate, and
/// whether the rate may change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamDesc {
    pub default: f32,
    pub min: f32,
    pub max: f32,
    pub rate: Rate,
    /// The rate is fixed: setting `automationRate` throws InvalidStateError.
    pub fixed_rate: bool,
}

impl ParamDesc {
    /// A parameter whose nominal range is all finite single-precision values.
    pub const fn unbounded(default: f32, rate: Rate) -> Self {
        ParamDesc {
            default,
            min: f32::MIN,
            max: f32::MAX,
            rate,
            fixed_rate: false,
        }
    }

    pub const fn fixed(mut self) -> Self {
        self.fixed_rate = true;
        self
    }

    pub const fn range(mut self, min: f32, max: f32) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    /// The value clamped to the nominal range.
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }
}

/// An automation event's kind.
#[derive(Clone, Debug)]
pub enum Kind {
    SetValue,
    LinearRamp,
    ExponentialRamp,
    /// `setTargetAtTime`: the value approaches the event's value with time constant `tau`, from
    /// `start`, the value when the event began (known once rendering reaches it).
    Target {
        tau: f64,
        start: Option<f32>,
    },
    /// `setValueCurveAtTime`: `values` over `duration` seconds, `rate` points per second ((N - 1)
    /// over the duration given; a cancellation shortens the duration but keeps the rate).
    Curve {
        values: Arc<[f32]>,
        duration: f64,
        rate: f64,
    },
}

/// An automation event. `value` is a ramp's end value, a target's target, a curve's last value.
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: Kind,
    pub time: f64,
    pub value: f32,
}

impl Event {
    pub fn set_value(value: f32, time: f64) -> Event {
        Event {
            kind: Kind::SetValue,
            time,
            value,
        }
    }

    pub fn linear(value: f32, time: f64) -> Event {
        Event {
            kind: Kind::LinearRamp,
            time,
            value,
        }
    }

    pub fn exponential(value: f32, time: f64) -> Event {
        Event {
            kind: Kind::ExponentialRamp,
            time,
            value,
        }
    }

    pub fn target(value: f32, time: f64, tau: f64) -> Event {
        Event {
            kind: Kind::Target { tau, start: None },
            time,
            value,
        }
    }

    /// A curve of at least two values.
    pub fn curve(values: Arc<[f32]>, time: f64, duration: f64) -> Event {
        let value = *values.last().expect("a curve has values");
        let rate = (values.len() - 1) as f64 / duration;
        Event {
            kind: Kind::Curve {
                values,
                duration,
                rate,
            },
            time,
            value,
        }
    }

    fn is_ramp(&self) -> bool {
        matches!(self.kind, Kind::LinearRamp | Kind::ExponentialRamp)
    }

    fn curve_end(&self) -> Option<f64> {
        match self.kind {
            Kind::Curve { duration, .. } => Some(self.time + duration),
            _ => None,
        }
    }
}

/// An event would overlap a value curve (NotSupportedError).
#[derive(Debug, PartialEq, Eq)]
pub struct Overlap;

/// The events of one AudioParam, in time order (events at one time in insertion order).
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    events: Vec<Event>,
}

/// §1.6.3: a linear ramp's value at `t`.
pub fn linear_at(t: f64, v0: f32, t0: f64, v1: f32, t1: f64) -> f32 {
    (v0 as f64 + (v1 as f64 - v0 as f64) * (t - t0) / (t1 - t0)) as f32
}

/// §1.6.3: an exponential ramp's value at `t`; constant at `v0` when `v0` is zero or the two
/// values differ in sign.
pub fn exponential_at(t: f64, v0: f32, t0: f64, v1: f32, t1: f64) -> f32 {
    if v0 == 0.0 || v0.is_sign_negative() != v1.is_sign_negative() {
        v0
    } else {
        (v0 as f64 * (v1 as f64 / v0 as f64).powf((t - t0) / (t1 - t0))) as f32
    }
}

/// §1.6.3: a value curve's value at `t`, linearly interpolated between its points.
pub fn curve_at(t: f64, t0: f64, rate: f64, values: &[f32]) -> f32 {
    let last = values.len() - 1;
    let pos = (rate * (t - t0)).max(0.0);
    let k = (pos as usize).min(last);
    let k1 = (k + 1).min(last);
    let delta = (pos - k as f64).min(1.0) as f32;
    values[k] + (values[k1] - values[k]) * delta
}

/// §1.6.3: a target's value at `t`, from `start` at `t0`.
fn target_at(t: f64, t0: f64, tau: f64, start: f32, target: f32) -> f32 {
    if tau == 0.0 {
        target
    } else {
        (target as f64 + (start as f64 - target as f64) * (-(t - t0) / tau).exp()) as f32
    }
}

impl Timeline {
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The NotSupportedError checks of §1.6.2's automation methods: a curve may contain no other
    /// event in (T, T + D), and no event may start within [T, T + D) of a curve.
    pub fn check(&self, ev: &Event) -> Result<(), Overlap> {
        let n = self.events.len();
        if n == 0 {
            return Ok(());
        }
        let at = self.events.partition_point(|e| e.time <= ev.time);
        let ub = if at < n { at } else { n - 1 };
        for e in self.events[..=ub].iter().rev() {
            if let Some(new_end) = ev.curve_end() {
                if let Some(end) = e.curve_end() {
                    if ev.time < end && e.time < new_end {
                        return Err(Overlap);
                    }
                } else if e.time > ev.time && e.time < new_end {
                    return Err(Overlap);
                }
            } else if let Some(end) = e.curve_end() {
                if ev.time >= e.time && ev.time < end {
                    return Err(Overlap);
                }
            }
            if e.time < ev.time {
                break;
            }
        }
        Ok(())
    }

    /// Inserts `ev` after the events at or before its time. A ramp into an empty timeline
    /// starts from `current` at `now` (an implicit SetValue first); a curve is followed by the
    /// implicit SetValue of its last value at its end.
    pub fn insert(&mut self, ev: Event, now: f64, current: f32) {
        if self.events.is_empty() && ev.is_ramp() {
            self.events.push(Event::set_value(current, now));
        }
        let end = ev.curve_end().map(|end| Event::set_value(ev.value, end));
        self.put(ev);
        if let Some(e) = end {
            self.put(e);
        }
    }

    fn put(&mut self, ev: Event) {
        let at = self.events.partition_point(|e| e.time <= ev.time);
        self.events.insert(at, ev);
    }

    /// `cancelScheduledValues(t)`: removes the events at or after `t`, and a curve still running
    /// at `t`, with everything after them.
    pub fn cancel(&mut self, t: f64) {
        if let Some(i) = self
            .events
            .iter()
            .position(|e| e.time >= t || e.curve_end().is_some_and(|end| e.time <= t && end > t))
        {
            self.events.truncate(i);
        }
    }

    /// `cancelAndHoldAtTime(t)`: removes the events after `t`, ending a ramp that runs past `t`
    /// at its value at `t`, and holding a target's or a curve's value at `t`. `before` is the
    /// value before the first event.
    pub fn cancel_and_hold(&mut self, t: f64, before: f32) {
        let i = self.events.partition_point(|e| e.time <= t);
        let mut cut = i;
        if i > 0
            && matches!(
                self.events[i - 1].kind,
                Kind::Target { .. } | Kind::Curve { .. }
            )
        {
            cut = i - 1;
        } else if i >= self.events.len() {
            return;
        }
        let e = self.events[cut].clone();
        let mut add = Vec::new();
        match &e.kind {
            Kind::LinearRamp | Kind::ExponentialRamp => {
                let hold = self.value_at(t, before);
                add.push(Event {
                    kind: e.kind.clone(),
                    time: t,
                    value: hold,
                });
            }
            Kind::Target { .. } => {
                if e.time < t {
                    let hold = self.value_at(t, before);
                    cut += 1;
                    add.push(Event::set_value(hold, t));
                }
            }
            Kind::Curve {
                values,
                duration,
                rate,
            } => {
                if e.time < t {
                    if t > e.time + duration {
                        cut += 1;
                    } else {
                        let hold = curve_at(t, e.time, *rate, values);
                        add.push(Event {
                            kind: Kind::Curve {
                                values: values.clone(),
                                duration: t - e.time,
                                rate: *rate,
                            },
                            time: e.time,
                            value: hold,
                        });
                        add.push(Event::set_value(hold, t));
                    }
                }
            }
            Kind::SetValue => {}
        }
        self.events.truncate(cut);
        for a in add {
            self.put(a);
        }
    }

    /// The intrinsic value at `t` from the events alone (`before`: the value before the first
    /// event). Does not change the timeline: a target not reached yet starts from the value just
    /// before it.
    pub fn value_at(&self, t: f64, before: f32) -> f32 {
        let k = self.events.partition_point(|e| e.time <= t);
        self.eval(k, t, before)
    }

    /// The value at `t` given that `k` events are at or before `t`.
    fn eval(&self, k: usize, t: f64, before: f32) -> f32 {
        if let Some(next) = self.events.get(k).filter(|n| n.is_ramp()) {
            if k == 0 {
                return before;
            }
            let prev = &self.events[k - 1];
            let v0 = self.end_value(k - 1, before);
            return match next.kind {
                Kind::LinearRamp => linear_at(t, v0, prev.time, next.value, next.time),
                _ => exponential_at(t, v0, prev.time, next.value, next.time),
            };
        }
        if k == 0 {
            return before;
        }
        let e = &self.events[k - 1];
        match &e.kind {
            Kind::SetValue | Kind::LinearRamp | Kind::ExponentialRamp => e.value,
            Kind::Target { tau, start } => {
                let v0 = start.unwrap_or_else(|| self.eval(k - 1, e.time, before));
                target_at(t, e.time, *tau, v0, e.value)
            }
            Kind::Curve {
                values,
                duration,
                rate,
            } => {
                if t < e.time + duration {
                    curve_at(t, e.time, *rate, values)
                } else {
                    e.value
                }
            }
        }
    }

    /// The value event `i` has at its own time: where a ramp that follows it starts.
    fn end_value(&self, i: usize, before: f32) -> f32 {
        let e = &self.events[i];
        match &e.kind {
            Kind::Target { start, .. } => start.unwrap_or_else(|| self.eval(i, e.time, before)),
            Kind::Curve { values, .. } => values[0],
            _ => e.value,
        }
    }

    /// Computes the intrinsic values of one render quantum starting at `start_frame`: every
    /// frame's into `out` (`Rate::A`), or the first frame's into `out[0]` (`Rate::K`). `current`
    /// holds the value before the first event and is left at the last value computed, clamped
    /// by `desc`. Events wholly in the past are dropped.
    pub fn render(
        &mut self,
        start_frame: u64,
        sample_rate: f64,
        rate: Rate,
        desc: &ParamDesc,
        out: &mut [f32],
        current: &mut f32,
    ) {
        let frames = if rate == Rate::A { out.len() } else { 1 };
        if self.events.is_empty() {
            out[..frames].fill(*current);
            return;
        }
        let mut last = *current;
        let mut t = 0.0;
        for (i, slot) in out[..frames].iter_mut().enumerate() {
            t = (start_frame + i as u64) as f64 / sample_rate;
            let k = self.events.partition_point(|e| e.time <= t);
            if k > 0 {
                // A target starts from the value at its time.
                if let Kind::Target { start: None, .. } = self.events[k - 1].kind {
                    let v0 = self.eval(k - 1, self.events[k - 1].time, last);
                    if let Kind::Target { start, .. } = &mut self.events[k - 1].kind {
                        *start = Some(v0);
                    }
                }
                let next_is_ramp = self.events.get(k).is_some_and(Event::is_ramp);
                let e = &mut self.events[k - 1];
                if let Kind::Target {
                    tau,
                    start: Some(v0),
                } = e.kind
                {
                    if next_is_ramp {
                        // A ramp after a target starts wherever the target has reached.
                        let v = target_at(t, e.time, tau, v0, e.value);
                        *e = Event::set_value(v, t);
                    }
                }
            }
            let v = desc.clamp(self.eval(k, t, last));
            *slot = v;
            last = v;
        }
        *current = last;
        // Keep the event in effect at the last frame computed, and what follows it.
        let k = self.events.partition_point(|e| e.time <= t);
        if k >= 2 {
            self.events.drain(..k - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 100.0;
    const DESC: ParamDesc = ParamDesc::unbounded(0.0, Rate::A);

    fn run(tl: &mut Timeline, frames: usize, start: f32) -> Vec<f32> {
        let mut out = Vec::new();
        let mut cur = start;
        let mut f = 0;
        while out.len() < frames {
            let mut q = [0.0f32; crate::RENDER_QUANTUM];
            tl.render(f, SR, Rate::A, &DESC, &mut q, &mut cur);
            out.extend_from_slice(&q);
            f += crate::RENDER_QUANTUM as u64;
        }
        out.truncate(frames);
        out
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-5 * b.abs().max(1.0)
    }

    #[test]
    fn set_value_applies_from_its_frame() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(2.0, 0.105), 0.0, 1.0);
        let v = run(&mut tl, 20, 1.0);
        assert_eq!(v[10], 1.0);
        assert_eq!(v[11], 2.0);
    }

    #[test]
    fn linear_ramp_matches_the_formula() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.1), 0.0, 0.0);
        tl.insert(Event::linear(3.0, 0.5), 0.0, 0.0);
        let v = run(&mut tl, 60, 0.0);
        for (f, x) in v.iter().enumerate() {
            let t = f as f64 / SR;
            let want = if t < 0.1 {
                0.0
            } else if t < 0.5 {
                linear_at(t, 1.0, 0.1, 3.0, 0.5)
            } else {
                3.0
            };
            assert!(close(*x, want), "frame {f}: {x} vs {want}");
        }
        assert!(close(v[30], 2.0));
    }

    #[test]
    fn exponential_ramp_matches_the_formula_and_holds_across_zero() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.0), 0.0, 0.0);
        tl.insert(Event::exponential(16.0, 0.4), 0.0, 0.0);
        let v = run(&mut tl, 50, 0.0);
        assert!(close(v[10], 2.0), "{}", v[10]);
        assert!(close(v[20], 4.0));
        assert_eq!(v[45], 16.0);
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(-1.0, 0.0), 0.0, 0.0);
        tl.insert(Event::exponential(1.0, 0.4), 0.0, 0.0);
        let v = run(&mut tl, 50, 0.0);
        assert_eq!(v[20], -1.0);
        assert_eq!(v[40], 1.0);
    }

    #[test]
    fn a_ramp_into_an_empty_timeline_starts_now_from_the_current_value() {
        let mut tl = Timeline::default();
        tl.insert(Event::linear(1.0, 1.0), 0.5, 0.0);
        let v = run(&mut tl, 101, 0.0);
        assert_eq!(v[40], 0.0);
        assert!(close(v[75], 0.5));
        assert_eq!(v[100], 1.0);
    }

    #[test]
    fn target_decays_from_the_value_it_found() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.0), 0.0, 0.0);
        tl.insert(Event::target(0.0, 0.1, 0.2), 0.0, 0.0);
        let v = run(&mut tl, 60, 0.0);
        for (f, x) in v.iter().enumerate().skip(10) {
            let t = f as f64 / SR;
            let want = (-(t - 0.1) / 0.2).exp() as f32;
            assert!(close(*x, want), "frame {f}: {x} vs {want}");
        }
    }

    #[test]
    fn value_curve_interpolates_and_ends_on_its_last_value() {
        let mut tl = Timeline::default();
        tl.insert(
            Event::curve(Arc::from([0.0f32, 1.0, 0.0, 2.0]), 0.1, 0.3),
            0.0,
            5.0,
        );
        let v = run(&mut tl, 50, 5.0);
        assert_eq!(v[9], 5.0);
        assert!(close(v[10], 0.0));
        assert!(close(v[15], 0.5));
        assert!(close(v[20], 1.0));
        assert!(close(v[35], 1.0));
        assert_eq!(v[40], 2.0);
        assert_eq!(v[49], 2.0);
    }

    #[test]
    fn curves_refuse_overlapping_events() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.5), 0.0, 0.0);
        let curve = |t, d| Event::curve(Arc::from([0.0f32, 1.0]), t, d);
        assert_eq!(tl.check(&curve(0.2, 0.5)), Err(Overlap));
        assert_eq!(tl.check(&curve(0.5, 0.5)), Ok(()));
        assert_eq!(tl.check(&curve(0.0, 0.5)), Ok(()));
        tl.insert(curve(1.0, 1.0), 0.0, 0.0);
        assert_eq!(tl.check(&Event::set_value(0.0, 1.0)), Err(Overlap));
        assert_eq!(tl.check(&Event::set_value(0.0, 1.5)), Err(Overlap));
        assert_eq!(tl.check(&Event::set_value(0.0, 2.0)), Ok(()));
        assert_eq!(tl.check(&curve(1.5, 1.0)), Err(Overlap));
        assert_eq!(tl.check(&curve(0.6, 0.5)), Err(Overlap));
        assert_eq!(tl.check(&curve(2.0, 1.0)), Ok(()));
    }

    #[test]
    fn cancel_removes_later_events_and_a_running_curve() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.1), 0.0, 0.0);
        tl.insert(Event::linear(2.0, 0.3), 0.0, 0.0);
        tl.cancel(0.2);
        assert_eq!(tl.events().len(), 1);
        tl.insert(Event::curve(Arc::from([0.0f32, 1.0]), 0.5, 1.0), 0.0, 0.0);
        tl.cancel(1.0);
        assert_eq!(tl.events().len(), 1);
    }

    #[test]
    fn cancel_and_hold_ends_a_ramp_at_its_value() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(0.0, 0.0), 0.0, 0.0);
        tl.insert(Event::linear(1.0, 1.0), 0.0, 0.0);
        tl.cancel_and_hold(0.25, 0.0);
        let v = run(&mut tl, 60, 0.0);
        assert!(close(v[10], 0.1));
        assert!(close(v[25], 0.25));
        assert!(close(v[50], 0.25));
        // A curve is cut at the hold time, keeping its shape.
        let mut tl = Timeline::default();
        tl.insert(
            Event::curve(Arc::from([0.0f32, 1.0, 2.0]), 0.0, 0.2),
            0.0,
            0.0,
        );
        tl.cancel_and_hold(0.15, 0.0);
        let v = run(&mut tl, 30, 0.0);
        assert!(close(v[10], 1.0));
        assert!(close(v[15], 1.5));
        assert!(close(v[25], 1.5));
        // A target holds the value it has at the hold time.
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.0), 0.0, 0.0);
        tl.insert(Event::target(0.0, 0.0, 0.1), 0.0, 0.0);
        tl.cancel_and_hold(0.1, 0.0);
        let v = run(&mut tl, 30, 0.0);
        let held = (-1.0f64).exp() as f32;
        assert!(close(v[10], held), "{}", v[10]);
        assert!(close(v[29], held));
    }

    #[test]
    fn a_ramp_after_a_target_starts_where_the_target_is() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(1.0, 0.0), 0.0, 0.0);
        tl.insert(Event::target(0.0, 0.0, 0.1), 0.0, 0.0);
        let mut cur = 0.0;
        let mut q = [0.0f32; crate::RENDER_QUANTUM];
        // Render 128 frames of the target alone, then add a ramp.
        tl.render(0, SR, Rate::A, &DESC, &mut q, &mut cur);
        tl.insert(Event::linear(1.0, 2.28), 1.28, cur);
        let at = (-(1.28f64) / 0.1).exp() as f32;
        tl.render(128, SR, Rate::A, &DESC, &mut q, &mut cur);
        assert!(close(q[0], at), "{} vs {at}", q[0]);
        assert!(close(q[50], at + (1.0 - at) * 0.5));
    }

    #[test]
    fn k_rate_takes_the_first_frame_and_values_are_clamped() {
        let mut tl = Timeline::default();
        tl.insert(Event::set_value(0.0, 0.0), 0.0, 0.0);
        tl.insert(Event::linear(10.0, 1.0), 0.0, 0.0);
        let desc = ParamDesc::unbounded(0.0, Rate::A).range(0.0, 2.0);
        let mut out = [0.0f32; crate::RENDER_QUANTUM];
        let mut cur = 0.0;
        tl.render(10, SR, Rate::K, &desc, &mut out, &mut cur);
        assert!(close(out[0], 1.0));
        tl.render(50, SR, Rate::A, &desc, &mut out, &mut cur);
        assert_eq!(out[127], 2.0);
        assert_eq!(cur, 2.0);
    }
}
