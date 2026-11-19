// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page's half of compositor animations (`docs/compositor-animations.md`, S2): the CSS
//! animations and transitions and the script animations (Web Animations) of a composited box that
//! the compositor can run, as the data it samples ([`LayerAnimation`]), resolved against the box as
//! it is painted. Blink's `CompositorAnimations::StartAnimationOnCompositor`
//! (`compositor_animations.cc`), which turns a keyframe effect into a `cc::KeyframeModel`.
//!
//! The page keeps running every animation on its own timeline (events, `getComputedStyle`,
//! `currentTime`): what script observes does not change. The compositor only draws the box's
//! transform or opacity at each display frame from the same keyframes and timing, so a busy page
//! thread no longer holds it back.

use snowlark_paint::layer_animation::{
    lists_interpolate, AnimatedValues, Direction, Easing, LayerAnimation, StepPosition, TransformFn,
};
use snowlark_paint::{ColorFilters, FilterOp, Matrix, Point, Rect, Ticks};
use snowlark_style::web_animations::{FillMode as ScriptFill, PlaybackDirection};
use snowlark_style::CompositorCandidate;
use style::animation::{AnimationState, KeyframesIterationState};
use style::properties::animated_properties::AnimationValue;
use style::properties::longhands::animation_direction::computed_value::single_value::T as AnimationDirection;
use style::properties::longhands::animation_fill_mode::computed_value::single_value::T as FillMode;
use style::properties::{ComputedValues, LonghandId, PropertyDeclarationId};
use style::values::computed::{Length, TimingFunction};
use style::values::generics::easing::{StepPosition as Steps, TimingFunction as Tf, TimingKeyword};

/// Pushes into `out` the compositor animations of `element` (of the document whose style engine
/// and data come first), a composited box whose transform reference box is `reference` (its
/// coordinates) and which is painted at `at` (the list's), with the computed values `values`; the
/// page's timeline reads 0 at `origin`.
pub(crate) fn collect(
    (engine, data): (&snowlark_style::StyleEngine, &snowlark_style::StyleData),
    element: snowlark_dom::NodeId,
    values: &ComputedValues,
    reference: Rect,
    at: Point,
    origin: Ticks,
    out: &mut Vec<LayerAnimation>,
) {
    engine.compositor_candidates(element, data, values, &mut |c| {
        if let Some(a) = convert(c, element, values, reference, at, origin) {
            if a.is_valid() {
                out.push(a);
            }
        }
    });
}

fn convert(
    c: CompositorCandidate<'_>,
    element: snowlark_dom::NodeId,
    values: &ComputedValues,
    reference: Rect,
    at: Point,
    origin: Ticks,
) -> Option<LayerAnimation> {
    let mut timing = Timing::css();
    let (keyframes, id, start, duration, iterations, direction, fill) = match c {
        CompositorCandidate::Animation(a, id) => {
            let (current, iterations) = match a.iteration_state {
                KeyframesIterationState::Finite(cur, max) => (cur, max),
                KeyframesIterationState::Infinite(cur) => (cur, f64::INFINITY),
            };
            // (stylo moves `started_at` to the start of each iteration it reaches)
            let start = a.started_at - current * a.duration;
            let keyframes: Vec<(f64, &AnimationValue, &TimingFunction)> =
                a.property_keyframes(PropertyDeclarationId::Longhand(id));
            let direction = match a.direction {
                AnimationDirection::Normal => Direction::Normal,
                AnimationDirection::Reverse => Direction::Reverse,
                AnimationDirection::Alternate => Direction::Alternate,
                AnimationDirection::AlternateReverse => Direction::AlternateReverse,
            };
            let fill = match a.fill_mode {
                FillMode::None => (false, false),
                FillMode::Forwards => (false, true),
                FillMode::Backwards => (true, false),
                FillMode::Both => (true, true),
            };
            if !matches!(a.state, AnimationState::Pending | AnimationState::Running) {
                return None;
            }
            let k = keyframes
                .into_iter()
                .map(|(o, v, t)| (o, v.clone(), Some(t)))
                .collect::<Vec<_>>();
            (k, id, start, a.duration, iterations, direction, fill)
        }
        CompositorCandidate::Transition(t, id) => {
            let (from, to, timing) = t.parts();
            // (CSS Transitions 1 §3: from its start time, the start value before it; none after
            // its end, when the page drops it)
            let k = vec![(0.0, from.clone(), Some(timing)), (1.0, to.clone(), None)];
            let d = t.property_animation.duration;
            (
                k,
                id,
                t.start_time,
                d,
                1.0,
                Direction::Normal,
                (true, false),
            )
        }
        CompositorCandidate::Script(s) => {
            // (the effect's timing in s; its easings with the page's precision, 1e-6)
            let w = &s.timing.timing;
            timing = Timing {
                playback_rate: s.timing.playback_rate,
                delay: w.delay / 1000.0,
                iteration_start: w.iteration_start,
                easing: easing(w.easing.function())?,
                epsilon: Some(1e-6),
            };
            let k = s
                .frames
                .iter()
                .map(|(o, v, e)| (*o, (*v).clone(), e.map(|e| e.function())))
                .collect::<Vec<_>>();
            let direction = match w.direction {
                PlaybackDirection::Normal => Direction::Normal,
                PlaybackDirection::Reverse => Direction::Reverse,
                PlaybackDirection::Alternate => Direction::Alternate,
                PlaybackDirection::AlternateReverse => Direction::AlternateReverse,
            };
            let fill = match w.computed_fill() {
                ScriptFill::Backwards => (true, false),
                ScriptFill::Forwards => (false, true),
                ScriptFill::Both => (true, true),
                ScriptFill::None | ScriptFill::Auto => (false, false),
            };
            (
                k,
                s.property,
                s.timing.start,
                w.iteration_duration() / 1000.0,
                w.iterations,
                direction,
                fill,
            )
        }
    };
    let offsets: Vec<f32> = keyframes.iter().map(|(o, _, _)| *o as f32).collect();
    let easings: Option<Vec<Easing>> = keyframes
        .iter()
        .map(|(_, _, t)| t.map_or(Some(Easing::Linear), easing))
        .collect();
    let values = match id {
        LonghandId::Opacity => AnimatedValues::Opacity(
            keyframes
                .iter()
                .map(|(_, v, _)| match v {
                    AnimationValue::Opacity(o) => Some(*o),
                    _ => None,
                })
                .collect::<Option<Vec<f32>>>()?,
        ),
        LonghandId::Transform => {
            let lists = keyframes
                .iter()
                .map(|(_, v, _)| match v {
                    AnimationValue::Transform(t) => transform_list(t, reference),
                    _ => None,
                })
                .collect::<Option<Vec<Vec<TransformFn>>>>()?;
            if !lists.windows(2).all(|w| lists_interpolate(&w[0], &w[1])) {
                return None;
            }
            // The box's transform group: `translate(-at - origin) · X · rest · translate(origin
            // + at)` (`css_transform` and the painter's `with_effects`).
            let o = crate::adapter::transform_origin(values, reference);
            if crate::motion::offset_matrix(values, reference, o, None, None).is_some() {
                return None;
            }
            let pre = Matrix::translation(-at.x - o.x, -at.y - o.y);
            let post = crate::adapter::individual_transforms(values, reference)
                .then(&Matrix::translation(o.x + at.x, o.y + at.y));
            let six = |m: Matrix| [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy];
            AnimatedValues::Transform {
                lists,
                pre: six(pre),
                post: six(post),
            }
        }
        LonghandId::Filter => {
            // (only functions that change colors, as Blink composites no filter animation that
            // moves pixels, `HasFilterThatMovesPixels`; not on a box with a backdrop filter, which
            // takes the box's filter into its backdrop)
            if !values.get_effects().backdrop_filter.0.is_empty() {
                return None;
            }
            let lists = keyframes
                .iter()
                .map(|(_, v, _)| match v {
                    AnimationValue::Filter(list) => color_filters(list.0.iter()),
                    _ => None,
                })
                .collect::<Option<Vec<ColorFilters>>>()?;
            if !lists
                .windows(2)
                .all(|w| ColorFilters::interpolate(&w[0], &w[1]))
            {
                return None;
            }
            AnimatedValues::Filter(lists)
        }
        _ => return None,
    };
    let easings = easings?;
    Some(LayerAnimation {
        element: element.to_bits(),
        origin,
        start,
        playback_rate: timing.playback_rate,
        delay: timing.delay,
        duration,
        iterations,
        iteration_start: timing.iteration_start,
        direction,
        easing: timing.easing,
        epsilon: timing.epsilon,
        fill_backwards: fill.0,
        fill_forwards: fill.1,
        offsets,
        easings,
        values,
    })
}

/// A filter keyframe value as color functions (amounts clamped as the computed value is), if it
/// has only those (Filter Effects 1 §10; `none` is the empty list).
fn color_filters<'a, L: 'a, S: 'a, U: 'a>(
    list: impl Iterator<
        Item = &'a style::values::generics::effects::Filter<
            style::values::computed::Angle,
            f32,
            L,
            S,
            U,
        >,
    >,
) -> Option<ColorFilters> {
    use style::values::generics::effects::Filter as F;
    let ops = list
        .map(|f| {
            Some(match f {
                F::Brightness(a) => FilterOp::Brightness(a.0),
                F::Contrast(a) => FilterOp::Contrast(a.0),
                F::Grayscale(a) => FilterOp::Grayscale(a.0),
                F::HueRotate(a) => FilterOp::HueRotate(a.degrees()),
                F::Invert(a) => FilterOp::Invert(a.0),
                F::Opacity(a) => FilterOp::Opacity(a.0),
                F::Saturate(a) => FilterOp::Saturate(a.0),
                F::Sepia(a) => FilterOp::Sepia(a.0),
                _ => return None,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ColorFilters::of(&ops)?.clamped())
}

/// The timing a script animation adds to a CSS one's (Web Animations 1 §4.5.4, §6.5).
struct Timing {
    playback_rate: f64,
    delay: f64,
    iteration_start: f64,
    easing: Easing,
    epsilon: Option<f64>,
}

impl Timing {
    /// A CSS animation's or transition's: its delay is in its start time.
    fn css() -> Timing {
        Timing {
            playback_rate: 1.0,
            delay: 0.0,
            iteration_start: 0.0,
            easing: Easing::Linear,
            epsilon: None,
        }
    }
}

fn easing(t: &TimingFunction) -> Option<Easing> {
    Some(match t {
        Tf::Keyword(k) => match k {
            TimingKeyword::Linear => Easing::Linear,
            TimingKeyword::Ease => Easing::CubicBezier([0.25, 0.1, 0.25, 1.0]),
            TimingKeyword::EaseIn => Easing::CubicBezier([0.42, 0.0, 1.0, 1.0]),
            TimingKeyword::EaseOut => Easing::CubicBezier([0.0, 0.0, 0.58, 1.0]),
            TimingKeyword::EaseInOut => Easing::CubicBezier([0.42, 0.0, 0.58, 1.0]),
        },
        Tf::CubicBezier { x1, y1, x2, y2 } => Easing::CubicBezier([*x1, *y1, *x2, *y2]),
        Tf::Steps(n, pos) => Easing::Steps(
            u32::try_from(*n).ok()?,
            match pos {
                Steps::JumpStart | Steps::Start => StepPosition::JumpStart,
                Steps::JumpEnd | Steps::End => StepPosition::JumpEnd,
                Steps::JumpNone => StepPosition::JumpNone,
                Steps::JumpBoth => StepPosition::JumpBoth,
            },
        ),
        Tf::LinearFunction(f) => Easing::Piecewise(f.iter().map(|e| [e.x, e.y]).collect()),
    })
}

/// A computed transform list as 2D primitives with lengths resolved against `reference` (`None`
/// for a function the compositor does not interpolate: matrices, 3D, perspective).
fn transform_list(
    t: &style::values::computed::Transform,
    reference: Rect,
) -> Option<Vec<TransformFn>> {
    use style::values::generics::transform::GenericTransformOperation as Op;
    let x = |lp: &style::values::computed::LengthPercentage| {
        lp.resolve(Length::new(reference.width)).px()
    };
    let y = |lp: &style::values::computed::LengthPercentage| {
        lp.resolve(Length::new(reference.height)).px()
    };
    t.0.iter()
        .map(|op| {
            Some(match op {
                Op::Translate(a, b) => TransformFn::Translate(x(a), y(b)),
                Op::TranslateX(a) => TransformFn::Translate(x(a), 0.0),
                Op::TranslateY(b) => TransformFn::Translate(0.0, y(b)),
                Op::Translate3D(a, b, z) if z.px() == 0.0 => TransformFn::Translate(x(a), y(b)),
                Op::Scale(a, b) => TransformFn::Scale(*a, *b),
                Op::ScaleX(a) => TransformFn::Scale(*a, 1.0),
                Op::ScaleY(b) => TransformFn::Scale(1.0, *b),
                Op::Scale3D(a, b, z) if *z == 1.0 => TransformFn::Scale(*a, *b),
                Op::Rotate(a) | Op::RotateZ(a) => TransformFn::Rotate(a.radians()),
                Op::Skew(a, b) => TransformFn::Skew(a.radians(), b.radians()),
                Op::SkewX(a) => TransformFn::SkewX(a.radians()),
                Op::SkewY(b) => TransformFn::SkewY(b.radians()),
                _ => return None,
            })
        })
        .collect()
}
