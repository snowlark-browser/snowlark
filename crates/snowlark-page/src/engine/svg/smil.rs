// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG Animations in the DOM (https://svgwg.org/specs/animations/): each outermost `svg` element's
//! time container, a [`Timegraph`] of its animation elements on the document's animation clock
//! (the document timeline's time, held within a task, with document begin after the `load` event,
//! SVG Animations §3.6.6); the animated values: `animVal` for attributes (the sandwich of the
//! `animate`, `set` and `animateTransform` elements that target one, SMIL 3 §3.3.6), effects in the
//! cascade for presentation attributes, the supplemental transforms of `animateMotion`; the
//! instance times of event conditions and `beginElement()`; and the `beginEvent`, `repeatEvent` and
//! `endEvent` events, dispatched with the rendering update's animation events.
//!
//! The registry of animation elements is kept by insertion and removal reactions, so a document
//! with no animation elements pays nothing; attribute changes mark it dirty and it is worked out
//! again at the next read or animation frame ([`Engine::smil_sync`]).

use std::collections::HashMap;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use snowlark_bindings::rt::Cx;
use snowlark_dom::{ns, Dom, ElementData, LocalName, NodeId};
use snowlark_layout::svg::smil::{
    cubic_bezier, key_splines_valid, key_times_valid, transform_function, Animator, CalcMode,
    Sample,
};
use snowlark_layout::svg::timegraph::{ElementTiming, TimeEventKind, TimeSpec, Timegraph};
use snowlark_layout::svg::{geom, LengthDir, Matrix, PathData};
use snowlark_render::svg_geometry::SvgQuery;
use snowlark_style::web_animations::{CompositeOperation, EffectSample, Keyframe, RawKeyframe};

use super::super::{node_handle, Engine};
use super::reflect::{Kind, Spec};

/// A time container (SVG Animations §3.6.6).
#[derive(Default)]
pub(crate) struct Container {
    graph: Timegraph,
    /// The document time (ms) at which the container's time was `offset`; `None` before document
    /// begin.
    begun: Option<f64>,
    /// The container time at `begun`, seconds.
    offset: f64,
    /// Paused: the time it stopped at.
    paused: Option<f64>,
    document: Option<NodeId>,
    /// The graph changed since it last moved, so moving it to the same time is not idle.
    stale: bool,
}

/// What an animation element animates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnimKind {
    Animate,
    Set,
    Transform,
    Motion,
}

/// An animation element's parsed attributes.
struct Record {
    container: NodeId,
    target: Option<NodeId>,
    attr: String,
    kind: AnimKind,
    /// The `animate`/`set`/`animateTransform` function, `None` when in error.
    animator: Option<Animator>,
    timing: ElementTiming,
    /// The attribute's animation in the cascade, lowest layer first.
    effects: Vec<Effect>,
    /// `animateMotion`: the `path` its `mpath` child references.
    mpath: Option<NodeId>,
}

/// A layer of the cascade side of an animation (SVG Animations §3.5.1): SMIL's function is a stack
/// of these. A from-by animation is `from` under a by-animation, accumulation (SMIL Animation
/// §3.3.5) the end value on top, once per iteration.
#[derive(Clone)]
struct Effect {
    id: u64,
    keyframes: Arc<Vec<Keyframe>>,
    composite: CompositeOperation,
    /// A layer of accumulation.
    per_iteration: bool,
}

/// An event condition of a `begin` or `end` list.
struct Condition {
    name: String,
    animation: NodeId,
    in_begin: bool,
    offset: f64,
}

#[derive(Default)]
pub(crate) struct Smil {
    /// The connected animation elements.
    elements: FxHashSet<NodeId>,
    /// The records must be worked out again.
    dirty: bool,
    containers: FxHashMap<NodeId, Container>,
    records: FxHashMap<NodeId, Record>,
    /// The animation elements of each target, in document order.
    by_target: FxHashMap<NodeId, Vec<NodeId>>,
    /// The `animateMotion` elements, in document order.
    motions: Vec<NodeId>,
    /// The animated values of attributes that are not in the cascade, as the DOM has them.
    attributes: FxHashMap<(NodeId, LocalName), String>,
    /// Event conditions by base element.
    conditions: FxHashMap<NodeId, Vec<Condition>>,
    /// Documents whose `load` event has fired.
    loaded: FxHashSet<NodeId>,
    next_effect: u64,
    /// The events to dispatch at the next animation frame.
    pending: Vec<(NodeId, TimeEventKind)>,
}

impl Smil {
    /// Forgets freed nodes.
    pub(crate) fn nodes_freed(&mut self, freed: &FxHashSet<NodeId>) {
        self.loaded.retain(|n| !freed.contains(n));
        if self.elements.is_empty() && self.containers.is_empty() {
            return;
        }
        self.elements.retain(|n| !freed.contains(n));
        self.pending.retain(|e| !freed.contains(&e.0));
        self.containers.retain(|n, _| !freed.contains(n));
        self.dirty = true;
    }
}

/// The geometry properties (SVG 2 §6.6). Their animations go through the attribute, with SMIL's
/// arithmetic on lengths: a by-animation's negative step is no value of `width`, nor do effects
/// add the way §3.2.3 does across units.
const GEOMETRY_SPECS: [Spec; 9] = [
    geometry_spec("x", LengthDir::Horizontal),
    geometry_spec("y", LengthDir::Vertical),
    geometry_spec("width", LengthDir::Horizontal),
    geometry_spec("height", LengthDir::Vertical),
    geometry_spec("cx", LengthDir::Horizontal),
    geometry_spec("cy", LengthDir::Vertical),
    geometry_spec("r", LengthDir::Other),
    geometry_spec("rx", LengthDir::Horizontal),
    geometry_spec("ry", LengthDir::Vertical),
];

const fn geometry_spec(attr: &'static str, dir: LengthDir) -> Spec {
    Spec {
        attr,
        kind: Kind::Length { dir, initial: "0" },
    }
}

/// Whether animations of a presentation attribute of this property go through the attribute
/// (`display` is out of reach of effects, as of CSS animations).
fn through_attribute(property: &str) -> bool {
    property == "display" || GEOMETRY_SPECS.iter().any(|s| s.attr == property)
}

/// Whether an element is one the registry keeps.
pub(crate) fn is_animation_element(e: &ElementData) -> bool {
    e.name().ns == ns!(svg)
        && matches!(
            &*e.name().local,
            "animate" | "set" | "animateTransform" | "animateMotion"
        )
}

/// Whether an animation value is legal for the attribute (SMIL Animation §3.2.2: an animation with
/// an illegal value has no effect).
fn valid_for(spec: &Spec, v: &str) -> bool {
    use snowlark_layout::svg::{parse, AspectRatio, Length, ViewBox};
    match &spec.kind {
        Kind::Length { .. } => Length::parse(v).is_some(),
        Kind::LengthList(_) => v
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
            .all(|t| Length::parse(t).is_some()),
        Kind::Number { .. } | Kind::Integer { .. } => parse::parse_number(v).is_some(),
        Kind::NumberPart { .. } | Kind::IntegerPart { .. } => {
            parse::parse_number_optional_number(v).is_some()
        }
        Kind::NumberList => parse::parse_number_list(v).is_some(),
        Kind::PointList => parse::parse_points(v).is_some(),
        Kind::Rect => ViewBox::parse(v).is_some(),
        Kind::Aspect => AspectRatio::parse(v).is_some(),
        Kind::Enum { keywords, .. } => keywords.contains(&v.trim()),
        Kind::Boolean => matches!(v.trim(), "true" | "false"),
        _ => true,
    }
}

/// Whether values of the attribute add (SMIL Animation §3.2.3: numbers, lengths and lists of
/// them; not keywords, strings or `preserveAspectRatio`).
fn additive_kind(spec: &Spec) -> bool {
    !matches!(
        spec.kind,
        Kind::Enum { .. }
            | Kind::Boolean
            | Kind::String
            | Kind::Href
            | Kind::Aspect
            | Kind::OrientType
            | Kind::StringList { .. }
    )
}

/// The initial value of a reflected attribute as text, the underlying value of an animation of an
/// absent attribute.
fn initial_text(spec: &Spec) -> String {
    match &spec.kind {
        Kind::Length { initial, .. } => (*initial).to_owned(),
        Kind::Number { initial } => initial.to_string(),
        Kind::NumberPart { initial, .. } => initial.to_string(),
        Kind::Integer { initial } => initial.to_string(),
        Kind::IntegerPart { initial, .. } => initial.to_string(),
        Kind::Enum { keywords, initial } => keywords
            .get((*initial as usize).saturating_sub(1))
            .copied()
            .unwrap_or("")
            .to_owned(),
        Kind::OrientType | Kind::OrientAngle => "0".to_owned(),
        Kind::Aspect => "xMidYMid meet".to_owned(),
        Kind::Boolean => "false".to_owned(),
        _ => String::new(),
    }
}

/// The `href` of an element (`href`, else `xlink:href`).
fn href(dom: &Dom, n: NodeId) -> Option<&str> {
    dom.attr(n, &ns!(), &"href".into())
        .or_else(|| dom.attr(n, &ns!(xlink), &"href".into()))
}

/// The ids of a tree, gathered once per rebuild.
#[derive(Default)]
pub(super) struct Ids {
    root: Option<NodeId>,
    map: FxHashMap<String, NodeId>,
}

impl Ids {
    pub(super) fn get(&mut self, dom: &Dom, root: NodeId, id: &str) -> Option<NodeId> {
        if self.root != Some(root) {
            self.root = Some(root);
            self.map.clear();
            for d in dom.descendants(root) {
                if dom.is_element(d) {
                    if let Some(v) = dom.attr(d, &ns!(), &"id".into()) {
                        self.map.entry(v.to_owned()).or_insert(d);
                    }
                }
            }
        }
        self.map.get(id).copied()
    }
}

/// Whether a value of a property is a keyword that does not interpolate (SMIL Animation §3.2.2: an
/// animation of such values is discrete, whatever its `calcMode`).
fn is_keyword(v: &str) -> bool {
    !v.is_empty()
        && v.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
        && !snowlark_style::supports_declaration("color", v)
}

/// The keyframes of an animation of a presentation attribute and how they composite, without
/// accumulation: the animation function's values at their key times (SMIL Animation §3.2.2; the
/// interpolation is the property's, as in CSS animations).
fn effect_layers(
    a: &Animator,
    property: &str,
    css: &dyn Fn(&str) -> String,
    base: &url::Url,
) -> Option<Vec<(Vec<Keyframe>, CompositeOperation, bool)>> {
    let raw = |offset: f64, value: &str, easing: &str| RawKeyframe {
        offset: Some(offset),
        easing: easing.to_owned(),
        composite: None,
        values: vec![(property.to_owned(), css(value))],
    };
    let frames = |raws| snowlark_style::web_animations::process_sequence(raws, base).ok();
    let constant = |v: &str| frames(vec![raw(0.0, v, "linear"), raw(1.0, v, "linear")]);
    let own = if a.additive {
        CompositeOperation::Add
    } else {
        CompositeOperation::Replace
    };
    let accumulate = CompositeOperation::Accumulate;
    let mut layers = Vec::new();
    if let Some(by) = &a.by {
        layers.push((constant(&a.values[0])?, own, false));
        layers.push((
            frames(vec![raw(1.0, by, "linear")])?,
            CompositeOperation::Add,
            false,
        ));
        if a.accumulate {
            layers.push((constant(&a.values[0])?, accumulate, true));
            layers.push((constant(by)?, accumulate, true));
        }
        return Some(layers);
    }
    let n = a.values.len();
    let mut raws = Vec::with_capacity(n + 1);
    if a.to_animation || a.by_animation {
        raws.push(raw(1.0, &a.values[0], "linear"));
    } else if n == 1 {
        raws.push(raw(0.0, &a.values[0], "linear"));
        raws.push(raw(1.0, &a.values[0], "linear"));
    } else {
        let keywords = a.values.iter().all(|v| is_keyword(v));
        let mode = if keywords {
            CalcMode::Discrete
        } else {
            a.calc_mode
        };
        let offsets = Animator {
            calc_mode: mode,
            ..a.clone()
        }
        .offsets();
        for (i, v) in a.values.iter().enumerate() {
            let easing = match (mode, a.key_splines.get(i)) {
                (CalcMode::Discrete, _) => "step-end".to_owned(),
                (CalcMode::Spline, Some(s)) => {
                    format!("cubic-bezier({}, {}, {}, {})", s[0], s[1], s[2], s[3])
                }
                _ => "linear".to_owned(),
            };
            raws.push(raw(offsets[i], v, &easing));
        }
        if mode == CalcMode::Discrete {
            // (the last value holds to the end of the simple duration)
            raws.push(raw(1.0, &a.values[n - 1], "step-end"));
        }
    }
    let composite = if a.to_animation {
        CompositeOperation::Replace
    } else if a.by_animation {
        CompositeOperation::Add
    } else {
        own
    };
    layers.push((frames(raws)?, composite, false));
    if a.accumulate && !a.to_animation {
        let last = a.values.last()?;
        layers.push((constant(last)?, accumulate, true));
    }
    Some(layers)
}

/// The vertices of a motion path: their distances along it, and where they are (SVG Animations
/// §3.8.3).
fn path_vertices(path: &kurbo::BezPath) -> Vec<(f64, kurbo::Point)> {
    let mut out = Vec::new();
    let mut so_far = kurbo::BezPath::new();
    let mut start = kurbo::Point::ZERO;
    for e in path.elements() {
        so_far.push(*e);
        let at = match e {
            kurbo::PathEl::MoveTo(p) => {
                start = *p;
                *p
            }
            kurbo::PathEl::LineTo(p)
            | kurbo::PathEl::QuadTo(_, p)
            | kurbo::PathEl::CurveTo(_, _, p) => *p,
            kurbo::PathEl::ClosePath => start,
        };
        out.push((geom::total_length(&so_far), at));
    }
    out
}

impl Engine {
    // ----- the registry -----------------------------------------------------------------------------

    /// An element was inserted (`connected`: into a document) or removed.
    pub(crate) fn smil_tree_changed(&self, node: NodeId, connected: bool) {
        let animation = self.dom().element(node).is_some_and(is_animation_element);
        self.smil_tree_changed_as(node, connected, animation);
    }

    /// [`Engine::smil_tree_changed`] for a node whose caller knows whether it is an animation
    /// element.
    pub(crate) fn smil_tree_changed_as(&self, node: NodeId, connected: bool, animation: bool) {
        let mut st = self.svg.borrow_mut();
        if animation {
            if connected {
                st.smil.elements.insert(node);
            } else {
                st.smil.elements.remove(&node);
            }
        }
        if !st.smil.elements.is_empty() || !st.smil.records.is_empty() {
            st.smil.dirty = true;
        }
    }

    /// An attribute changed: an animation element's attributes and ids matter.
    pub(crate) fn smil_attribute_changed(&self, el: NodeId, local: &str) {
        let mut st = self.svg.borrow_mut();
        if !st.smil.elements.is_empty() && (local == "id" || st.smil.elements.contains(&el)) {
            st.smil.dirty = true;
        }
    }

    /// A document's `load` event was dispatched: its containers begin (SVG Animations §3.6.6), at
    /// the time a script set before.
    pub(crate) fn smil_document_loaded(&self, doc: NodeId) {
        let now = self.document_time(doc);
        let mut st = self.svg.borrow_mut();
        let s = &mut st.smil;
        s.loaded.insert(doc);
        for c in s.containers.values_mut() {
            if c.document == Some(doc) && c.begun.is_none() {
                c.begun = now;
                if c.offset > 0.0 {
                    c.graph.seek(c.offset, c.paused.is_none());
                    s.pending.append(&mut c.graph.events);
                }
                c.stale = true;
            }
        }
    }

    /// Makes sure `svg`'s container exists.
    fn smil_ensure_container(&self, svg: NodeId) {
        if self.svg.borrow().smil.containers.contains_key(&svg) {
            return;
        }
        let doc = self.dom().node_document(svg);
        let now = self.document_time(doc);
        let mut st = self.svg.borrow_mut();
        let loaded = st.smil.loaded.contains(&doc);
        st.smil.containers.insert(
            svg,
            Container {
                begun: if loaded { now } else { None },
                document: Some(doc),
                ..Container::default()
            },
        );
    }

    /// Works one animation element's record out; `syncs` gets its conditions on other elements.
    fn smil_record(
        &self,
        a: NodeId,
        container: NodeId,
        previous: Option<&Record>,
        ids: &mut Ids,
        syncs: &mut Vec<(NodeId, NodeId, TimeSpec, bool)>,
    ) -> Record {
        let attr = |name: &str| self.svg_attr(a, name);
        let timing = ElementTiming::parse(attr);
        let (kind, target, mpath) = {
            let dom = self.dom();
            let kind = match dom.element(a).map(|e| &*e.name().local) {
                Some("set") => AnimKind::Set,
                Some("animateTransform") => AnimKind::Transform,
                Some("animateMotion") => AnimKind::Motion,
                _ => AnimKind::Animate,
            };
            let root = dom.root(a);
            let target = match href(&dom, a) {
                Some(h) => h
                    .trim()
                    .strip_prefix('#')
                    .and_then(|id| ids.get(&dom, root, id)),
                None => dom.parent_element(a),
            };
            let mpath = (kind == AnimKind::Motion)
                .then(|| dom.children(a).find(|&c| super::is_svg(&dom, c, "mpath")))
                .flatten()
                .and_then(|c| {
                    let h = href(&dom, c)?.trim().strip_prefix('#')?;
                    ids.get(&dom, root, h)
                })
                .filter(|&p| p != a);
            (kind, target, mpath)
        };
        let name = attr("attributeName")
            .map(|n| n.trim().to_owned())
            .unwrap_or_default();
        let name = if name == "xlink:href" {
            "href".to_owned()
        } else {
            name
        };
        let animator = match kind {
            AnimKind::Motion => None,
            _ => Animator::parse(attr, kind == AnimKind::Set).filter(|a| !a.in_error()),
        };
        // A presentation attribute animates its property (SVG Animations §3.5.1);
        // attributeType="CSS" on anything else has no effect (§3.5.2).
        let (local, outermost) = target.map_or((String::new(), false), |t| {
            let dom = self.dom();
            (
                dom.element(t)
                    .map_or(String::new(), |e| e.name().local.to_string()),
                snowlark_render::svg_geometry::is_outermost_svg(&dom, t),
            )
        });
        let property = snowlark_style::svg_presentation_property(&local, outermost, &name)
            .filter(|p| (*p == "transform") == (kind == AnimKind::Transform));
        let css_type = attr("attributeType").as_deref().map(str::trim) == Some("CSS");
        let animator = animator.filter(|_| !css_type || property.is_some());
        let effects = match (property.filter(|p| !through_attribute(p)), &animator) {
            (Some(p), Some(an)) => {
                let reusable = previous
                    .filter(|r| r.animator.as_ref() == Some(an) && r.attr == name && r.kind == kind)
                    .map(|r| r.effects.clone());
                reusable.unwrap_or_else(|| {
                    let function = (kind == AnimKind::Transform).then(|| {
                        attr("type").map_or("translate".to_owned(), |t| t.trim().to_owned())
                    });
                    let css = |v: &str| -> String {
                        let value = match &function {
                            Some(f) => transform_function(f, v),
                            None => v.trim().to_owned(),
                        };
                        match value.as_str() {
                            "" if p == "d" => "none".to_owned(),
                            "inherit" => value,
                            _ => snowlark_style::svg_presentation_declaration(
                                &local, outermost, &name, &value,
                            )
                            .map_or(value.clone(), |d| d.1.into_owned()),
                        }
                    };
                    self.smil_effects(an, p, &css, a)
                })
            }
            _ => Vec::new(),
        };
        for (list, in_begin) in [(&timing.begin, true), (&timing.end, false)] {
            for s in list {
                if !matches!(s, TimeSpec::Offset(_) | TimeSpec::Indefinite) {
                    syncs.push((a, container, s.clone(), in_begin));
                }
            }
        }
        // (a value that is not one of the property's: the whole animation has no effect)
        let animator =
            animator.filter(|_| !effects.is_empty() || property.is_none_or(through_attribute));
        Record {
            container,
            target,
            attr: name,
            kind,
            animator,
            timing,
            effects,
            mpath,
        }
    }

    /// The cascade layers of an animation of `property`; none when one of its values is not one
    /// of the property's (SMIL Animation §3.2.2: the animation has no effect).
    fn smil_effects(
        &self,
        an: &Animator,
        property: &str,
        css: &dyn Fn(&str) -> String,
        a: NodeId,
    ) -> Vec<Effect> {
        let values = an.values.iter().chain(&an.by);
        if !values
            .into_iter()
            .all(|v| snowlark_style::supports_declaration(property, &css(v)))
        {
            return Vec::new();
        }
        let layers = effect_layers(an, property, css, &self.base_url_of(a)).unwrap_or_default();
        let mut st = self.svg.borrow_mut();
        layers
            .into_iter()
            .map(|(frames, composite, per_iteration)| {
                st.smil.next_effect += 1;
                Effect {
                    id: st.smil.next_effect,
                    keyframes: Arc::new(frames),
                    composite,
                    per_iteration,
                }
            })
            .collect()
    }

    /// Works the records and the timegraphs out again after a change.
    pub(crate) fn smil_sync(&self) {
        {
            let mut st = self.svg.borrow_mut();
            if !std::mem::take(&mut st.smil.dirty) {
                return;
            }
        }
        // The elements in document order.
        let mut elements: Vec<NodeId> = {
            let st = self.svg.borrow();
            let dom = self.dom();
            st.smil
                .elements
                .iter()
                .copied()
                .filter(|&e| dom.is_alive(e) && dom.is_connected(e))
                .collect()
        };
        // SVG 2 §5.6: what is inside an element that fails its conditions does not run.
        {
            let q = SvgQuery::unstyled(self.dom.borrow());
            let dom = self.dom();
            elements.retain(|&e| {
                dom.ancestors(e)
                    .filter(|&a| super::is_svg_element(&dom, a))
                    .all(|a| q.passes_conditions(a))
            });
        }
        {
            let dom = self.dom();
            elements.sort_by(|&a, &b| dom.tree_order(a, b));
        }
        let mut old = std::mem::take(&mut self.svg.borrow_mut().smil.records);
        let mut by_target: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
        let mut records: FxHashMap<NodeId, Record> = FxHashMap::default();
        let mut orders: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
        let mut motions = Vec::new();
        let mut ids = Ids::default();
        let mut syncs = Vec::new();
        for &a in &elements {
            let Some(container) = self.svg_outermost(a) else {
                continue;
            };
            self.smil_ensure_container(container);
            let previous = old.get(&a).filter(|p| p.container == container);
            let same = previous.is_some();
            let record = self.smil_record(a, container, previous, &mut ids, &mut syncs);
            if same {
                old.remove(&a);
            }
            if let Some(t) = record.target {
                by_target.entry(t).or_default().push(a);
            }
            if record.kind == AnimKind::Motion {
                motions.push(a);
            }
            orders.entry(container).or_default().push(a);
            records.insert(a, record);
        }
        // The timegraphs: gone elements leave, changed ones are told.
        {
            let mut st = self.svg.borrow_mut();
            for (a, r) in old {
                if let Some(c) = st.smil.containers.get_mut(&r.container) {
                    c.graph.remove_element(a);
                }
            }
            for (a, r) in &records {
                if let Some(c) = st.smil.containers.get_mut(&r.container) {
                    c.graph.set_element_if_changed(*a, &r.timing);
                }
            }
            for c in st.smil.containers.values_mut() {
                c.graph.clear_dependencies();
                c.stale = true;
            }
            for (container, order) in orders {
                if let Some(c) = st.smil.containers.get_mut(&container) {
                    c.graph.set_order(order);
                }
            }
        }
        let conditions = self.smil_link(syncs, &records, &mut ids);
        let mut st = self.svg.borrow_mut();
        let s = &mut st.smil;
        s.records = records;
        s.by_target = by_target;
        s.conditions = conditions;
        s.motions = motions;
        drop(st);
        self.animations.borrow_mut().dirty = true;
    }

    /// Resolves the time values on other elements: syncbase values to the timegraph, event values to
    /// conditions on their base elements (SVG Animations §3.6.7).
    fn smil_link(
        &self,
        syncs: Vec<(NodeId, NodeId, TimeSpec, bool)>,
        records: &FxHashMap<NodeId, Record>,
        ids: &mut Ids,
    ) -> FxHashMap<NodeId, Vec<Condition>> {
        let mut conditions: FxHashMap<NodeId, Vec<Condition>> = FxHashMap::default();
        let dom = self.dom();
        let mut st = self.svg.borrow_mut();
        for (a, container, spec, in_begin) in syncs {
            let root = dom.root(a);
            let Some(c) = st.smil.containers.get_mut(&container) else {
                continue;
            };
            match spec {
                TimeSpec::Sync { base, end, offset } => {
                    if let Some(b) = ids.get(&dom, root, &base) {
                        c.graph.add_sync(b, a, in_begin, end, offset);
                    }
                }
                TimeSpec::Repeat {
                    base,
                    iteration,
                    offset,
                } => {
                    if let Some(b) = ids.get(&dom, root, &base) {
                        c.graph.add_repeat_sync(b, a, in_begin, iteration, offset);
                    }
                }
                TimeSpec::Event { base, name, offset } => {
                    let b = match base {
                        Some(id) => ids.get(&dom, root, &id),
                        None => records.get(&a).and_then(|r| r.target),
                    };
                    if let Some(b) = b {
                        conditions.entry(b).or_default().push(Condition {
                            name,
                            animation: a,
                            in_begin,
                            offset,
                        });
                    }
                }
                TimeSpec::AccessKey { key, offset } => {
                    conditions
                        .entry(dom.node_document(a))
                        .or_default()
                        .push(Condition {
                            name: format!("accesskey:{key}"),
                            animation: a,
                            in_begin,
                            offset,
                        });
                }
                TimeSpec::Offset(_) | TimeSpec::Indefinite => {}
            }
        }
        conditions
    }

    // ----- the clock ---------------------------------------------------------------------------------

    /// A container's current time, seconds.
    fn smil_container_time(&self, svg: NodeId) -> f64 {
        let (paused, begun, offset, doc) = {
            let st = self.svg.borrow();
            let Some(c) = st.smil.containers.get(&svg) else {
                return 0.0;
            };
            (c.paused, c.begun, c.offset, c.document)
        };
        if let Some(p) = paused {
            return p;
        }
        match (begun, doc.and_then(|d| self.document_time(d))) {
            (Some(b), Some(n)) => offset + (n - b).max(0.0) / 1000.0,
            _ => offset,
        }
    }

    /// Brings a container's timegraph to its current time and queues its events. Returns whether it
    /// moved.
    fn smil_advance(&self, svg: NodeId) -> bool {
        let t = self.smil_container_time(svg);
        let mut st = self.svg.borrow_mut();
        let s = &mut st.smil;
        let Some(c) = s.containers.get_mut(&svg) else {
            return false;
        };
        // (nothing plays before document begin)
        if c.begun.is_none() || (t == c.graph.time() && !std::mem::take(&mut c.stale)) {
            return false;
        }
        c.stale = false;
        c.graph.advance(t);
        s.pending.append(&mut c.graph.events);
        true
    }

    /// The container of `el`, with the registry up to date and its time current.
    fn smil_current(&self, el: NodeId) -> Option<NodeId> {
        self.smil_sync();
        let svg = self.svg_outermost(el)?;
        self.smil_ensure_container(svg);
        self.smil_advance(svg);
        Some(svg)
    }

    /// The current time of the time container `el` is in (`getCurrentTime()`), seconds.
    pub(crate) fn svg_time_now(&self, el: NodeId) -> f64 {
        self.svg_outermost(el).map_or(0.0, |svg| {
            self.smil_ensure_container(svg);
            self.smil_container_time(svg)
        })
    }

    /// `pauseAnimations()`.
    pub(crate) fn smil_pause(&self, el: NodeId) {
        let Some(svg) = self.smil_current(el) else {
            return;
        };
        let t = self.smil_container_time(svg);
        if let Some(c) = self.svg.borrow_mut().smil.containers.get_mut(&svg) {
            if c.paused.is_none() {
                c.paused = Some(t);
            }
        }
    }

    /// `unpauseAnimations()`.
    pub(crate) fn smil_unpause(&self, el: NodeId) {
        let Some(svg) = self.smil_current(el) else {
            return;
        };
        let doc = self.dom().node_document(svg);
        let now = self.document_time(doc);
        if let Some(c) = self.svg.borrow_mut().smil.containers.get_mut(&svg) {
            if let Some(t) = c.paused.take() {
                c.offset = t;
                if c.begun.is_some() {
                    c.begun = now;
                }
            }
        }
    }

    /// `animationsPaused()`.
    pub(crate) fn smil_paused(&self, el: NodeId) -> bool {
        let Some(svg) = self.svg_outermost(el) else {
            return false;
        };
        self.svg
            .borrow()
            .smil
            .containers
            .get(&svg)
            .is_some_and(|c| c.paused.is_some())
    }

    /// The animated values of what is not in the cascade go to the DOM's side table, where
    /// rendering and geometry read them: the sandwich of each attribute without a property, and of
    /// properties animations cannot set (`display`), over its base value.
    fn smil_apply_attributes(&self, cx: &mut Cx) {
        let mut values: FxHashMap<(NodeId, LocalName), String> = FxHashMap::default();
        let mut geometry: Vec<(NodeId, &'static Spec)> = Vec::new();
        {
            let st = self.svg.borrow();
            let s = &st.smil;
            if s.records.is_empty() && s.attributes.is_empty() {
                return;
            }
            let mut active: Vec<(f64, usize, NodeId, &Record, Sample)> = Vec::new();
            for (target, list) in &s.by_target {
                for (order, a) in list.iter().enumerate() {
                    let Some(r) = s.records.get(a) else { continue };
                    if r.animator.is_none() || !r.effects.is_empty() || r.attr.is_empty() {
                        continue;
                    }
                    let sample = s
                        .containers
                        .get(&r.container)
                        .and_then(|c| c.graph.sample(*a));
                    if let Some(sample) = sample {
                        active.push((sample.begin, order, *target, r, sample));
                    }
                }
            }
            active.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            for (_, _, target, r, sample) in active {
                let Some(animator) = &r.animator else {
                    continue;
                };
                if r.kind == AnimKind::Transform {
                    continue;
                }
                if let Some(spec) = GEOMETRY_SPECS.iter().find(|s| s.attr == r.attr) {
                    if !geometry.contains(&(target, spec)) {
                        geometry.push((target, spec));
                    }
                    continue;
                }
                let key = (target, LocalName::from(r.attr.as_str()));
                let underlying = values
                    .get(&key)
                    .cloned()
                    .or_else(|| self.svg_attr(target, &r.attr))
                    .unwrap_or_default();
                let v = animator.value(&sample, &underlying);
                values.insert(key, v);
            }
        }
        for (target, spec) in geometry {
            let base = self.svg_attr(target, spec.attr);
            if let Some(v) = self.smil_value(target, spec, base.as_deref()) {
                values.insert((target, LocalName::from(spec.attr)), v);
            }
        }
        let old = std::mem::take(&mut self.svg.borrow_mut().smil.attributes);
        if old == values {
            self.svg.borrow_mut().smil.attributes = values;
            return;
        }
        {
            let mut dom = self.dom_mut();
            for key in old.keys().filter(|k| !values.contains_key(k)) {
                if dom.is_alive(key.0) {
                    dom.set_animated_attr(key.0, key.1.clone(), None);
                }
            }
            for (key, v) in &values {
                if old.get(key) != Some(v) && dom.is_alive(key.0) {
                    dom.set_animated_attr(key.0, key.1.clone(), Some(v.clone()));
                }
            }
        }
        self.svg.borrow_mut().smil.attributes = values;
        self.process_reactions(cx);
    }

    /// `setCurrentTime()`: before document begin the time is where the timeline will start (SVG
    /// Animations §3.6.8).
    pub(crate) fn smil_seek(&self, cx: &mut Cx, el: NodeId, seconds: f64) {
        self.smil_sync();
        let Some(svg) = self.svg_outermost(el) else {
            return;
        };
        self.smil_ensure_container(svg);
        let doc = self.dom().node_document(svg);
        let now = self.document_time(doc);
        {
            let mut st = self.svg.borrow_mut();
            let s = &mut st.smil;
            if let Some(c) = s.containers.get_mut(&svg) {
                let t = seconds.max(0.0);
                if c.begun.is_some() {
                    c.graph.seek(t, c.paused.is_none());
                    s.pending.append(&mut c.graph.events);
                    c.begun = now;
                }
                if c.paused.is_some() {
                    c.paused = Some(t);
                }
                c.offset = t;
            }
        }
        self.animations.borrow_mut().dirty = true;
        self.smil_apply_attributes(cx);
    }

    /// `beginElementAt(offset)`/`endElementAt(offset)`, and event conditions: an instance at the
    /// current time plus `offset`.
    pub(crate) fn smil_add_instance(&self, cx: &mut Cx, el: NodeId, begin: bool, offset: f64) {
        let Some(svg) = self.smil_current(el) else {
            return;
        };
        let t = self.smil_container_time(svg) + offset;
        if let Some(c) = self.svg.borrow_mut().smil.containers.get_mut(&svg) {
            c.graph.add_instance(el, begin, t);
            c.stale = true;
        }
        self.smil_advance(svg);
        self.animations.borrow_mut().dirty = true;
        self.smil_apply_attributes(cx);
    }

    /// `getStartTime()`: the begin of the current interval.
    pub(crate) fn smil_start_time(&self, el: NodeId) -> Option<f64> {
        let svg = self.smil_current(el)?;
        self.svg
            .borrow()
            .smil
            .containers
            .get(&svg)?
            .graph
            .current_begin(el)
    }

    /// Whether dispatches matter to the animations.
    pub(crate) fn smil_has_conditions(&self) -> bool {
        let st = self.svg.borrow();
        !st.smil.conditions.is_empty() || (st.smil.dirty && !st.smil.elements.is_empty())
    }

    /// An event was dispatched at `target`: event conditions on it, and on its ancestors when it
    /// bubbles, add instances (SMIL 3 §5.3.2 "Event-value").
    pub(crate) fn smil_event(&self, cx: &mut Cx, name: &str, target: NodeId, bubbles: bool) {
        self.smil_sync();
        let mut hits: Vec<(NodeId, bool, f64)> = Vec::new();
        {
            let st = self.svg.borrow();
            let dom = self.dom();
            let mut cur = Some(target);
            while let Some(n) = cur {
                if let Some(list) = st.smil.conditions.get(&n) {
                    hits.extend(
                        list.iter()
                            .filter(|c| c.name == name)
                            .map(|c| (c.animation, c.in_begin, c.offset)),
                    );
                }
                cur = if bubbles { dom.parent(n) } else { None };
            }
        }
        for (a, in_begin, offset) in hits {
            self.smil_add_instance(cx, a, in_begin, offset);
        }
    }

    /// When the animations next want a rendering update, in event loop time (`NEG_INFINITY`: at the
    /// next opportunity).
    pub(crate) fn smil_frame_at(&self) -> Option<f64> {
        let mut waiting = Vec::new();
        {
            let st = self.svg.borrow();
            let s = &st.smil;
            if s.containers.is_empty() {
                return None;
            }
            if !s.pending.is_empty() || s.dirty {
                return Some(f64::NEG_INFINITY);
            }
            for (svg, c) in &s.containers {
                if c.begun.is_some() && c.stale {
                    return Some(f64::NEG_INFINITY);
                }
                if c.paused.is_some() || c.begun.is_none() {
                    continue;
                }
                if c.graph.playing() {
                    return Some(f64::NEG_INFINITY);
                }
                if let Some(next) = c.graph.next_time() {
                    waiting.push((*svg, next));
                }
            }
        }
        let last_frame = self.event_loop.borrow().last_frame.max(0.0);
        waiting
            .into_iter()
            .map(|(svg, next)| {
                last_frame + (next - self.smil_container_time(svg)).max(0.0) * 1000.0
            })
            .reduce(f64::min)
    }

    /// The animation frame's step: the containers' times move on, the queued events are dispatched
    /// (SVG Animations §5.2) and the cascade gets the new effects.
    pub(crate) fn smil_update(&self, cx: &mut Cx) {
        let containers: Vec<NodeId> = {
            let st = self.svg.borrow();
            if st.smil.containers.is_empty() && st.smil.elements.is_empty() {
                return;
            }
            st.smil.containers.keys().copied().collect()
        };
        self.smil_sync();
        let mut moved = false;
        for svg in containers {
            moved |= self.smil_advance(svg);
        }
        if moved {
            self.animations.borrow_mut().dirty = true;
        }
        self.smil_apply_attributes(cx);
        let events = std::mem::take(&mut self.svg.borrow_mut().smil.pending);
        for (el, kind) in events {
            if !self.dom().is_alive(el) {
                continue;
            }
            let name = match kind {
                TimeEventKind::Begin => "beginEvent",
                TimeEventKind::End => "endEvent",
                TimeEventKind::Repeat(_) => "repeatEvent",
            };
            self.fire_event(cx, node_handle(el), name, false, false);
        }
    }

    /// The samples of the animations of presentation attributes in effect, by document, lowest
    /// priority first.
    pub(crate) fn smil_effect_samples(&self, out: &mut FxHashMap<NodeId, Vec<EffectSample>>) {
        if self.svg.borrow().smil.elements.is_empty() {
            return;
        }
        self.smil_sync();
        let st = self.svg.borrow();
        let s = &st.smil;
        let dom = self.dom();
        let mut active: Vec<(f64, usize, EffectSample, NodeId)> = Vec::new();
        for list in s.by_target.values() {
            for (order, a) in list.iter().enumerate() {
                let Some(r) = s.records.get(a) else { continue };
                let Some(target) = r.target.filter(|_| !r.effects.is_empty()) else {
                    continue;
                };
                let Some(sample) = s
                    .containers
                    .get(&r.container)
                    .and_then(|c| c.graph.sample(*a))
                else {
                    continue;
                };
                for e in &r.effects {
                    if e.per_iteration && sample.iteration == 0 {
                        continue;
                    }
                    active.push((
                        sample.begin,
                        order,
                        EffectSample {
                            target,
                            id: e.id,
                            keyframes: e.keyframes.clone(),
                            composite: e.composite,
                            progress: Some(sample.progress),
                            iteration: sample.iteration as f64,
                            repeat: sample.iteration as u64,
                            compositor: None,
                        },
                        dom.node_document(target),
                    ));
                }
            }
        }
        active.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
        for (_, _, sample, doc) in active {
            out.entry(doc).or_default().push(sample);
        }
    }

    // ----- animated values ---------------------------------------------------------------------------

    /// A reflected attribute's value: its base value, or for `animVal` its animated value.
    pub(crate) fn svg_value(&self, el: NodeId, spec: &Spec, anim: bool) -> Option<String> {
        let base = self.svg_attr(el, spec.attr);
        if !anim {
            return base;
        }
        self.smil_value(el, spec, base.as_deref()).or(base)
    }

    /// The animated value of `spec`'s attribute of `el` over `base` (`None`: no animation applies).
    pub(crate) fn smil_value(&self, el: NodeId, spec: &Spec, base: Option<&str>) -> Option<String> {
        if self.svg.borrow().smil.elements.is_empty() {
            return None;
        }
        self.smil_sync();
        let list: Vec<NodeId> = {
            let st = self.svg.borrow();
            let s = &st.smil;
            let list = s.by_target.get(&el)?;
            list.iter()
                .copied()
                .filter(|a| {
                    s.records.get(a).is_some_and(|r| {
                        r.attr == spec.attr && r.kind != AnimKind::Motion && r.animator.is_some()
                    })
                })
                .collect()
        };
        if list.is_empty() {
            return None;
        }
        // The active ones, by priority: begin time, then document order (SMIL 3 §3.3.6).
        let mut active: Vec<(f64, usize, Sample, Animator, Option<String>)> = Vec::new();
        for (order, &a) in list.iter().enumerate() {
            let container = self.svg.borrow().smil.records.get(&a)?.container;
            self.smil_advance(container);
            let st = self.svg.borrow();
            let s = &st.smil;
            let r = s.records.get(&a)?;
            let Some(sample) = s.containers.get(&container).and_then(|c| c.graph.sample(a)) else {
                continue;
            };
            let mut animator = r.animator.clone()?;
            let transform = r.kind == AnimKind::Transform;
            let valid = if transform {
                // (an animateTransform value is one to three numbers)
                animator.values.iter().all(|v| {
                    snowlark_layout::svg::parse_number_list(v)
                        .is_some_and(|n| (1..=3).contains(&n.len()))
                })
            } else {
                animator.values.iter().all(|v| valid_for(spec, v))
            };
            if !valid {
                continue;
            }
            if !additive_kind(spec) {
                // (a by-animation of a non-additive type has no effect; additive is ignored)
                if animator.by_animation || animator.by.is_some() {
                    continue;
                }
                animator.additive = false;
                animator.accumulate = false;
            }
            drop(st);
            let function = transform.then(|| {
                self.svg_attr(a, "type")
                    .map_or("translate".to_owned(), |t| t.trim().to_owned())
            });
            active.push((sample.begin, order, sample, animator, function));
        }
        if active.is_empty() {
            return None;
        }
        active.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
        let mut value = base
            .map(str::to_owned)
            .unwrap_or_else(|| initial_text(spec));
        for (_, _, sample, mut animator, function) in active {
            if let Some(kind) = function {
                // (the function's arguments interpolate; the result replaces or follows the list)
                let additive = std::mem::take(&mut animator.additive);
                let args = animator.value(&sample, "");
                let f = transform_function(&kind, &args);
                value = if additive && !value.trim().is_empty() {
                    format!("{} {f}", value.trim())
                } else {
                    f
                };
            } else {
                let original = self.smil_unified(el, spec, &mut animator, &mut value);
                let underlying = std::mem::take(&mut value);
                value = animator.value(&sample, &underlying);
                // SMIL leaves a value across units open. A length keeps the unit of the value
                // nearest in time: the first value's in the first half of the simple duration, the
                // last one's after.
                if let (Some(original), Kind::Length { dir, .. }) = (original, &spec.kind) {
                    let shown = if sample.progress < 0.5 {
                        original.first()
                    } else {
                        original.last()
                    };
                    let unit = shown.and_then(|v| snowlark_layout::svg::Length::parse(v));
                    if let (Some(unit), Ok(px)) = (unit, value.trim().parse::<f32>()) {
                        let ctx = self.svg_length_context(el);
                        value = snowlark_layout::svg::Length::converted(unit.unit, px, *dir, &ctx)
                            .to_css();
                    }
                }
            }
        }
        if matches!(spec.kind, Kind::Integer { .. } | Kind::IntegerPart { .. }) {
            // (integers animate in whole steps)
            let t = snowlark_layout::svg::smil::Template::parse(&value);
            let n = t.numbers.iter().map(|v| v.round()).collect::<Vec<_>>();
            value = snowlark_layout::svg::smil::Template {
                parts: t.parts,
                numbers: n,
            }
            .to_css();
        }
        Some(value)
    }

    /// Brings an animation's values and the underlying value to one form so that they interpolate
    /// (SMIL Animation §3.2.4): lengths in different units to user units, angles to degrees, and
    /// the one-number form of a number-optional-number to two numbers. Returns the values as
    /// written when lengths or angles changed.
    fn smil_unified(
        &self,
        el: NodeId,
        spec: &Spec,
        animator: &mut Animator,
        underlying: &mut String,
    ) -> Option<Vec<String>> {
        use snowlark_layout::svg::{parse, smil::Template, Angle, Length, LengthDir};
        let values = &mut animator.values;
        let unit_of = |v: &str| Template::parse(v).parts;
        let mixed = |values: &[String], underlying: &str| {
            let u = unit_of(values.first().map(String::as_str).unwrap_or(""));
            values.iter().any(|v| unit_of(v) != u) || unit_of(underlying) != u
        };
        match &spec.kind {
            Kind::Length { dir, .. } | Kind::LengthList(dir) => {
                if !mixed(values, underlying) {
                    return None;
                }
                let original = values.clone();
                let dir: LengthDir = *dir;
                let ctx = self.svg_length_context(el);
                let px = |v: &str| -> String {
                    v.split(|c: char| c == ',' || c.is_ascii_whitespace())
                        .filter(|t| !t.is_empty())
                        .map(|t| {
                            Length::parse(t).map_or_else(
                                || t.to_owned(),
                                |l| snowlark_layout::svg::fmt_number(l.to_user(dir, &ctx)),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                for v in values.iter_mut() {
                    *v = px(v);
                }
                *underlying = px(underlying);
                Some(original)
            }
            Kind::OrientAngle => {
                if !mixed(values, underlying) {
                    return None;
                }
                let original = values.clone();
                let deg = |v: &str| {
                    Angle::parse(v).map_or_else(
                        || v.to_owned(),
                        |a| snowlark_layout::svg::fmt_number(a.degrees()),
                    )
                };
                for v in values.iter_mut() {
                    *v = deg(v);
                }
                *underlying = deg(underlying);
                Some(original)
            }
            Kind::NumberPart { .. } | Kind::IntegerPart { .. } => {
                let two = |v: &str| match parse::parse_number_optional_number(v) {
                    Some((a, None)) => format!("{} {}", parse::fmt_f64(a), parse::fmt_f64(a)),
                    _ => v.to_owned(),
                };
                for v in values.iter_mut() {
                    *v = two(v);
                }
                *underlying = two(underlying);
                None
            }
            _ => None,
        }
    }

    // ----- animateMotion -----------------------------------------------------------------------------

    /// The supplemental transforms of the `animateMotion` elements in effect, by target (SVG
    /// Animations §3.8).
    pub(crate) fn smil_motions(&self) -> HashMap<NodeId, Matrix> {
        let mut out: HashMap<NodeId, Matrix> = HashMap::new();
        if self.svg.borrow().smil.elements.is_empty() {
            return out;
        }
        self.smil_sync();
        let motions = self.svg.borrow().smil.motions.clone();
        let mut active: Vec<(f64, usize, NodeId, NodeId, Sample, Option<NodeId>)> = Vec::new();
        for (order, a) in motions.into_iter().enumerate() {
            let Some((container, target, mpath)) = self
                .svg
                .borrow()
                .smil
                .records
                .get(&a)
                .map(|r| (r.container, r.target, r.mpath))
            else {
                continue;
            };
            self.smil_advance(container);
            let sample = self
                .svg
                .borrow()
                .smil
                .containers
                .get(&container)
                .and_then(|c| c.graph.sample(a));
            if let (Some(t), Some(sample)) = (target, sample) {
                active.push((sample.begin, order, a, t, sample, mpath));
            }
        }
        active.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
        for (_, _, a, target, sample, mpath) in active {
            if let Some(m) = self.smil_motion(a, &sample, mpath) {
                let additive =
                    self.svg_attr(a, "additive").as_deref().map(str::trim) == Some("sum");
                let e = out.entry(target).or_insert(Matrix::IDENTITY);
                *e = if additive { *e * m } else { m };
            }
        }
        out
    }

    /// The motion path of an `animateMotion`: an `mpath` child's path, else `path`, else the lines
    /// through `values` or `from`, `to` and `by` (SVG Animations §3.8.2).
    fn smil_motion_path(&self, a: NodeId, mpath: Option<NodeId>) -> Option<kurbo::BezPath> {
        let attr = |name: &str| self.svg_attr(a, name);
        if let Some(p) = mpath {
            let shape = SvgQuery::unstyled(self.dom.borrow()).shape_path(p)?;
            let m = self
                .svg_attr(p, "transform")
                .and_then(|t| snowlark_layout::svg::transform::parse_transform_list(&t))
                .map_or(Matrix::IDENTITY, |l| {
                    snowlark_layout::svg::transform::list_matrix(&l)
                });
            return Some(m * shape);
        }
        if let Some(d) = attr("path") {
            return Some(PathData::parse(&d).to_bezpath());
        }
        let point = |v: &str| -> Option<(f64, f64)> {
            let n = snowlark_layout::svg::parse_number_list(v)?;
            (n.len() == 2).then(|| (n[0], n[1]))
        };
        let points: Vec<(f64, f64)> = if let Some(v) = attr("values") {
            v.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(point)
                .collect::<Option<Vec<_>>>()?
        } else {
            match (attr("from"), attr("to"), attr("by")) {
                (Some(f), Some(t), _) => vec![point(&f)?, point(&t)?],
                (None, Some(t), _) => vec![(0.0, 0.0), point(&t)?],
                (Some(f), None, Some(b)) => {
                    let (f, b) = (point(&f)?, point(&b)?);
                    vec![f, (f.0 + b.0, f.1 + b.1)]
                }
                (None, None, Some(b)) => vec![(0.0, 0.0), point(&b)?],
                _ => return None,
            }
        };
        let mut p = kurbo::BezPath::new();
        for (i, (x, y)) in points.iter().enumerate() {
            if i == 0 {
                p.move_to((*x, *y));
            } else {
                p.line_to((*x, *y));
            }
        }
        Some(p)
    }

    /// One `animateMotion` element's transform at `sample` (`None`: no effect).
    fn smil_motion(&self, a: NodeId, sample: &Sample, mpath: Option<NodeId>) -> Option<Matrix> {
        let attr = |name: &str| self.svg_attr(a, name);
        let path = self.smil_motion_path(a, mpath)?;
        let length = geom::total_length(&path);
        let progress = sample.progress.clamp(0.0, 1.0);
        let numbers = |name: &str| -> Option<Vec<f64>> {
            attr(name)?
                .split(';')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.parse::<f64>().ok())
                .collect()
        };
        // (paced is the default of animateMotion, §3.8.1)
        let mode = match attr("calcMode").as_deref().map(str::trim) {
            Some("discrete") => CalcMode::Discrete,
            Some("linear") => CalcMode::Linear,
            Some("spline") => CalcMode::Spline,
            _ => CalcMode::Paced,
        };
        let key_times = match attr("keyTimes") {
            Some(_) => numbers("keyTimes")?,
            None => Vec::new(),
        };
        // The points time maps to, as distances along the path: `keyPoints`, else the vertices.
        let key_points = attr("keyPoints").is_some();
        let vertices = path_vertices(&path);
        let points: Vec<f64> = if key_points {
            let k = numbers("keyPoints")?;
            if k.len() != key_times.len() || k.iter().any(|p| !(0.0..=1.0).contains(p)) {
                return None;
            }
            k.iter().map(|p| p * length).collect()
        } else {
            vertices.iter().map(|v| v.0).collect()
        };
        let n = points.len();
        if n == 0 || !key_times_valid(mode, &key_times, n) {
            return None;
        }
        let splines: Vec<[f64; 4]> = attr("keySplines").map_or(Vec::new(), |v| {
            v.split(';')
                .filter_map(|s| {
                    let n = snowlark_layout::svg::parse_number_list(s.trim())?;
                    (n.len() == 4).then(|| [n[0], n[1], n[2], n[3]])
                })
                .collect()
        });
        if mode == CalcMode::Spline && !key_splines_valid(&splines, n) {
            return None;
        }
        let times = |steps: usize| -> Vec<f64> {
            if key_times.len() == n {
                key_times.clone()
            } else {
                (0..n).map(|i| i as f64 / steps.max(1) as f64).collect()
            }
        };
        let mut vertex = None;
        let distance = match mode {
            CalcMode::Paced if key_points => {
                // (the key points paced: constant speed along their sequence)
                let legs: Vec<f64> = points.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
                let total = legs.iter().fold(0.0, |s, l| s + l);
                let mut left = progress * total;
                let mut at = points[0];
                for (i, leg) in legs.iter().enumerate() {
                    if left <= *leg {
                        let t = if *leg > 0.0 { left / leg } else { 0.0 };
                        at = points[i] + (points[i + 1] - points[i]) * t;
                        break;
                    }
                    left -= leg;
                    at = points[i + 1];
                }
                at
            }
            CalcMode::Paced => progress * length,
            CalcMode::Discrete => {
                let t = times(n);
                let i = t.iter().rposition(|k| *k <= progress).unwrap_or(0);
                let i = if progress >= 1.0 { n - 1 } else { i };
                if !key_points {
                    // (a vertex after a moveto is not on the way along the path)
                    vertex = Some(vertices[i].1);
                }
                points[i]
            }
            CalcMode::Linear | CalcMode::Spline => {
                let t = times(n - 1);
                if n == 1 {
                    points[0]
                } else {
                    let i = (0..n - 1).rev().find(|&i| t[i] <= progress).unwrap_or(0);
                    let span = t[i + 1] - t[i];
                    let mut f = if span > 0.0 {
                        ((progress - t[i]) / span).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    if let (CalcMode::Spline, Some(s)) = (mode, splines.get(i)) {
                        f = cubic_bezier(s, f);
                    }
                    points[i] + (points[i + 1] - points[i]) * f
                }
            }
        };
        let p = vertex.unwrap_or_else(|| geom::point_at_length(&path, distance));
        let (mut x, mut y) = (p.x, p.y);
        // Accumulation (§3.8.1): the end of the path added per completed iteration.
        if sample.iteration > 0 && attr("accumulate").as_deref().map(str::trim) == Some("sum") {
            let end = geom::point_at_length(&path, length);
            x += end.x * sample.iteration as f64;
            y += end.y * sample.iteration as f64;
        }
        let rotate = match attr("rotate").as_deref().map(str::trim) {
            Some("auto") => geom::angle_at_length(&path, distance),
            Some("auto-reverse") => geom::angle_at_length(&path, distance) + 180.0,
            Some(v) => v.parse::<f64>().unwrap_or(0.0),
            None => 0.0,
        };
        Some(Matrix::translate((x, y)) * Matrix::rotate(rotate.to_radians()))
    }
}
