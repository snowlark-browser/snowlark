// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SVG DOM (SVG 2 §4 and the DOM interfaces of each chapter; SVG Animations; CSS Masking §14;
//! Filter Effects §15): the element interfaces, the reflection of content attributes through the
//! animated value objects and lists (§4.3.4–§4.6), the basic data types (`SVGNumber`,
//! `SVGLength`, `SVGAngle`, `SVGTransform`, `SVGPreserveAspectRatio` and the lists), and the
//! geometry methods over the renderer's SVG layout (`getBBox`, `getCTM`, `getTotalLength`...).
//!
//! # Objects
//!
//! [`SvgObject`] is every non-node SVG DOM object. An `SVGAnimated*` object names its element and
//! the [`reflect::Spec`] of the attribute it reflects; reading its base value parses the attribute
//! each time, writing it serializes into the attribute. The value objects (`SVGLength`...) are
//! [`Item`]s: detached (own value), reflecting the base value of an attribute (value read from the
//! attribute), or elements of a list (own value, kept in step with the attribute by the list's
//! synchronization, §4.5.4). Lists keep their elements as objects so that identity holds across
//! `getItem` calls and same-length attribute changes; a list synchronizes lazily, on each access,
//! when the attribute differs from the value it last synchronized with or serialized.
//!
//! The `DOMRect` of `viewBox`, the `DOMPoint`s of a point list and the `DOMMatrix` of an
//! `SVGTransform` are the geometry module's objects with a link to the SVG object they reflect
//! (`geometry::Rect::svg` and friends).

mod elements;
mod layout;
mod lists;
pub(crate) mod reflect;
pub(crate) mod smil;
pub(crate) mod use_tree;
mod values;

use snowlark_bindings::generated::SVGElementImpl;
use snowlark_bindings::rt::{self, Cx, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{ns, Dom, NodeId};
use snowlark_layout::svg::{Angle, Length, LengthDir, Transform};

use reflect::{Kind, Spec};

use super::{exception, node_handle, Engine, Object};
use rt::ExceptionName;

pub(crate) use layout::SvgState;

/// A non-node SVG DOM object.
pub enum SvgObject {
    /// An `SVGAnimated*` object.
    Animated {
        element: NodeId,
        spec: &'static Spec,
    },
    /// An `SVGNumber`, `SVGLength`, `SVGAngle` or `SVGTransform`.
    Item(Box<Item>),
    /// An `SVGPreserveAspectRatio` reflecting an element's attribute (`anim`: through `animVal`,
    /// read only).
    Aspect {
        element: NodeId,
        spec: &'static Spec,
        anim: bool,
    },
    /// A list interface object.
    List(Box<List>),
}

/// A value object.
pub struct Item {
    pub value: ItemValue,
    pub owner: Owner,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemValue {
    Number(f32),
    Length(Length, LengthDir),
    Angle(Angle),
    /// A transform and its `matrix` object, once made.
    Transform(Transform, Option<Handle>),
}

/// What a value object reflects.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Owner {
    Detached,
    /// The base value of an element's (non-list) attribute: the value is the attribute's.
    Attr {
        element: NodeId,
        spec: &'static Spec,
    },
    /// An element of a list.
    List(Handle),
}

/// A list interface object (§4.5.4).
pub struct List {
    pub element: NodeId,
    pub spec: &'static Spec,
    /// Reflected through `animVal` (read only).
    pub anim: bool,
    pub items: ListItems,
    /// The attribute value the list last synchronized with or serialized (`None`: never; the inner
    /// `None`: the attribute was absent).
    pub synced: Option<Option<String>>,
}

pub enum ListItems {
    /// `SVGNumber`, `SVGLength`, `SVGTransform` items, or `DOMPoint`s.
    Objects(Vec<Handle>),
    Strings(Vec<String>),
}

impl SvgObject {
    pub(crate) fn interface(&self) -> InterfaceId {
        use InterfaceId as I;
        match self {
            SvgObject::Animated { spec, .. } => match spec.kind {
                Kind::Length { .. } => I::SVGAnimatedLength,
                Kind::LengthList(_) => I::SVGAnimatedLengthList,
                Kind::Number { .. } | Kind::NumberPart { .. } => I::SVGAnimatedNumber,
                Kind::Integer { .. } | Kind::IntegerPart { .. } => I::SVGAnimatedInteger,
                Kind::NumberList => I::SVGAnimatedNumberList,
                Kind::Boolean => I::SVGAnimatedBoolean,
                Kind::Enum { .. } | Kind::OrientType => I::SVGAnimatedEnumeration,
                Kind::OrientAngle => I::SVGAnimatedAngle,
                Kind::String | Kind::Href => I::SVGAnimatedString,
                Kind::Rect => I::SVGAnimatedRect,
                Kind::Aspect => I::SVGAnimatedPreserveAspectRatio,
                Kind::TransformList => I::SVGAnimatedTransformList,
                // (lists are reflected directly, not through an animated object)
                Kind::PointList => I::SVGPointList,
                Kind::StringList { .. } => I::SVGStringList,
            },
            SvgObject::Item(i) => match i.value {
                ItemValue::Number(_) => I::SVGNumber,
                ItemValue::Length(..) => I::SVGLength,
                ItemValue::Angle(_) => I::SVGAngle,
                ItemValue::Transform(..) => I::SVGTransform,
            },
            SvgObject::Aspect { .. } => I::SVGPreserveAspectRatio,
            SvgObject::List(l) => match l.spec.kind {
                Kind::LengthList(_) => I::SVGLengthList,
                Kind::NumberList => I::SVGNumberList,
                Kind::PointList => I::SVGPointList,
                Kind::TransformList => I::SVGTransformList,
                _ => I::SVGStringList,
            },
        }
    }

    /// The element whose wrapper anchors this object's (`None`: the object is weak).
    pub(crate) fn gc_owner(&self) -> Option<NodeId> {
        match self {
            SvgObject::Animated { element, .. } | SvgObject::Aspect { element, .. } => {
                Some(*element)
            }
            SvgObject::List(l) => Some(l.element),
            SvgObject::Item(i) => match i.owner {
                Owner::Attr { element, .. } => Some(element),
                Owner::Detached | Owner::List(_) => None,
            },
        }
    }

    /// The objects this one keeps alive: a list's elements, a transform's matrix.
    pub(crate) fn gc_refs(&self, out: &mut Vec<Handle>) {
        match self {
            SvgObject::List(l) => {
                if let ListItems::Objects(items) = &l.items {
                    out.extend(items);
                }
            }
            SvgObject::Item(i) => {
                if let ItemValue::Transform(_, Some(m)) = i.value {
                    out.push(m);
                }
            }
            _ => {}
        }
    }
}

/// The interface of an element in the SVG namespace (SVG 2 §4.3.3: the one its element definition
/// names, `SVGElement` for unknown names).
pub(crate) fn element_interface(local: &str) -> InterfaceId {
    use InterfaceId as I;
    match local {
        "a" => I::SVGAElement,
        "animate" => I::SVGAnimateElement,
        "animateMotion" => I::SVGAnimateMotionElement,
        "animateTransform" => I::SVGAnimateTransformElement,
        "circle" => I::SVGCircleElement,
        "clipPath" => I::SVGClipPathElement,
        "defs" => I::SVGDefsElement,
        "desc" => I::SVGDescElement,
        "ellipse" => I::SVGEllipseElement,
        "feBlend" => I::SVGFEBlendElement,
        "feColorMatrix" => I::SVGFEColorMatrixElement,
        "feComponentTransfer" => I::SVGFEComponentTransferElement,
        "feComposite" => I::SVGFECompositeElement,
        "feConvolveMatrix" => I::SVGFEConvolveMatrixElement,
        "feDiffuseLighting" => I::SVGFEDiffuseLightingElement,
        "feDisplacementMap" => I::SVGFEDisplacementMapElement,
        "feDistantLight" => I::SVGFEDistantLightElement,
        "feDropShadow" => I::SVGFEDropShadowElement,
        "feFlood" => I::SVGFEFloodElement,
        "feFuncA" => I::SVGFEFuncAElement,
        "feFuncB" => I::SVGFEFuncBElement,
        "feFuncG" => I::SVGFEFuncGElement,
        "feFuncR" => I::SVGFEFuncRElement,
        "feGaussianBlur" => I::SVGFEGaussianBlurElement,
        "feImage" => I::SVGFEImageElement,
        "feMerge" => I::SVGFEMergeElement,
        "feMergeNode" => I::SVGFEMergeNodeElement,
        "feMorphology" => I::SVGFEMorphologyElement,
        "feOffset" => I::SVGFEOffsetElement,
        "fePointLight" => I::SVGFEPointLightElement,
        "feSpecularLighting" => I::SVGFESpecularLightingElement,
        "feSpotLight" => I::SVGFESpotLightElement,
        "feTile" => I::SVGFETileElement,
        "feTurbulence" => I::SVGFETurbulenceElement,
        "filter" => I::SVGFilterElement,
        "foreignObject" => I::SVGForeignObjectElement,
        "g" => I::SVGGElement,
        "image" => I::SVGImageElement,
        "line" => I::SVGLineElement,
        "linearGradient" => I::SVGLinearGradientElement,
        "marker" => I::SVGMarkerElement,
        "mask" => I::SVGMaskElement,
        "metadata" => I::SVGMetadataElement,
        "mpath" => I::SVGMPathElement,
        "path" => I::SVGPathElement,
        "pattern" => I::SVGPatternElement,
        "polygon" => I::SVGPolygonElement,
        "polyline" => I::SVGPolylineElement,
        "radialGradient" => I::SVGRadialGradientElement,
        "rect" => I::SVGRectElement,
        "script" => I::SVGScriptElement,
        "set" => I::SVGSetElement,
        "stop" => I::SVGStopElement,
        "style" => I::SVGStyleElement,
        "svg" => I::SVGSVGElement,
        "switch" => I::SVGSwitchElement,
        "symbol" => I::SVGSymbolElement,
        "text" => I::SVGTextElement,
        "textPath" => I::SVGTextPathElement,
        "title" => I::SVGTitleElement,
        "tspan" => I::SVGTSpanElement,
        "use" => I::SVGUseElement,
        "view" => I::SVGViewElement,
        _ => I::SVGElement,
    }
}

/// The event type of an SVG animation element's `onbegin`, `onend` and `onrepeat` handlers (SVG
/// Animations §5: they handle `beginEvent`, `endEvent` and `repeatEvent`); `None` for other
/// elements and handlers.
pub(crate) fn animation_event_type(dom: &Dom, el: NodeId, event: &str) -> Option<&'static str> {
    let e = dom.element(el)?;
    if e.name().ns != ns!(svg)
        || !matches!(
            &*e.name().local,
            "animate" | "set" | "animateMotion" | "animateTransform"
        )
    {
        return None;
    }
    match event {
        "begin" => Some("beginEvent"),
        "end" => Some("endEvent"),
        "repeat" => Some("repeatEvent"),
        _ => None,
    }
}

/// Whether `n` is an SVG element with local name `local`.
pub(crate) fn is_svg(dom: &Dom, n: NodeId, local: &str) -> bool {
    dom.element(n)
        .is_some_and(|e| e.name().ns == ns!(svg) && &*e.name().local == local)
}

/// Whether `n` is an element in the SVG namespace.
pub(crate) fn is_svg_element(dom: &Dom, n: NodeId) -> bool {
    dom.element(n).is_some_and(|e| e.name().ns == ns!(svg))
}

/// The parent element in the tree for SVG's purposes: a shadow root's host (`use` instances).
fn svg_parent(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let p = dom.parent(n)?;
    if dom.is_shadow_root(p) {
        return dom.host(p);
    }
    dom.is_element(p).then_some(p)
}

/// Whether an element establishes an SVG viewport for its descendants (SVG 2 §8.1: `svg`,
/// `symbol` instances, `image`, `foreignObject`).
fn establishes_viewport(dom: &Dom, n: NodeId) -> bool {
    dom.element(n).is_some_and(|e| {
        e.name().ns == ns!(svg)
            && (matches!(&*e.name().local, "svg" | "image" | "foreignObject")
                // (a `symbol` only where `use` instantiates it)
                || (&*e.name().local == "symbol"
                    && svg_parent(dom, n).is_some_and(|p| is_svg(dom, p, "use"))))
    })
}

impl Engine {
    /// The SVG object behind a handle the bindings brand-checked.
    pub(crate) fn svg_obj<R>(&self, h: Handle, f: impl FnOnce(&SvgObject) -> R) -> R {
        match &*self.obj(h) {
            Object::Svg(o) => f(o),
            _ => panic!("not an SVG object"),
        }
    }

    pub(crate) fn svg_obj_mut<R>(&self, h: Handle, f: impl FnOnce(&mut SvgObject) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Svg(o) => f(o),
            _ => panic!("not an SVG object"),
        }
    }

    /// The value of an attribute in the null namespace, owned.
    pub(crate) fn svg_attr(&self, el: NodeId, local: &str) -> Option<String> {
        let dom = self.dom();
        if !dom.is_alive(el) || !dom.is_element(el) {
            return None;
        }
        let i = dom.attr_index(el, "", local)?;
        Some(dom.attrs(el)[i].value().to_owned())
    }

    /// Sets an attribute in the null namespace of a live element.
    pub(crate) fn svg_set_attr(&self, cx: &mut Cx, el: NodeId, local: &str, value: &str) {
        if !self.dom().is_alive(el) {
            return;
        }
        self.set_attr(cx, el, local, value);
    }

    /// A freed SVG object: its list's elements and its transform's matrix become detached, keeping
    /// their values.
    pub(crate) fn svg_freed(&self, o: &SvgObject) {
        match o {
            SvgObject::List(l) => {
                if let ListItems::Objects(items) = &l.items {
                    for &h in items {
                        self.svg_detach(h);
                    }
                }
            }
            SvgObject::Item(i) => {
                if let ItemValue::Transform(t, Some(m)) = &i.value {
                    let mut objects = self.objects.borrow_mut();
                    if let Some(Object::DomMatrix(md)) = objects.get_mut(m.get() as usize - 1) {
                        md.svg = None;
                        md.m = values::matrix_of(&t.matrix);
                    }
                }
            }
            _ => {}
        }
    }
}

impl SVGElementImpl for Engine {
    fn svg_reflect(&self, this: Handle, owner: &'static str, member: &'static str) -> Handle {
        let element = self.n(this);
        // (the generator's list and `reflect::spec` are kept together; a member without a rule
        // reflects nothing rather than take the page down)
        let spec = reflect::spec(owner, member).unwrap_or(&reflect::NO_RULE);
        match spec.kind {
            // (lists that are not animated: the list itself)
            Kind::PointList | Kind::StringList { .. } => {
                self.new_svg_list(element, spec, member == "animatedPoints")
            }
            _ => self.alloc(Object::Svg(SvgObject::Animated { element, spec })),
        }
    }

    /// SVG 2 §4.4.1 `ownerSVGElement`: the nearest ancestor `svg` element (null for the outermost
    /// one).
    fn owner_svg_element(&self, this: Handle) -> Option<Handle> {
        let dom = self.dom();
        let mut n = svg_parent(&dom, self.n(this));
        while let Some(p) = n {
            // (an `svg` in a `foreignObject` is an outermost one)
            if !is_svg_element(&dom, p) || is_svg(&dom, p, "foreignObject") {
                return None;
            }
            if is_svg(&dom, p, "svg") {
                return Some(node_handle(p));
            }
            n = svg_parent(&dom, p);
        }
        None
    }

    /// SVG 2 §4.4.1 `viewportElement`: the nearest ancestor that establishes an SVG viewport (null
    /// for the outermost `svg` element).
    fn viewport_element(&self, this: Handle) -> Option<Handle> {
        let dom = self.dom();
        let mut n = svg_parent(&dom, self.n(this));
        while let Some(p) = n {
            // (an `svg` in a `foreignObject` is an outermost one)
            if !is_svg_element(&dom, p) || is_svg(&dom, p, "foreignObject") {
                return None;
            }
            if establishes_viewport(&dom, p) {
                return Some(node_handle(p));
            }
            n = svg_parent(&dom, p);
        }
        None
    }
}

impl Engine {
    /// `Document.rootElement` (SVG 2 §5.1.3): the document element when it is an `svg` element.
    pub(crate) fn svg_root_element(&self, doc: NodeId) -> Option<Handle> {
        let dom = self.dom();
        let root = dom.document_element(doc)?;
        is_svg(&dom, root, "svg").then(|| node_handle(root))
    }
}

/// A NoModificationAllowedError for writes to read-only objects.
pub(crate) fn read_only_error() -> rt::Error {
    exception(
        ExceptionName::NoModificationAllowedError,
        "The object is read-only.",
    )
}
