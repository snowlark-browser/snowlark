// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Script animations in the cascade (Web Animations 1 §5.3.4 the effect value of a keyframe effect,
//! §5.4 combining effects): the keyframe effects in effect for each target element, in composite
//! order, sampled at the iteration progress the page computed, composited over the CSS animations'
//! values (or the base value) and inserted at the Animations level of the cascade.
//!
//! The page owns the timing model; it hands the style engine one [`EffectSample`] per keyframe effect
//! in effect with a target ([`StyleEngine::set_script_effects`]). Property values are computed against
//! the target's style (§5.3.2) when its effect is first sampled and again when the target's style
//! without animations changes: the restyle traversal resolves them
//! ([`resolve_for_element`]) and recascades the element with the values.
//!
//! [`StyleEngine::set_script_effects`]: crate::StyleEngine::set_script_effects

use std::sync::{Arc as StdArc, RwLock};

use rustc_hash::FxHashMap;
use servo_arc::Arc;
use smallvec::SmallVec;
use snowlark_dom::NodeId;
use style::context::{CascadeInputs, StyleContext};
use style::dom::{TDocument, TElement, TNode};
use style::properties::animated_properties::{AnimationValue, AnimationValueMap};
use style::properties::{
    ComputedValues, Importance, LonghandId, OwnedPropertyDeclarationId, PropertyDeclarationBlock,
    PropertyDeclarationId,
};
use style::rule_tree::{CascadeLevel, CascadeOrigin, RuleCascadeFlags, StrongRuleNode};
use style::shared_lock::{Locked, SharedRwLock};
use style::style_resolver::{PseudoElementResolution, StyleResolverForElement};
use style::stylesheets::layer_rule::LayerOrder;
use style::stylist::RuleInclusion;
use style::values::animated::{Animate, Procedure};

use super::easing::Easing;
use super::keyframes::{CompositeOperation, Keyframe};
use super::timing::Timing;

/// What a compositor needs to run an effect on its own (`docs/compositor-animations.md`, S2): its
/// animation's timing (Blink's `CompositorAnimations::CompositorTiming`). The page gives it only
/// for an animation a compositor could run (Blink's
/// `Animation::CheckCanStartAnimationOnCompositorInternal` and
/// `CompositorAnimations::ConvertTimingForCompositor`): playing, its start time resolved, on its
/// target's document timeline, at a playback rate that is not 0, with an iteration duration
/// finite and above 0, iterations above 0, a finite delay, no end delay, and a finite active
/// duration when it plays backwards.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositorTiming {
    /// The animation's start time in the timeline time of the target's renderer (s).
    pub start: f64,
    pub playback_rate: f64,
    /// The effect's timing (ms).
    pub timing: Timing,
}

/// One property of a script animation's effect that a compositor may run
/// ([`crate::StyleEngine::compositor_candidates`]).
pub struct ScriptCandidate<'a> {
    pub property: LonghandId,
    pub timing: &'a CompositorTiming,
    /// The property-specific keyframes (§5.3.4), in order: the offset, the value (a neutral
    /// keyframe's is the value without animations: nothing else animates the property), and the
    /// easing to the next keyframe (`None`: linear).
    pub frames: Vec<(f64, &'a AnimationValue, Option<&'a Easing>)>,
}

/// One keyframe effect in effect, as the page sampled it.
#[derive(Clone, Debug)]
pub struct EffectSample {
    pub target: NodeId,
    /// A stable id of the effect (its keyframes' computed values are kept under it).
    pub id: u64,
    pub keyframes: StdArc<Vec<Keyframe>>,
    pub composite: CompositeOperation,
    /// The iteration progress (§4.7.8); `None` for an effect that is current but not in effect
    /// (§4.6.7: in its delay without a backwards fill): it gives no value yet, but what it
    /// animates acts as `will-change` (§5.7, [`crate::StyleEngine::animation_flags`]) and a
    /// compositor may run it.
    pub progress: Option<f64>,
    /// The current iteration index (for iteration composite operations; unused in this level).
    pub iteration: f64,
    /// How many times `CompositeOperation::Accumulate` adds the value.
    pub repeat: u64,
    /// The animation's timing, when a compositor could run it.
    pub compositor: Option<CompositorTiming>,
}

impl EffectSample {
    fn same_as(&self, other: &EffectSample) -> bool {
        self.target == other.target
            && self.id == other.id
            && StdArc::ptr_eq(&self.keyframes, &other.keyframes)
            && self.composite == other.composite
            && self.progress.map(f64::to_bits) == other.progress.map(f64::to_bits)
            && self.iteration.to_bits() == other.iteration.to_bits()
            && self.repeat == other.repeat
            // (a compositor gets the timing with a frame: a change needs one)
            && self.compositor == other.compositor
    }
}

/// One property of a resolved effect: its keyframes that set it (the §5.3.4 "property-specific
/// keyframes", neutral keyframes added at 0 and 1 where missing).
#[derive(Debug)]
struct PropertyTrack {
    id: OwnedPropertyDeclarationId,
    /// The value without animations (the underlying value when nothing composites below).
    base: AnimationValue,
    frames: SmallVec<[TrackFrame; 3]>,
}

#[derive(Debug)]
struct TrackFrame {
    offset: f64,
    /// The keyframe whose easing applies to the interval after this one (`None`: a neutral keyframe,
    /// linear).
    keyframe: Option<usize>,
    /// The keyframe-specific composite operation (`None`: the effect's).
    composite: Option<CompositeOperation>,
    /// `None`: the neutral value (the underlying value).
    value: Option<AnimationValue>,
}

/// An effect's keyframes computed against its target's style.
#[derive(Debug)]
struct Resolved {
    keyframes: StdArc<Vec<Keyframe>>,
    /// What the computed values depend on: the target's rules without animations, and its parent's
    /// style.
    base_rules: StrongRuleNode,
    parent: usize,
    tracks: Vec<PropertyTrack>,
}

/// The script animations of one document, shared with the restyle traversal.
#[derive(Default)]
pub struct ScriptAnimations {
    /// The samples, sorted by target, each target's in composite order.
    samples: Vec<EffectSample>,
    resolved: FxHashMap<u64, Resolved>,
}

impl ScriptAnimations {
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn range(&self, target: NodeId) -> &[EffectSample] {
        let key = target.to_bits();
        let start = self.samples.partition_point(|s| s.target.to_bits() < key);
        let end = start
            + self.samples[start..]
                .iter()
                .take_while(|s| s.target == target)
                .count();
        &self.samples[start..end]
    }

    /// Whether an effect in effect on `target` declares `property` in a keyframe.
    pub fn animates(&self, target: NodeId, property: PropertyDeclarationId) -> bool {
        self.range(target).iter().any(|s| {
            s.progress.is_some()
                && s.keyframes.iter().any(|k| {
                    k.declarations()
                        .declarations()
                        .iter()
                        .any(|d| d.id() == property)
                })
        })
    }

    /// The targets of effects current or in effect that declare a property `wanted` accepts, into
    /// `out` (each once, by id).
    pub fn targets_animating(
        &self,
        wanted: &dyn Fn(PropertyDeclarationId) -> bool,
        out: &mut Vec<NodeId>,
    ) {
        for s in &self.samples {
            if out.last() == Some(&s.target) {
                continue;
            }
            let animates = s.keyframes.iter().any(|k| {
                k.declarations()
                    .declarations()
                    .iter()
                    .any(|d| wanted(d.id()))
            });
            if animates {
                out.push(s.target);
            }
        }
    }

    /// Whether `target` has an effect current or in effect.
    pub fn has_target(&self, target: NodeId) -> bool {
        !self.range(target).is_empty()
    }

    /// How many effects current or in effect on `target` declare `property` in a keyframe (Blink's
    /// `HasIncompatibleAnimations` counts an animation in its delay too).
    pub fn count_animating(&self, target: NodeId, property: PropertyDeclarationId) -> usize {
        self.range(target)
            .iter()
            .filter(|s| {
                s.keyframes.iter().any(|k| {
                    k.declarations()
                        .declarations()
                        .iter()
                        .any(|d| d.id() == property)
                })
            })
            .count()
    }

    /// The properties of `target`'s effects that a compositor may run, by the rules of Blink's
    /// `CompositorAnimations::CheckCanStartEffectOnCompositor` on the effect itself: its animation
    /// has a [`CompositorTiming`], its keyframes are computed, every property it animates is
    /// `transform` or `opacity` (else `kUnsupportedCSSProperty`), and every keyframe that is not
    /// neutral replaces (else `kEffectHasNonReplaceCompositeMode`). The caller checks the target
    /// and the other effects on it.
    /// `alone(id)` tells whether the effect that animates `id` is the only one there: every
    /// property of an effect must pass (Blink: an effect with an incompatible property runs on the
    /// main thread as a whole).
    pub(crate) fn compositor_candidates(
        &self,
        target: NodeId,
        alone: &dyn Fn(LonghandId) -> bool,
        f: &mut dyn FnMut(ScriptCandidate<'_>),
    ) {
        for sample in self.range(target) {
            let Some(timing) = &sample.compositor else {
                continue;
            };
            let Some(r) = self.resolved.get(&sample.id) else {
                continue;
            };
            if !StdArc::ptr_eq(&r.keyframes, &sample.keyframes)
                || r.tracks
                    .iter()
                    .any(|t| !compositor_longhand(t).is_some_and(alone))
            {
                continue;
            }
            let replaces = r.tracks.iter().all(|t| {
                t.frames.iter().all(|f| {
                    f.value.is_none()
                        || f.composite.unwrap_or(sample.composite) == CompositeOperation::Replace
                })
            });
            if !replaces {
                continue;
            }
            for t in &r.tracks {
                let Some(property) = compositor_longhand(t) else {
                    continue;
                };
                f(ScriptCandidate {
                    property,
                    timing,
                    frames: t
                        .frames
                        .iter()
                        .map(|f| {
                            (
                                f.offset,
                                f.value.as_ref().unwrap_or(&t.base),
                                f.keyframe.map(|k| &r.keyframes[k].easing),
                            )
                        })
                        .collect(),
                });
            }
        }
    }

    /// Whether a compositor runs the effect `id` (Blink's
    /// `KeyframeEffect::HasActiveAnimationsOnCompositor`): it has a [`CompositorTiming`], and every
    /// property it animates is one a compositor animates on its target (`on`) and that it alone
    /// animates there (`alone`).
    pub(crate) fn effect_on_compositor(
        &self,
        id: u64,
        on: &dyn Fn(NodeId, LonghandId) -> bool,
        alone: &dyn Fn(NodeId, LonghandId) -> bool,
    ) -> bool {
        let Some(sample) = self.samples.iter().find(|s| s.id == id) else {
            return false;
        };
        let Some(r) = self.resolved.get(&id) else {
            return false;
        };
        if sample.compositor.is_none() || r.tracks.is_empty() {
            return false;
        }
        r.tracks.iter().all(|t| {
            compositor_longhand(t).is_some_and(|p| on(sample.target, p) && alone(sample.target, p))
        })
    }

    /// Replaces the samples (`new` in composite order). Returns the targets whose samples changed:
    /// they need a restyle. Computed keyframes of effects that are gone are dropped.
    pub(crate) fn replace(
        &mut self,
        mut new: Vec<EffectSample>,
    ) -> (Vec<EffectSample>, Vec<NodeId>) {
        // (a stable sort keeps each target's composite order)
        new.sort_by_key(|s| s.target.to_bits());
        let mut changed: Vec<NodeId> = Vec::new();
        {
            let (old, new) = (&self.samples, &new);
            let mut i = 0;
            let mut j = 0;
            while i < old.len() || j < new.len() {
                let t = match (old.get(i), new.get(j)) {
                    (Some(a), Some(b)) => {
                        if a.target.to_bits() <= b.target.to_bits() {
                            a.target
                        } else {
                            b.target
                        }
                    }
                    (Some(a), None) => a.target,
                    (None, Some(b)) => b.target,
                    (None, None) => break,
                };
                let a_end = i + old[i..].iter().take_while(|s| s.target == t).count();
                let b_end = j + new[j..].iter().take_while(|s| s.target == t).count();
                let same = a_end - i == b_end - j
                    && old[i..a_end]
                        .iter()
                        .zip(&new[j..b_end])
                        .all(|(a, b)| a.same_as(b));
                if !same {
                    changed.push(t);
                }
                i = a_end;
                j = b_end;
            }
        }
        let old = std::mem::replace(&mut self.samples, new);
        let samples = &self.samples;
        self.resolved.retain(|id, r| {
            samples
                .iter()
                .any(|s| s.id == *id && StdArc::ptr_eq(&s.keyframes, &r.keyframes))
        });
        (old, changed)
    }

    /// Composites the effects targeting `target` onto `map` (the CSS animations' values) in composite
    /// order (§5.4.3). Effects whose keyframes are not computed yet contribute nothing.
    pub(crate) fn composite_into(&self, target: NodeId, map: &mut AnimationValueMap) {
        for sample in self.range(target) {
            let Some(r) = self.resolved.get(&sample.id) else {
                continue;
            };
            if !StdArc::ptr_eq(&r.keyframes, &sample.keyframes) {
                continue;
            }
            for track in &r.tracks {
                let underlying = map
                    .get(&track.id)
                    .cloned()
                    .unwrap_or_else(|| track.base.clone());
                if let Some(v) = track.value(sample, &r.keyframes, &underlying) {
                    map.insert(track.id.clone(), v);
                }
            }
        }
    }

    /// The effects of `target` whose keyframes need computing against a style with rules `base_rules`
    /// and parent style `parent`.
    fn stale(
        &self,
        target: NodeId,
        base_rules: &StrongRuleNode,
        parent: usize,
    ) -> SmallVec<[(u64, StdArc<Vec<Keyframe>>); 2]> {
        self.range(target)
            .iter()
            .filter(|s| {
                !self.resolved.get(&s.id).is_some_and(|r| {
                    StdArc::ptr_eq(&r.keyframes, &s.keyframes)
                        && r.base_rules == *base_rules
                        && r.parent == parent
                })
            })
            .map(|s| (s.id, s.keyframes.clone()))
            .collect()
    }
}

impl ScriptAnimations {
    /// Calls `f` with each target and property its effects current or in effect declare.
    pub(crate) fn each_animated(&self, f: &mut dyn FnMut(NodeId, PropertyDeclarationId)) {
        for s in &self.samples {
            for k in s.keyframes.iter() {
                for d in k.declarations().declarations() {
                    f(s.target, d.id());
                }
            }
        }
    }
}

/// A track's property when a compositor animates it (`transform`, `opacity`, `filter`).
fn compositor_longhand(t: &PropertyTrack) -> Option<LonghandId> {
    match t.id.as_borrowed() {
        PropertyDeclarationId::Longhand(
            id @ (LonghandId::Transform | LonghandId::Opacity | LonghandId::Filter),
        ) => Some(id),
        _ => None,
    }
}

/// Adds the composite operation's result of `value` onto `underlying` (§5.4.4).
fn composite(
    underlying: &AnimationValue,
    value: &AnimationValue,
    op: CompositeOperation,
    repeat: u64,
) -> AnimationValue {
    let procedure = match op {
        CompositeOperation::Replace => return value.clone(),
        CompositeOperation::Add => Procedure::Add,
        CompositeOperation::Accumulate => Procedure::Accumulate { count: repeat },
    };
    underlying
        .animate(value, procedure)
        .unwrap_or_else(|()| value.clone())
}

impl PropertyTrack {
    /// §5.3.4 the effect value of the property at the sample's iteration progress.
    fn value(
        &self,
        sample: &EffectSample,
        keyframes: &[Keyframe],
        underlying: &AnimationValue,
    ) -> Option<AnimationValue> {
        let p = sample.progress?;
        let frames = &self.frames;
        let at = |o: f64| frames.iter().filter(|f| f.offset == o).count();
        // The interval endpoints.
        let (start, end) = if p < 0.0 && at(0.0) > 1 {
            (0, None)
        } else if p >= 1.0 && at(1.0) > 1 {
            (frames.len() - 1, None)
        } else {
            let start = frames
                .iter()
                .rposition(|f| f.offset <= p && f.offset < 1.0)
                .or_else(|| frames.iter().rposition(|f| f.offset == 0.0))?;
            (start, (start + 1 < frames.len()).then_some(start + 1))
        };
        let resolve = |f: &TrackFrame| -> AnimationValue {
            match &f.value {
                None => underlying.clone(),
                Some(v) => composite(
                    underlying,
                    v,
                    f.composite.unwrap_or(sample.composite),
                    sample.repeat,
                ),
            }
        };
        let first = &frames[start];
        let Some(end) = end else {
            return Some(resolve(first));
        };
        let last = &frames[end];
        let from = resolve(first);
        let to = resolve(last);
        let distance = (p - first.offset) / (last.offset - first.offset);
        let eased = match first.keyframe {
            Some(k) => keyframes[k].easing.apply(distance, false),
            None => distance,
        };
        Some(
            from.animate(&to, Procedure::Interpolate { progress: eased })
                .unwrap_or(if eased < 0.5 { from } else { to }),
        )
    }
}

/// The physical longhands (and custom properties) a keyframe's declarations set, against writing
/// mode `wm`, as a block of normal declarations (`display` and properties that are not animatable
/// left out, as for CSS animations).
fn physical_block(
    k: &Keyframe,
    wm: style::logical_geometry::WritingMode,
) -> PropertyDeclarationBlock {
    let mut out = PropertyDeclarationBlock::new();
    for d in k.declarations().normal_declaration_iter() {
        if let PropertyDeclarationId::Longhand(id) = d.id() {
            if id == LonghandId::Display || !id.is_animatable() {
                continue;
            }
        }
        out.push(d.to_physical(wm), Importance::Normal);
    }
    out
}

/// Computes the keyframes of the effects targeting `element` whose values depend on a style that
/// changed (or were never computed): §5.3.3 "computed keyframes", with each keyframe's declarations
/// cascaded over the element's rules without animations (§5.3.2 "compute a property value"). Returns
/// whether some effect's computed keyframes changed (its animation rule must be recascaded).
pub(crate) fn resolve_for_element<E: TElement>(
    store: &RwLock<ScriptAnimations>,
    target: NodeId,
    element: E,
    context: &mut StyleContext<E>,
    new_style: &Arc<ComputedValues>,
) -> bool {
    let Some(rules) = new_style.rules.as_ref() else {
        return false;
    };
    let base_rules = context
        .shared
        .stylist
        .rule_tree()
        .remove_animation_rules(rules);
    let parent = element
        .inheritance_parent()
        .and_then(|p| {
            p.borrow_data()
                .map(|d| &**d.styles.primary() as *const ComputedValues as usize)
        })
        .unwrap_or(0);
    let stale = {
        let Ok(store) = store.read() else {
            return false;
        };
        store.stale(target, &base_rules, parent)
    };
    if stale.is_empty() {
        return false;
    }
    let shared = context.shared;
    let doc = element.as_node().owner_doc();
    let lock: &SharedRwLock = doc.shared_lock();
    let mut resolver = StyleResolverForElement::new(
        element,
        context,
        RuleInclusion::All,
        PseudoElementResolution::IfApplicable,
    );
    let cascade = |resolver: &mut StyleResolverForElement<E>, rules: StrongRuleNode| {
        resolver
            .cascade_style_and_visited_with_default_parents(CascadeInputs {
                rules: Some(rules),
                visited_rules: None,
                flags: new_style.flags.for_cascade_inputs(),
                included_cascade_flags: RuleCascadeFlags::empty(),
            })
            .0
    };
    let base_style = cascade(&mut resolver, base_rules.clone());
    let wm = base_style.writing_mode;
    let mut results: SmallVec<[(u64, Resolved); 2]> = SmallVec::new();
    for (id, keyframes) in stale {
        // Each keyframe's computed values.
        let mut computed: Vec<Vec<AnimationValue>> = Vec::with_capacity(keyframes.len());
        for k in keyframes.iter() {
            let block = physical_block(k, wm);
            if block.declarations().is_empty() {
                computed.push(Vec::new());
                continue;
            }
            let ids: SmallVec<[OwnedPropertyDeclarationId; 4]> = block
                .declarations()
                .iter()
                .map(|d| d.id().to_owned())
                .collect();
            let locked: Arc<Locked<PropertyDeclarationBlock>> = Arc::new(lock.wrap(block));
            let mut important = false;
            let rules = shared
                .stylist
                .rule_tree()
                .update_rule_at_level(
                    CascadeLevel::new(CascadeOrigin::Animations),
                    LayerOrder::root(),
                    Some(locked.borrow_arc()),
                    &base_rules,
                    &shared.guards,
                    &mut important,
                )
                .unwrap_or_else(|| base_rules.clone());
            let style = cascade(&mut resolver, rules);
            computed.push(
                ids.iter()
                    .filter_map(|p| AnimationValue::from_computed_values(p.as_borrowed(), &style))
                    .collect(),
            );
        }
        // The property-specific keyframes of each property.
        let mut tracks: Vec<PropertyTrack> = Vec::new();
        for (i, values) in computed.into_iter().enumerate() {
            let k = &keyframes[i];
            for v in values {
                let pid = v.id().to_owned();
                let track = match tracks.iter_mut().position(|t| t.id == pid) {
                    Some(t) => &mut tracks[t],
                    None => {
                        let Some(base) =
                            AnimationValue::from_computed_values(pid.as_borrowed(), &base_style)
                        else {
                            continue;
                        };
                        tracks.push(PropertyTrack {
                            id: pid,
                            base,
                            frames: SmallVec::new(),
                        });
                        tracks.last_mut().expect("pushed")
                    }
                };
                track.frames.push(TrackFrame {
                    offset: k.computed_offset,
                    keyframe: Some(i),
                    composite: k.composite,
                    value: Some(v),
                });
            }
        }
        for t in &mut tracks {
            if !t.frames.iter().any(|f| f.offset == 0.0) {
                t.frames.insert(
                    0,
                    TrackFrame {
                        offset: 0.0,
                        keyframe: None,
                        composite: None,
                        value: None,
                    },
                );
            }
            if !t.frames.iter().any(|f| f.offset == 1.0) {
                t.frames.push(TrackFrame {
                    offset: 1.0,
                    keyframe: None,
                    composite: None,
                    value: None,
                });
            }
        }
        results.push((
            id,
            Resolved {
                keyframes,
                base_rules: base_rules.clone(),
                parent,
                tracks,
            },
        ));
    }
    let Ok(mut store) = store.write() else {
        return false;
    };
    for (id, r) in results {
        store.resolved.insert(id, r);
    }
    true
}

/// The animation declarations of `target`: the CSS animations' values (`css`) with the script
/// effects composited over them, as a block for the Animations level.
pub(crate) fn animation_block(
    store: &RwLock<ScriptAnimations>,
    target: NodeId,
    css: Option<AnimationValueMap>,
    lock: &SharedRwLock,
) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
    let mut map = css.unwrap_or_default();
    if let Ok(store) = store.read() {
        store.composite_into(target, &mut map);
    }
    if map.is_empty() {
        return None;
    }
    Some(Arc::new(lock.wrap(
        PropertyDeclarationBlock::from_animation_value_map(&map),
    )))
}
