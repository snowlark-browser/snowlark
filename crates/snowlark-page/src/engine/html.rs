// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML elements (HTML §3.2, §4): the element interface table, `HTMLElement` and its mixins, and the
//! members of the simpler element interfaces. Form controls are in `forms.rs`, embedded content and
//! media in `media.rs`.

use std::collections::HashMap;

use snowlark_bindings::generated::{
    self as g, ARIAMixinImpl, ARIANotifyMixinImpl, ActivationBlockersMixinBlockerReason,
    ActivationBlockersMixinImpl, AriaNotificationOptions, BooleanOrUnrestrictedDoubleOrString,
    ElementContentEditableImpl, FocusOptions, HTMLAnchorElementImpl, HTMLBRElementImpl,
    HTMLBaseElementImpl, HTMLBodyElementImpl, HTMLDListElementImpl, HTMLDataElementImpl,
    HTMLDataListElementImpl, HTMLDetailsElementImpl, HTMLDirectoryElementImpl, HTMLDivElementImpl,
    HTMLElementImpl, HTMLFontElementImpl, HTMLFrameSetElementImpl, HTMLHRElementImpl,
    HTMLHeadElementImpl, HTMLHeadingElementImpl, HTMLHtmlElementImpl,
    HTMLHyperlinkElementUtilsImpl, HTMLLIElementImpl, HTMLLinkElementImpl, HTMLMapElementImpl,
    HTMLMarqueeElementImpl, HTMLMenuElementImpl, HTMLMetaElementImpl, HTMLModElementImpl,
    HTMLOListElementImpl, HTMLOrSVGOrMathMLElementImpl, HTMLParagraphElementImpl,
    HTMLParamElementImpl, HTMLPictureElementImpl, HTMLPreElementImpl, HTMLQuoteElementImpl,
    HTMLScriptElementImpl, HTMLSelectedContentElementImpl, HTMLSourceElementImpl,
    HTMLSpanElementImpl, HTMLStyleElementImpl, HTMLTableCaptionElementImpl,
    HTMLTableCellElementImpl, HTMLTableColElementImpl, HTMLTableElementImpl,
    HTMLTableRowElementImpl, HTMLTableSectionElementImpl, HTMLTemplateElementImpl,
    HTMLTimeElementImpl, HTMLTitleElementImpl, HTMLUListElementImpl, HTMLUnknownElementImpl,
    HyperlinkElementUtilsImpl, PermissionState, PopoverTargetAttributesImpl,
    PowerfulFeatureObserverImpl, RegionImpl, ShowPopoverOptions, TogglePopoverOptionsOrBoolean,
    TrustedScriptOrNullEmptyString, TrustedScriptOrString, TrustedScriptURLOrString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, Dom, LocalName, NodeId, QualName};

use super::collections::Filter;
use super::document::DocState;
use super::{exception, node_handle, Engine, Object};

/// Per-element HTML state the DOM does not keep.
#[derive(Default)]
pub struct HtmlState {
    pub docs: rustc_hash::FxHashMap<NodeId, DocState>,
    pub controls: rustc_hash::FxHashMap<NodeId, super::forms::ControlState>,
    /// The URL each `<img>`'s last `load` or `error` event was for (`None`: no source).
    /// The Content-Type essences of the `object` and `embed` resources that arrived (HTML §4.8.7):
    /// what they show from then on.
    pub embed_types: rustc_hash::FxHashMap<NodeId, String>,
    /// The "click in progress flag" of elements being clicked.
    pub clicking: Vec<NodeId>,
    /// The `details` element whose default summary the last hit test found the pointer on (the
    /// default summary is no node: a click on it targets the element).
    pub default_summary_hit: Option<NodeId>,
    /// Open popovers.
    pub popovers: Vec<NodeId>,
    /// Labels whose activation behavior is sending its click to their control.
    pub activating_labels: Vec<NodeId>,
    /// `dialog.returnValue`.
    pub dialog_return: rustc_hash::FxHashMap<NodeId, String>,
    /// Elements with explicitly set element reflection targets (ARIA element references).
    pub element_refs: HashMap<(NodeId, &'static str), Vec<NodeId>>,
    /// File inputs' selected files (FileList objects).
    pub files: rustc_hash::FxHashMap<NodeId, Handle>,
    /// Media elements' `srcObject`.
    pub src_objects: rustc_hash::FxHashMap<NodeId, Handle>,
    /// Custom elements' attached internals.
    pub internals: rustc_hash::FxHashMap<NodeId, Handle>,
    /// Forms whose entry list is being constructed, and forms firing submission events (HTML
    /// §4.10.21.3 steps 2 and 5).
    pub constructing_entry_list: rustc_hash::FxHashSet<NodeId>,
    pub firing_submission: rustc_hash::FxHashSet<NodeId>,
    /// HTML §4.10.21.3 each form's planned navigation not performed yet: the navigable it planned
    /// and the navigation (emptied when planned navigations are performed, between tasks).
    pub planned_navigations:
        rustc_hash::FxHashMap<NodeId, (super::frames::ContextId, super::frames::NavigationRequest)>,
    /// A focused text control's value when it got focus (its `change` event on blur).
    pub value_at_focus: rustc_hash::FxHashMap<NodeId, String>,
    /// Focused text controls whose value the user edited since they got focus.
    pub edited_since_focus: rustc_hash::FxHashSet<NodeId>,
}

/// The element interface for an element name (HTML §3.2.2 and each element's DOM interface).
pub fn element_interface(name: &QualName) -> InterfaceId {
    use InterfaceId as I;
    if name.ns == ns!(svg) {
        // SVG 2 §4.3.3: the interface of the element's definition, SVGElement for others.
        return super::svg::element_interface(&name.local);
    }
    if name.ns != ns!(html) {
        return I::Element;
    }
    match name.local {
        local_name!("a") => I::HTMLAnchorElement,
        local_name!("area") => I::HTMLAreaElement,
        local_name!("audio") => I::HTMLAudioElement,
        local_name!("base") => I::HTMLBaseElement,
        local_name!("blockquote") | local_name!("q") => I::HTMLQuoteElement,
        local_name!("body") => I::HTMLBodyElement,
        local_name!("br") => I::HTMLBRElement,
        local_name!("button") => I::HTMLButtonElement,
        local_name!("canvas") => I::HTMLCanvasElement,
        local_name!("caption") => I::HTMLTableCaptionElement,
        local_name!("col") | local_name!("colgroup") => I::HTMLTableColElement,
        local_name!("data") => I::HTMLDataElement,
        local_name!("datalist") => I::HTMLDataListElement,
        local_name!("del") | local_name!("ins") => I::HTMLModElement,
        local_name!("details") => I::HTMLDetailsElement,
        local_name!("dialog") => I::HTMLDialogElement,
        local_name!("dir") => I::HTMLDirectoryElement,
        local_name!("div") => I::HTMLDivElement,
        local_name!("dl") => I::HTMLDListElement,
        local_name!("embed") => I::HTMLEmbedElement,
        local_name!("fieldset") => I::HTMLFieldSetElement,
        local_name!("font") => I::HTMLFontElement,
        local_name!("form") => I::HTMLFormElement,
        local_name!("frame") => I::HTMLFrameElement,
        local_name!("frameset") => I::HTMLFrameSetElement,
        local_name!("h1")
        | local_name!("h2")
        | local_name!("h3")
        | local_name!("h4")
        | local_name!("h5")
        | local_name!("h6") => I::HTMLHeadingElement,
        local_name!("head") => I::HTMLHeadElement,
        local_name!("hr") => I::HTMLHRElement,
        local_name!("html") => I::HTMLHtmlElement,
        local_name!("iframe") => I::HTMLIFrameElement,
        local_name!("img") => I::HTMLImageElement,
        local_name!("input") => I::HTMLInputElement,
        local_name!("label") => I::HTMLLabelElement,
        local_name!("legend") => I::HTMLLegendElement,
        local_name!("li") => I::HTMLLIElement,
        local_name!("link") => I::HTMLLinkElement,
        local_name!("map") => I::HTMLMapElement,
        local_name!("marquee") => I::HTMLMarqueeElement,
        local_name!("menu") => I::HTMLMenuElement,
        local_name!("meta") => I::HTMLMetaElement,
        local_name!("meter") => I::HTMLMeterElement,
        local_name!("object") => I::HTMLObjectElement,
        local_name!("ol") => I::HTMLOListElement,
        local_name!("optgroup") => I::HTMLOptGroupElement,
        local_name!("option") => I::HTMLOptionElement,
        local_name!("output") => I::HTMLOutputElement,
        local_name!("p") => I::HTMLParagraphElement,
        local_name!("param") => I::HTMLParamElement,
        local_name!("picture") => I::HTMLPictureElement,
        local_name!("pre") | local_name!("listing") | local_name!("xmp") => I::HTMLPreElement,
        local_name!("progress") => I::HTMLProgressElement,
        local_name!("script") => I::HTMLScriptElement,
        local_name!("select") => I::HTMLSelectElement,
        local_name!("slot") => I::HTMLSlotElement,
        local_name!("source") => I::HTMLSourceElement,
        local_name!("span") => I::HTMLSpanElement,
        local_name!("style") => I::HTMLStyleElement,
        local_name!("table") => I::HTMLTableElement,
        local_name!("tbody") | local_name!("thead") | local_name!("tfoot") => {
            I::HTMLTableSectionElement
        }
        local_name!("td") | local_name!("th") => I::HTMLTableCellElement,
        local_name!("template") => I::HTMLTemplateElement,
        local_name!("textarea") => I::HTMLTextAreaElement,
        local_name!("time") => I::HTMLTimeElement,
        local_name!("title") => I::HTMLTitleElement,
        local_name!("tr") => I::HTMLTableRowElement,
        local_name!("track") => I::HTMLTrackElement,
        local_name!("ul") => I::HTMLUListElement,
        local_name!("video") => I::HTMLVideoElement,
        local_name!("abbr")
        | local_name!("acronym")
        | local_name!("address")
        | local_name!("article")
        | local_name!("aside")
        | local_name!("b")
        | local_name!("basefont")
        | local_name!("bdi")
        | local_name!("bdo")
        | local_name!("big")
        | local_name!("center")
        | local_name!("cite")
        | local_name!("code")
        | local_name!("dd")
        | local_name!("dfn")
        | local_name!("dt")
        | local_name!("em")
        | local_name!("figcaption")
        | local_name!("figure")
        | local_name!("footer")
        | local_name!("header")
        | local_name!("hgroup")
        | local_name!("i")
        | local_name!("kbd")
        | local_name!("main")
        | local_name!("mark")
        | local_name!("menuitem")
        | local_name!("nav")
        | local_name!("nobr")
        | local_name!("noembed")
        | local_name!("noframes")
        | local_name!("noscript")
        | local_name!("plaintext")
        | local_name!("rb")
        | local_name!("rp")
        | local_name!("rt")
        | local_name!("rtc")
        | local_name!("ruby")
        | local_name!("s")
        | local_name!("samp")
        | local_name!("section")
        | local_name!("small")
        | local_name!("strike")
        | local_name!("strong")
        | local_name!("sub")
        | local_name!("summary")
        | local_name!("sup")
        | local_name!("tt")
        | local_name!("u")
        | local_name!("var")
        | local_name!("wbr") => I::HTMLElement,
        _ => {
            if &*name.local == "search" || &*name.local == "selectedcontent" {
                return if &*name.local == "search" {
                    I::HTMLElement
                } else {
                    I::HTMLSelectedContentElement
                };
            }
            if snowlark_dom::names::is_valid_custom_element_name(&name.local) {
                I::HTMLElement
            } else {
                I::HTMLUnknownElement
            }
        }
    }
}

/// The uppercase tag name of the common HTML elements, so `tagName` needs no allocation.
pub fn upper_tag(local: &LocalName) -> Option<&'static str> {
    Some(match *local {
        local_name!("a") => "A",
        local_name!("b") => "B",
        local_name!("body") => "BODY",
        local_name!("br") => "BR",
        local_name!("button") => "BUTTON",
        local_name!("div") => "DIV",
        local_name!("em") => "EM",
        local_name!("form") => "FORM",
        local_name!("h1") => "H1",
        local_name!("h2") => "H2",
        local_name!("h3") => "H3",
        local_name!("head") => "HEAD",
        local_name!("html") => "HTML",
        local_name!("i") => "I",
        local_name!("iframe") => "IFRAME",
        local_name!("img") => "IMG",
        local_name!("input") => "INPUT",
        local_name!("label") => "LABEL",
        local_name!("li") => "LI",
        local_name!("link") => "LINK",
        local_name!("meta") => "META",
        local_name!("ol") => "OL",
        local_name!("option") => "OPTION",
        local_name!("p") => "P",
        local_name!("pre") => "PRE",
        local_name!("script") => "SCRIPT",
        local_name!("section") => "SECTION",
        local_name!("select") => "SELECT",
        local_name!("slot") => "SLOT",
        local_name!("span") => "SPAN",
        local_name!("strong") => "STRONG",
        local_name!("style") => "STYLE",
        local_name!("table") => "TABLE",
        local_name!("tbody") => "TBODY",
        local_name!("td") => "TD",
        local_name!("template") => "TEMPLATE",
        local_name!("textarea") => "TEXTAREA",
        local_name!("th") => "TH",
        local_name!("title") => "TITLE",
        local_name!("tr") => "TR",
        local_name!("ul") => "UL",
        _ => return None,
    })
}

/// Whether an interface is MouseEvent or one inheriting from it.
pub fn is_mouse_event(i: InterfaceId) -> bool {
    matches!(
        i,
        InterfaceId::MouseEvent | InterfaceId::PointerEvent | InterfaceId::WheelEvent
    )
}

/// The event handlers of GlobalEventHandlers, WindowEventHandlers and DocumentAndElementEventHandlers,
/// by event type, so content attributes find the `&'static str` the handler map is keyed by.
const EVENT_HANDLERS: &[&str] = &[
    "abort",
    "auxclick",
    "beforeinput",
    "beforematch",
    "beforetoggle",
    "blur",
    "cancel",
    "canplay",
    "canplaythrough",
    "change",
    "click",
    "close",
    "command",
    "contextlost",
    "contextmenu",
    "contextrestored",
    "copy",
    "cuechange",
    "cut",
    "dblclick",
    "drag",
    "dragend",
    "dragenter",
    "dragleave",
    "dragover",
    "dragstart",
    "drop",
    "durationchange",
    "emptied",
    "ended",
    "error",
    "focus",
    "formdata",
    "input",
    "invalid",
    "keydown",
    "keypress",
    "keyup",
    "load",
    "loadeddata",
    "loadedmetadata",
    "loadstart",
    "mousedown",
    "mouseenter",
    "mouseleave",
    "mousemove",
    "mouseout",
    "mouseover",
    "mouseup",
    "paste",
    "pause",
    "play",
    "playing",
    "progress",
    "ratechange",
    "reset",
    "resize",
    "scroll",
    "scrollend",
    "securitypolicyviolation",
    "seeked",
    "seeking",
    "select",
    "slotchange",
    "stalled",
    "submit",
    "suspend",
    "timeupdate",
    "toggle",
    "volumechange",
    "waiting",
    "webkitanimationend",
    "webkitanimationiteration",
    "webkitanimationstart",
    "webkittransitionend",
    "wheel",
    "afterprint",
    "beforeprint",
    "beforeunload",
    "hashchange",
    "languagechange",
    "message",
    "messageerror",
    "offline",
    "online",
    "pagehide",
    "pagereveal",
    "pageshow",
    "pageswap",
    "popstate",
    "rejectionhandled",
    "storage",
    "unhandledrejection",
    "unload",
    "animationstart",
    "animationiteration",
    "animationend",
    "animationcancel",
    "transitionrun",
    "transitionstart",
    "transitionend",
    "transitioncancel",
    "pointerover",
    "pointerenter",
    "pointerdown",
    "pointermove",
    "pointerrawupdate",
    "pointerup",
    "pointercancel",
    "pointerout",
    "pointerleave",
    "gotpointercapture",
    "lostpointercapture",
    "selectstart",
    "selectionchange",
    "touchstart",
    "touchend",
    "touchmove",
    "touchcancel",
    "focusin",
    "focusout",
    "readystatechange",
    "visibilitychange",
    "fullscreenchange",
    "fullscreenerror",
];

/// The `&'static str` of an event handler's event type, if `name` is one.
pub fn event_handler_name(name: &str) -> Option<&'static str> {
    EVENT_HANDLERS.iter().copied().find(|e| *e == name)
}

/// The event type an event handler handles, given its name without "on" (HTML §8.1.8.2): the
/// prefixed animation and transition handlers handle the camel-cased legacy types.
pub fn handler_event_type(event: &'static str) -> &'static str {
    match event {
        "webkitanimationend" => "webkitAnimationEnd",
        "webkitanimationiteration" => "webkitAnimationIteration",
        "webkitanimationstart" => "webkitAnimationStart",
        "webkittransitionend" => "webkitTransitionEnd",
        e => e,
    }
}

/// Whether an element's `on<event>` attribute is an event handler content attribute (HTML §8.1.8.2):
/// GlobalEventHandlers' are every element's, WindowEventHandlers' only `body`'s and `frameset`'s,
/// and DocumentEventHandlers' (`readystatechange`, `visibilitychange`) no element's.
pub fn is_handler_content_attribute(dom: &Dom, el: NodeId, event: &str) -> bool {
    match event {
        "readystatechange" | "visibilitychange" => false,
        "afterprint" | "beforeprint" | "beforeunload" | "hashchange" | "languagechange"
        | "message" | "messageerror" | "offline" | "online" | "pagehide" | "pagereveal"
        | "pageshow" | "pageswap" | "popstate" | "rejectionhandled" | "storage"
        | "unhandledrejection" | "unload" => {
            is(dom, el, &local_name!("body")) || is(dom, el, &local_name!("frameset"))
        }
        _ => true,
    }
}

/// The Window-reflecting body element event handler set (HTML §8.1.8.2): `body` and `frameset`
/// forward these to the window.
pub fn is_window_reflecting_handler(event: &str) -> bool {
    matches!(
        event,
        "blur"
            | "error"
            | "focus"
            | "load"
            | "resize"
            | "scroll"
            | "afterprint"
            | "beforeprint"
            | "beforeunload"
            | "hashchange"
            | "languagechange"
            | "message"
            | "messageerror"
            | "offline"
            | "online"
            | "pagehide"
            | "pagereveal"
            | "pageshow"
            | "pageswap"
            | "popstate"
            | "rejectionhandled"
            | "storage"
            | "unhandledrejection"
            | "unload"
    )
}

fn is(dom: &Dom, id: NodeId, local: &LocalName) -> bool {
    dom.is_html_element_named(id, local)
}

fn attr<'a>(dom: &'a Dom, id: NodeId, local: &LocalName) -> Option<&'a str> {
    dom.attr(id, &ns!(), local)
}

/// Whether `el` is a shadow host whose shadow root's delegates focus is true.
pub(crate) fn delegates_focus(dom: &Dom, el: NodeId) -> bool {
    dom.shadow_root(el)
        .and_then(|s| dom.shadow_root_data(s))
        .is_some_and(|d| d.delegates_focus())
}

/// Labelable elements (HTML §4.10.2).
pub fn is_labelable(dom: &Dom, id: NodeId) -> bool {
    let Some(e) = dom.element(id) else {
        return false;
    };
    if e.name().ns != ns!(html) {
        return false;
    }
    match e.name().local {
        local_name!("button")
        | local_name!("meter")
        | local_name!("output")
        | local_name!("progress")
        | local_name!("select")
        | local_name!("textarea") => true,
        local_name!("input") => {
            !attr(dom, id, &local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
        }
        _ => false,
    }
}

/// A label's labeled control (HTML §4.10.4).
pub fn labeled_control(dom: &Dom, label: NodeId) -> Option<NodeId> {
    if let Some(for_) = attr(dom, label, &local_name!("for")) {
        let root = dom.root(label);
        let target = dom
            .descendants(root)
            .find(|&d| dom.is_element(d) && attr(dom, d, &local_name!("id")) == Some(for_))?;
        return is_labelable(dom, target).then_some(target);
    }
    dom.descendants(label).find(|&d| is_labelable(dom, d))
}

/// Elements that can be disabled (HTML §4.10.18.5 "actually disabled").
pub fn can_be_disabled(dom: &Dom, id: NodeId) -> bool {
    let Some(e) = dom.element(id) else {
        return false;
    };
    e.name().ns == ns!(html)
        && (matches!(
            e.name().local,
            local_name!("button")
                | local_name!("input")
                | local_name!("select")
                | local_name!("textarea")
                | local_name!("optgroup")
                | local_name!("option")
                | local_name!("fieldset")
        ) || dom.element_flags(id) & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0)
}

/// "Actually disabled" / `:disabled`.
pub fn is_disabled(dom: &Dom, id: NodeId) -> bool {
    if !can_be_disabled(dom, id) {
        return false;
    }
    if attr(dom, id, &local_name!("disabled")).is_some() {
        return true;
    }
    if is(dom, id, &local_name!("option")) {
        return dom.parent(id).is_some_and(|p| {
            is(dom, p, &local_name!("optgroup")) && attr(dom, p, &local_name!("disabled")).is_some()
        });
    }
    if is(dom, id, &local_name!("optgroup")) {
        return false;
    }
    // A disabled fieldset ancestor disables its descendants, except those in its first legend.
    let mut child = id;
    for a in dom.ancestors(id) {
        if is(dom, a, &local_name!("fieldset")) && attr(dom, a, &local_name!("disabled")).is_some()
        {
            let first_legend = dom
                .element_children(a)
                .find(|&c| is(dom, c, &local_name!("legend")));
            if first_legend != Some(child) {
                return true;
            }
        }
        child = a;
    }
    false
}

/// Interactive content (HTML §3.2.5.2.7).
pub fn is_interactive_content(dom: &Dom, id: NodeId) -> bool {
    let Some(e) = dom.element(id) else {
        return false;
    };
    if e.name().ns != ns!(html) {
        return false;
    }
    match e.name().local {
        local_name!("a") => attr(dom, id, &local_name!("href")).is_some(),
        local_name!("audio") | local_name!("video") => {
            attr(dom, id, &local_name!("controls")).is_some()
        }
        local_name!("img") => attr(dom, id, &local_name!("usemap")).is_some(),
        local_name!("input") => {
            !attr(dom, id, &local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
        }
        local_name!("button")
        | local_name!("details")
        | local_name!("embed")
        | local_name!("iframe")
        | local_name!("label")
        | local_name!("select")
        | local_name!("textarea") => true,
        _ => false,
    }
}

/// Elements `:required` and `:optional` apply to.
pub fn is_required_capable(dom: &Dom, id: NodeId) -> bool {
    is(dom, id, &local_name!("input"))
        || is(dom, id, &local_name!("select"))
        || is(dom, id, &local_name!("textarea"))
}

/// `:read-write` (HTML §4.16.3).
pub fn is_read_write(dom: &Dom, id: NodeId) -> bool {
    if is(dom, id, &local_name!("input")) {
        let t = attr(dom, id, &local_name!("type"))
            .unwrap_or("text")
            .to_ascii_lowercase();
        let mutable_type = matches!(
            t.as_str(),
            "text"
                | "search"
                | "url"
                | "tel"
                | "email"
                | "password"
                | "date"
                | "month"
                | "week"
                | "time"
                | "datetime-local"
                | "number"
        ) || !matches!(
            t.as_str(),
            "hidden"
                | "range"
                | "color"
                | "checkbox"
                | "radio"
                | "file"
                | "submit"
                | "image"
                | "reset"
                | "button"
        );
        return mutable_type
            && attr(dom, id, &local_name!("readonly")).is_none()
            && !is_disabled(dom, id);
    }
    if is(dom, id, &local_name!("textarea")) {
        return attr(dom, id, &local_name!("readonly")).is_none() && !is_disabled(dom, id);
    }
    // Editing hosts.
    dom.inclusive_ancestors(id)
        .filter(|&a| dom.is_element(a))
        .find_map(|a| attr(dom, a, &LocalName::from("contenteditable")))
        .is_some_and(|v| {
            v.is_empty()
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("plaintext-only")
        })
}

impl Engine {
    /// Whether `el` is focusable (HTML §6.6.2, without layout): has a tabindex, or is a focusable
    /// element type that is not disabled.
    pub(crate) fn is_focusable(&self, el: NodeId) -> bool {
        // (inert nodes are not focusable: HTML §6.3)
        if self.is_inert(el) {
            return false;
        }
        let dom = self.dom();
        if !dom.is_connected(el) || !dom.is_element(el) {
            return false;
        }
        // (a shadow host whose shadow root delegates focus is not a focusable area itself: focus goes
        // to its focus delegate)
        if delegates_focus(&dom, el) {
            return false;
        }
        if attr(&dom, el, &local_name!("tabindex")).is_some_and(|t| rt::parse_integer(t).is_some())
        {
            return !is_disabled(&dom, el);
        }
        let e = dom.element(el).unwrap();
        if e.name().ns != ns!(html) {
            return false;
        }
        match e.name().local {
            local_name!("a") | local_name!("area") => {
                attr(&dom, el, &local_name!("href")).is_some()
            }
            local_name!("button")
            | local_name!("select")
            | local_name!("textarea")
            | local_name!("iframe") => !is_disabled(&dom, el),
            local_name!("input") => {
                !is_disabled(&dom, el)
                    && !attr(&dom, el, &local_name!("type"))
                        .is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
            }
            local_name!("summary") => dom.parent(el).is_some_and(|p| {
                is(&dom, p, &local_name!("details"))
                    && dom
                        .element_children(p)
                        .find(|&c| is(&dom, c, &local_name!("summary")))
                        == Some(el)
            }),
            _ => attr(&dom, el, &LocalName::from("contenteditable")).is_some_and(|v| {
                v.is_empty()
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("plaintext-only")
            }),
        }
    }

    /// HTML §6.6.4 "get the focusable area" for an element that is not a focusable area itself: for
    /// a shadow host that delegates focus, the focused element when it is in the host's shadow
    /// trees, else the host's focus delegate.
    pub(crate) fn focusable_area_for(&self, el: NodeId) -> Option<NodeId> {
        if !delegates_focus(&self.dom(), el) {
            return None;
        }
        let doc = self.dom().node_document(el);
        if let Some(f) = self.focused_element(doc) {
            let dom = self.dom();
            if dom.is_alive(f) && dom.is_shadow_including_inclusive_ancestor_of(el, f) {
                return Some(f);
            }
        }
        self.focus_delegate(el, false)
    }

    /// HTML §6.6.4 "focus delegate" for `target` (a shadow host that delegates focus, a dialog or
    /// another element): its autofocus delegate, else the first focusable area among the
    /// descendants of the shadow root (or of the element) in tree order, each shadow host there
    /// that delegates focus searched in turn. Slotted elements are not descendants of the shadow
    /// root and are never chosen. A dialog takes only sequentially focusable descendants.
    pub(crate) fn focus_delegate(&self, target: NodeId, dialog: bool) -> Option<NodeId> {
        let descendants: Vec<NodeId> = {
            let dom = self.dom();
            let where_to_look = match dom.shadow_root(target) {
                Some(s) if delegates_focus(&dom, target) => s,
                Some(_) => return None,
                None => target,
            };
            dom.descendants(where_to_look)
                .filter(|&n| dom.is_element(n))
                .collect()
        };
        // The autofocus delegate.
        for &d in &descendants {
            if self
                .dom()
                .attr(d, &ns!(), &local_name!("autofocus"))
                .is_none()
            {
                continue;
            }
            let area = if self.is_focusable_area(d) {
                Some(d)
            } else {
                self.focusable_area_for(d)
            };
            if area.is_some() {
                return area;
            }
        }
        for &d in &descendants {
            if self.is_focusable_area(d) {
                let sequential = !dialog
                    || self
                        .dom()
                        .get_attribute(d, "tabindex")
                        .and_then(rt::parse_integer)
                        .is_none_or(|t| t >= 0);
                if sequential {
                    return Some(d);
                }
            }
            if let Some(area) = self.focusable_area_for(d) {
                return Some(area);
            }
        }
        None
    }

    /// HTML §6.6.4 the `autofocus` steps when an element with the attribute is inserted into a
    /// document: it becomes the last autofocus candidate of the top-level document, unless that
    /// document already processed its candidates, or the element's document is not fully active
    /// or has an ancestor document of another origin.
    pub(crate) fn autofocus_inserted(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        if !self.is_fully_active(doc) {
            return;
        }
        let origin = self.origin_of_document(doc);
        let mut top = doc;
        while let Some(parent) = self.parent_document(top) {
            if self.origin_of_document(parent) != origin {
                return;
            }
            top = parent;
        }
        self.doc_state(top, |s| {
            if !s.autofocus_processed {
                s.autofocus_candidates.retain(|&c| c != el);
                s.autofocus_candidates.push(el);
            }
        });
    }

    /// The last steps of HTML's dialog and popover focusing steps: once `control` was focused, the
    /// top-level document (when of the same origin) drops its autofocus candidates and collects
    /// no more.
    pub(crate) fn autofocus_preempted(&self, control: NodeId) {
        let doc = self.dom().node_document(control);
        let mut top = doc;
        while let Some(parent) = self.parent_document(top) {
            top = parent;
        }
        if self.origin_of_document(doc) != self.origin_of_document(top) {
            return;
        }
        self.doc_state(top, |s| {
            s.autofocus_candidates.clear();
            s.autofocus_processed = true;
        });
    }

    /// HTML §6.6.4 "flush autofocus candidates" for a top-level document (a step of the rendering
    /// update): nothing when something has the focus already; otherwise the first candidate that
    /// is (or delegates to) a focusable area takes it, and the document stops collecting.
    pub(crate) fn flush_autofocus_candidates(&self, cx: &mut Cx, top: NodeId) {
        let (processed, empty) = self.doc_state(top, |s| {
            (s.autofocus_processed, s.autofocus_candidates.is_empty())
        });
        if processed || empty {
            return;
        }
        // (4: something has the focus, or the document has a target element)
        if self.focused_element(top).is_some()
            || self.doc_state(top, |s| s.target_element).is_some()
        {
            self.doc_state(top, |s| {
                s.autofocus_candidates.clear();
                s.autofocus_processed = true;
            });
            return;
        }
        loop {
            let Some(el) = self.doc_state(top, |s| s.autofocus_candidates.first().copied()) else {
                return;
            };
            self.doc_state(top, |s| {
                s.autofocus_candidates.remove(0);
            });
            let doc = {
                let dom = self.dom();
                if !dom.is_alive(el) || !dom.is_connected(el) {
                    continue;
                }
                dom.node_document(el)
            };
            if !self.is_fully_active(doc) {
                continue;
            }
            // (a candidate that moved to another top-level traversable's document is dropped)
            let mut its_top = doc;
            while let Some(parent) = self.parent_document(its_top) {
                its_top = parent;
            }
            if its_top != top {
                continue;
            }
            // (7: skipped when its document or an ancestor document has a target element)
            let mut ancestor = Some(doc);
            let mut has_target = false;
            while let Some(d) = ancestor {
                has_target |= self.doc_state(d, |s| s.target_element).is_some();
                ancestor = self.parent_document(d);
            }
            if has_target {
                continue;
            }
            let target = if self.is_focusable_area(el) {
                Some(el)
            } else {
                self.focusable_area_for(el)
            };
            if let Some(target) = target {
                self.doc_state(top, |s| {
                    s.autofocus_candidates.clear();
                    s.autofocus_processed = true;
                });
                self.focus_element(cx, target);
                return;
            }
        }
    }

    /// The focused element of a document.
    pub(crate) fn focused_element(&self, doc: NodeId) -> Option<NodeId> {
        self.doc_state(doc, |s| s.focused)
    }

    /// HTML §6.6.4 "focusing steps" for an element, firing focus/blur and focusin/focusout.
    pub(crate) fn focus_element(&self, cx: &mut Cx, el: NodeId) {
        // 1-2: a shadow host that delegates focus focuses its focusable area instead.
        let el = if self.is_focusable(el) {
            el
        } else {
            match self.focusable_area_for(el) {
                Some(area) if self.is_focusable(area) => area,
                _ => return,
            }
        };
        let doc = self.dom().node_document(el);
        // (the focus chain from the top-level document: a frame's element takes the focus through
        // its containers, and other frames lose theirs)
        if !self.frame_renders.borrow().is_empty() {
            self.focus_frame_chain(cx, doc);
        }
        let old = self.doc_state(doc, |s| s.focused);
        if old == Some(el) {
            return;
        }
        if let Some(o) = old {
            self.doc_state(doc, |s| s.focused = None);
            self.text_control_unfocused(cx, o);
            self.fire_focus_event(cx, o, "blur", false, Some(el));
            self.fire_focus_event(cx, o, "focusout", true, Some(el));
        }
        self.doc_state(doc, |s| {
            s.focused = Some(el);
            s.nav_start = Some(el);
        });
        if self.is_text_control(el) {
            let v = self.control_value(el);
            let mut html = self.html.borrow_mut();
            html.value_at_focus.insert(el, v);
            html.edited_since_focus.remove(&el);
        }
        // (style follows in a rendered document; `:focus-visible` for selector queries in any)
        self.focus_states_changed(old, Some(el));
        // (a focused editing host gets a caret if the selection is not in it, before the focus
        // events: UI Events "the focus MUST be taken from the element before the dispatch")
        self.place_caret_on_focus(el);
        self.fire_focus_event(cx, el, "focus", false, old);
        self.fire_focus_event(cx, el, "focusin", true, old);
    }

    /// HTML §4.10.5.5: a text control whose value the user changed fires `change` when it loses
    /// focus (a value only script changed does not).
    fn text_control_unfocused(&self, cx: &mut Cx, el: NodeId) {
        let (before, edited) = {
            let mut html = self.html.borrow_mut();
            let edited = html.edited_since_focus.remove(&el);
            (html.value_at_focus.remove(&el), edited)
        };
        let Some(before) = before else {
            return;
        };
        if !edited || !self.dom().is_alive(el) {
            return;
        }
        // (a field the user edited has its user validity once it loses focus, whether or not the
        // edits changed its value)
        self.set_user_validity(el, true);
        if self.control_value(el) != before {
            self.fire_event(cx, node_handle(el), "change", true, false);
        }
    }

    /// "Unfocusing steps".
    /// HTML §8.1.7.3 "update the rendering": "for each doc of docs, if the focused area of doc is not
    /// a focusable area, then run the focusing steps for doc's viewport" (after the resize
    /// observations, before the intersection observations). A focused element that became
    /// disabled, inert, not rendered, invisible or lost what made it focusable is blurred.
    pub(crate) fn focus_fixup(&self, cx: &mut Cx, docs: &[NodeId]) {
        for &doc in docs {
            let Some(f) = self.doc_state(doc, |s| s.focused) else {
                continue;
            };
            if !self.dom().is_alive(f) || self.is_focusable_area(f) {
                continue;
            }
            self.blur_element(cx, f);
        }
    }

    /// HTML §6.6.2 "focusable area" for an element: focusable, being rendered and visible.
    pub(crate) fn is_focusable_area(&self, el: NodeId) -> bool {
        if !self.is_focusable(el) || !self.being_rendered(el) {
            return false;
        }
        self.with_style(el, |r| {
            r.element_style(el).is_none_or(|s| {
                snowlark_layout::Style::visibility(&s)
                    == snowlark_layout::style::Visibility::Visible
            })
        })
        .unwrap_or(true)
    }

    pub(crate) fn blur_element(&self, cx: &mut Cx, el: NodeId) {
        let doc = self.dom().node_document(el);
        // "Unfocusing steps" 1: a shadow host that delegates focus stands for the focused element in
        // its shadow trees.
        let el = match self.focused_element(doc) {
            Some(f) if f != el && delegates_focus(&self.dom(), el) => {
                let dom = self.dom();
                let inside = dom.shadow_root(el).is_some_and(|s| {
                    dom.is_alive(f) && dom.is_shadow_including_inclusive_ancestor_of(s, f)
                });
                if inside {
                    f
                } else {
                    el
                }
            }
            _ => el,
        };
        if self.doc_state(doc, |s| s.focused) != Some(el) {
            return;
        }
        self.doc_state(doc, |s| s.focused = None);
        self.text_control_unfocused(cx, el);
        self.focus_states_changed(Some(el), None);
        self.fire_focus_event(cx, el, "blur", false, None);
        self.fire_focus_event(cx, el, "focusout", true, None);
    }

    fn fire_focus_event(
        &self,
        cx: &mut Cx,
        target: NodeId,
        type_: &str,
        bubbles: bool,
        related: Option<NodeId>,
    ) {
        let u = super::events::UiFields {
            view: self.window_of_document(self.dom().node_document(target)),
            related_target: related.map(node_handle),
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::FocusEvent,
            type_,
            bubbles,
            false,
            true,
            super::events::EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| {
            ev.is_trusted = true;
            ev.related_target = related.map(node_handle);
        });
        self.dispatch(cx, e, node_handle(target), false);
    }

    // ----- activation behavior (HTML §6.4.4 and the elements') ------------------------------------------

    /// HTML §4.6.4 "follow the hyperlink" of an `a` or `area` activated by `event`: its `target`
    /// (Ctrl, or Shift, opens a new tab), a `javascript:` URL run in the document's window.
    pub(crate) fn follow_hyperlink(&self, cx: &mut Cx, el: NodeId, event: Handle) {
        let (href, target) = {
            let dom = self.dom();
            (
                attr(&dom, el, &local_name!("href"))
                    .unwrap_or("")
                    .trim()
                    .to_owned(),
                attr(&dom, el, &local_name!("target")).map(str::to_owned),
            )
        };
        let doc = self.dom().node_document(el);
        let Ok(url) = self.base_url_of(el).join(&href) else {
            return;
        };
        let (ctrl, shift) = match &*self.obj(event) {
            super::Object::Event(e) => match &e.ext {
                super::events::EventExt::Ui(u) => (u.ctrl || u.meta, u.shift),
                _ => (false, false),
            },
            _ => (false, false),
        };
        if url.scheme() == "javascript" {
            // CSP §4.2.4: a `javascript:` URL is inline script ("navigation"), checked against the
            // document's policy; a sandbox without scripts runs nothing.
            if self.policy.borrow().any
                && (self.scripts_sandboxed(doc)
                    || self.inline_blocked(
                        cx,
                        el,
                        snowlark_net::csp::InlineType::Navigation,
                        url.as_str(),
                    ))
            {
                return;
            }
            // HTML "navigate" step 13: the URL's script runs in a task of its own ("navigate to a
            // `javascript:` URL"), in the link's document (its `target` is not followed).
            if let Some(id) = self.context_of_document(doc) {
                self.navigate_request(
                    id,
                    super::frames::NavigationRequest {
                        url,
                        post: None,
                        source: Some(doc),
                        form: false,
                        reload: false,
                        referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                        history: Default::default(),
                    },
                );
            }
            return;
        }
        if ctrl || shift {
            self.open_requests.borrow_mut().push((url, ctrl && !shift));
            return;
        }
        let target = target.or_else(|| self.base_target(doc)).unwrap_or_default();
        self.navigate_element_target(cx, el, &target, url, None, false);
    }

    pub(crate) fn has_activation_behavior(&self, target: Handle) -> bool {
        let Some(n) = super::handle_node(target) else {
            return false;
        };
        let dom = self.dom();
        if is(&dom, n, &local_name!("input")) {
            return true;
        }
        if is(&dom, n, &local_name!("button")) || is(&dom, n, &local_name!("label")) {
            return true;
        }
        if (is(&dom, n, &local_name!("a")) || is(&dom, n, &local_name!("area")))
            && attr(&dom, n, &local_name!("href")).is_some()
        {
            return true;
        }
        // (HTML 4.11.2: every `summary` has the activation behavior; it does nothing unless the
        // element is a summary for its parent details)
        is(&dom, n, &local_name!("summary"))
            // (and a details element's default summary, which the pointer is on)
            || self.html.borrow().default_summary_hit == Some(n)
                && is(&dom, n, &local_name!("details"))
    }

    /// Legacy-pre-activation behavior (checkbox and radio inputs): returns the state to restore if the
    /// event is canceled.
    pub(crate) fn legacy_pre_activation(
        &self,
        cx: &mut Cx,
        target: Handle,
    ) -> Option<(bool, bool, Option<NodeId>)> {
        let n = super::handle_node(target)?;
        let kind = self.input_type(n)?;
        match kind.as_str() {
            "checkbox" => {
                let old = (self.checkedness(n), self.indeterminate(n), None);
                self.set_checkedness(cx, n, !old.0);
                self.set_indeterminate_state(n, false);
                Some(old)
            }
            "radio" => {
                let previously = self.checked_radio_in_group(n);
                let old = (self.checkedness(n), false, previously);
                self.set_checkedness(cx, n, true);
                Some(old)
            }
            _ => None,
        }
    }

    /// Legacy-canceled-activation behavior: restore the checkbox or radio.
    pub(crate) fn legacy_canceled_activation(
        &self,
        cx: &mut Cx,
        target: Handle,
        state: (bool, bool, Option<NodeId>),
    ) {
        let Some(n) = super::handle_node(target) else {
            return;
        };
        match self.input_type(n).as_deref() {
            Some("checkbox") => {
                self.set_checkedness(cx, n, state.0);
                self.set_indeterminate_state(n, state.1);
            }
            Some("radio") => {
                if let Some(prev) = state.2 {
                    if self.dom().is_alive(prev) {
                        self.set_checkedness(cx, prev, true);
                        return;
                    }
                }
                self.set_checkedness(cx, n, false);
            }
            _ => {}
        }
    }

    /// HTML §4.10.5 "show the picker, if applicable" for an input or select: with transient
    /// activation and a mutable element, the user activation is consumed and the picker shows (a
    /// drop-down select's list; inputs have none here). Whether the element is connected does not
    /// matter.
    pub(crate) fn show_picker_if_applicable(&self, n: NodeId) {
        let doc = self.dom().node_document(n);
        if !self.has_transient_activation(doc) || !self.picker_control_is_mutable(n) {
            return;
        }
        self.consume_user_activation(doc);
        if self.dom().is_html_element_named(n, &local_name!("select")) {
            self.open_select_popup(n);
        }
    }

    /// Whether an input or select is mutable for its picker: not disabled, and for the input types
    /// `readonly` applies to (HTML §4.10.5.3, "Common input element attributes"), not read-only.
    pub(crate) fn picker_control_is_mutable(&self, n: NodeId) -> bool {
        let readonly_applies = matches!(
            self.input_type(n).as_deref(),
            Some(
                "text"
                    | "search"
                    | "url"
                    | "tel"
                    | "email"
                    | "password"
                    | "date"
                    | "month"
                    | "week"
                    | "time"
                    | "datetime-local"
                    | "number"
            )
        );
        let dom = self.dom();
        !is_disabled(&dom, n)
            && !(readonly_applies && attr(&dom, n, &local_name!("readonly")).is_some())
    }

    /// The activation behavior of `target` for `event` (not canceled).
    pub(crate) fn activation_behavior(&self, cx: &mut Cx, target: Handle, event: Handle) {
        let Some(n) = super::handle_node(target) else {
            return;
        };
        let is_link = {
            let dom = self.dom();
            // An editable link is edited, not followed.
            (is(&dom, n, &local_name!("a")) || is(&dom, n, &local_name!("area")))
                && attr(&dom, n, &local_name!("href")).is_some()
                && !is_read_write(&dom, n)
        };
        if is_link {
            self.follow_hyperlink(cx, n, event);
            return;
        }
        let (input, button, label, summary) = {
            let dom = self.dom();
            (
                is(&dom, n, &local_name!("input")),
                is(&dom, n, &local_name!("button")),
                is(&dom, n, &local_name!("label")),
                is(&dom, n, &local_name!("summary")),
            )
        };
        if input {
            let ty = self.input_type(n);
            if matches!(
                ty.as_deref(),
                Some("color" | "date" | "datetime-local" | "file" | "month" | "time" | "week")
            ) {
                self.show_picker_if_applicable(n);
                return;
            }
            if !self.dom().is_connected(n) {
                return;
            }
            match ty.as_deref() {
                Some("checkbox") | Some("radio") => {
                    self.set_user_validity(n, true);
                    self.fire_event(cx, target, "input", true, false);
                    self.fire_event(cx, target, "change", true, false);
                }
                Some("submit") | Some("image") => {
                    if let Some(form) = self.form_owner_of(n) {
                        self.submit_form(cx, form, Some(n), true);
                    }
                }
                Some("reset") => {
                    if let Some(form) = self.form_owner_of(n) {
                        self.reset_form(cx, form);
                    }
                }
                _ => {}
            }
            // (then the popover target attribute activation behavior)
            if self.dom().is_alive(n) {
                let origin = self.ev(event, |e| e.target).and_then(super::handle_node);
                self.popover_target_activation(cx, n, origin);
            }
        } else if button {
            if is_disabled(&self.dom(), n) {
                return;
            }
            // HTML §4.10.6 the button's activation behavior. (The type state: a button with
            // command or commandfor and no valid type is a Button, in the Auto state.)
            let t = super::forms::button_type_state(&self.dom(), n);
            let explicit = self
                .dom()
                .attr(n, &ns!(), &local_name!("type"))
                .is_some_and(|v| {
                    ["submit", "reset", "button"]
                        .iter()
                        .any(|k| v.eq_ignore_ascii_case(k))
                });
            let handled = match self.form_owner_of(n) {
                Some(f) if t == "submit" => {
                    self.submit_form(cx, f, Some(n), true);
                    true
                }
                Some(f) if t == "reset" => {
                    self.reset_form(cx, f);
                    true
                }
                // (in a form, an Auto-state button with commands is ambiguous: nothing)
                Some(_) => !explicit,
                None => false,
            };
            if !handled {
                let origin = self.ev(event, |e| e.target).and_then(super::handle_node);
                self.button_command(cx, n, origin);
            }
        } else if label {
            // HTML §4.10.4: nothing when the click was on the labeled control (or inside it) or on
            // interactive content inside the label; never re-entered from the click it sends.
            let origin = self.ev(event, |e| e.target).and_then(super::handle_node);
            let control = labeled_control(&self.dom(), n).filter(|&c| {
                let dom = self.dom();
                origin.is_none_or(|o| {
                    !dom.is_inclusive_ancestor_of(c, o)
                        && !dom
                            .inclusive_ancestors(o)
                            .take_while(|&a| a != n)
                            .any(|a| is_interactive_content(&dom, a))
                })
            });
            if self.html.borrow().activating_labels.contains(&n) {
                return;
            }
            if let Some(c) = control {
                self.html.borrow_mut().activating_labels.push(n);
                let view = self.window_of_document(self.dom().node_document(c));
                let e = self.trusted_mouse_event(InterfaceId::PointerEvent, "click", 1, view);
                self.ev_mut(e, |ev| ev.is_trusted = false);
                self.dispatch(cx, e, node_handle(c), false);
                self.html.borrow_mut().activating_labels.retain(|&x| x != n);
            }
        } else if summary {
            self.summary_activation(cx, n);
        } else if self.html.borrow_mut().default_summary_hit.take() == Some(n)
            && self.ev(event, |e| e.is_trusted)
            && self.dom().is_html_element_named(n, &local_name!("details"))
        {
            // The default summary's activation behavior: toggle `open`.
            if self.dom().attr(n, &ns!(), &local_name!("open")).is_some() {
                self.remove_attr(cx, n, "open");
            } else {
                self.set_attr(cx, n, "open", "");
            }
        }
    }
}

impl HTMLElementImpl for Engine {
    fn attach_internals(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.attach_internals_steps(self.n(this))
    }
    fn translate(&self, this: Handle) -> bool {
        let el = self.n(this);
        let dom = self.dom();
        for a in dom.inclusive_ancestors(el).filter(|&a| dom.is_element(a)) {
            if let Some(v) = attr(&dom, a, &LocalName::from("translate")) {
                if v.is_empty() || v.eq_ignore_ascii_case("yes") {
                    return true;
                }
                if v.eq_ignore_ascii_case("no") {
                    return false;
                }
            }
        }
        true
    }
    fn set_translate(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_attr(
            cx,
            self.n(this),
            "translate",
            if value { "yes" } else { "no" },
        );
        Ok(())
    }
    fn hidden(&self, this: Handle) -> Option<BooleanOrUnrestrictedDoubleOrString> {
        let el = self.n(this);
        let dom = self.dom();
        match attr(&dom, el, &local_name!("hidden")) {
            Some(v) if v.eq_ignore_ascii_case("until-found") => Some(
                BooleanOrUnrestrictedDoubleOrString::String("until-found".into()),
            ),
            Some(_) => Some(BooleanOrUnrestrictedDoubleOrString::Boolean(true)),
            None => Some(BooleanOrUnrestrictedDoubleOrString::Boolean(false)),
        }
    }
    fn set_hidden(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<BooleanOrUnrestrictedDoubleOrString>,
    ) -> rt::Result<()> {
        use BooleanOrUnrestrictedDoubleOrString as V;
        let el = self.n(this);
        match value {
            Some(V::String(s)) if s.eq_ignore_ascii_case("until-found") => {
                self.set_attr(cx, el, "hidden", "until-found")
            }
            None | Some(V::Boolean(false)) => self.remove_attr(cx, el, "hidden"),
            Some(V::String(s)) if s.is_empty() => self.remove_attr(cx, el, "hidden"),
            Some(V::UnrestrictedDouble(d)) if d == 0.0 || d.is_nan() => {
                self.remove_attr(cx, el, "hidden")
            }
            _ => self.set_attr(cx, el, "hidden", ""),
        }
        Ok(())
    }
    fn access_key_label(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn draggable(&self, this: Handle) -> bool {
        crate::dnd::is_draggable(&self.dom(), self.n(this))
    }
    fn set_draggable(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_attr(
            cx,
            self.n(this),
            "draggable",
            if value { "true" } else { "false" },
        );
        Ok(())
    }
    fn spellcheck(&self, this: Handle) -> bool {
        let el = self.n(this);
        let dom = self.dom();
        for a in dom.inclusive_ancestors(el).filter(|&a| dom.is_element(a)) {
            if let Some(v) = attr(&dom, a, &LocalName::from("spellcheck")) {
                if v.is_empty() || v.eq_ignore_ascii_case("true") {
                    return true;
                }
                if v.eq_ignore_ascii_case("false") {
                    return false;
                }
            }
        }
        true
    }
    fn set_spellcheck(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_attr(
            cx,
            self.n(this),
            "spellcheck",
            if value { "true" } else { "false" },
        );
        Ok(())
    }
    fn writing_suggestions(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        for a in dom.inclusive_ancestors(el).filter(|&a| dom.is_element(a)) {
            if let Some(v) = attr(&dom, a, &LocalName::from("writingsuggestions")) {
                if v.eq_ignore_ascii_case("false") {
                    return DomStr::Borrowed("false");
                }
                if v.is_empty() || v.eq_ignore_ascii_case("true") {
                    return DomStr::Borrowed("true");
                }
            }
        }
        DomStr::Borrowed("true")
    }
    fn autocapitalize(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        let state = attr(&dom, el, &LocalName::from("autocapitalize")).map(str::to_ascii_lowercase);
        DomStr::Borrowed(match state.as_deref() {
            Some("off") | Some("none") => "none",
            Some("on") | Some("sentences") => "sentences",
            Some("words") => "words",
            Some("characters") => "characters",
            Some(_) => "sentences",
            None => "",
        })
    }
    fn autocorrect(&self, this: Handle) -> bool {
        let el = self.n(this);
        let dom = self.dom();
        !attr(&dom, el, &LocalName::from("autocorrect"))
            .is_some_and(|v| v.eq_ignore_ascii_case("off"))
    }
    fn set_autocorrect(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_attr(
            cx,
            self.n(this),
            "autocorrect",
            if value { "on" } else { "off" },
        );
        Ok(())
    }
    /// HTML §3.2.7 the innerText getter steps: the rendered text, or the descendant text content of
    /// an element that is not being rendered.
    fn inner_text(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        DomStr::Owned(
            self.inner_text_of(el)
                .unwrap_or_else(|| self.dom().descendant_text_content(el)),
        )
    }
    fn set_inner_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        let fragment = self.rendered_text_fragment(el, value);
        self.dom_mut().replace_all(Some(fragment), el);
        self.after_mutation(cx);
        Ok(())
    }
    fn outer_text(&self, this: Handle) -> DomStr<'_> {
        HTMLElementImpl::inner_text(self, this)
    }
    fn set_outer_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        let parent = self.dom().parent(el).ok_or_else(|| {
            exception(
                ExceptionName::NoModificationAllowedError,
                "The element has no parent",
            )
        })?;
        let fragment = self.rendered_text_fragment(el, value);
        let doc = self.dom().node_document(el);
        if self.dom().first_child(fragment).is_none() {
            let t = self.dom_mut().create_text_node(doc, "");
            self.dom_mut().insert(t, fragment, None, true);
        }
        let r = self.dom_mut().replace_child(parent, fragment, el);
        self.after_mutation(cx);
        r.map(drop).map_err(super::dom_err)
    }
    fn scroll_parent(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn offset_parent(&self, this: Handle) -> Option<Handle> {
        self.view_offset_parent(self.n(this))
            .map(super::node_handle)
    }
    fn offset_top(&self, this: Handle) -> i32 {
        self.view_offset(self.n(this)).0
    }
    fn offset_left(&self, this: Handle) -> i32 {
        self.view_offset(self.n(this)).1
    }
    fn offset_width(&self, this: Handle) -> i32 {
        self.view_offset(self.n(this)).2
    }
    fn offset_height(&self, this: Handle) -> i32 {
        self.view_offset(self.n(this)).3
    }
    /// HTML §6.4.3 `click()`.
    fn click(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        if is_disabled(&self.dom(), el) {
            return Ok(());
        }
        if self.html.borrow().clicking.contains(&el) {
            return Ok(());
        }
        self.html.borrow_mut().clicking.push(el);
        let view = self.window_of_document(self.dom().node_document(el));
        let e = self.trusted_mouse_event(InterfaceId::PointerEvent, "click", 1, view);
        self.ev_mut(e, |ev| ev.is_trusted = false);
        self.dispatch(cx, e, this, false);
        self.html.borrow_mut().clicking.retain(|&c| c != el);
        Ok(())
    }
    fn show_popover(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ShowPopoverOptions,
    ) -> rt::Result<()> {
        self.show_popover_method(cx, this, options)
    }
    fn hide_popover(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.hide_popover_steps(cx, self.n(this), true, true, true, None)
    }
    fn toggle_popover(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: TogglePopoverOptionsOrBoolean,
    ) -> rt::Result<bool> {
        self.toggle_popover_method(cx, this, options)
    }
}

impl Engine {
    /// HTML §4.10.6 a button's command steps (its activation behavior once no form action took
    /// it): its `commandfor` element, if any, gets a cancelable `command` event (a CommandEvent with
    /// the command and the button as its source), then, unless canceled or custom (`--…`), the
    /// built-in command: a dialog's `show-modal`, `close` and `request-close` (closing with the
    /// button's value), an HTML element's `toggle-popover`, `show-popover` and `hide-popover`.
    pub(crate) fn button_command(&self, cx: &mut Cx, button: NodeId, event_target: Option<NodeId>) {
        use snowlark_bindings::generated::HTMLButtonElementImpl;
        let h = node_handle(button);
        let Some(target) = HTMLButtonElementImpl::command_for_element(self, h) else {
            // (no commandfor element: the popover target attribute activation behavior)
            self.popover_target_activation(cx, button, event_target);
            return;
        };
        let command = HTMLButtonElementImpl::command(self, h).to_string();
        if command.is_empty() {
            return;
        }
        let target_el = self.n(target);
        let custom = command.starts_with("--");
        let dialog_command = matches!(command.as_str(), "show-modal" | "close" | "request-close");
        let popover_command = command.ends_with("-popover");
        let (is_dialog, is_html) = {
            let dom = self.dom();
            (
                dom.is_html_element_named(target_el, &local_name!("dialog")),
                dom.namespace(target_el) == &ns!(html),
            )
        };
        let valid = (dialog_command && is_dialog) || (popover_command && is_html);
        if !valid && !custom {
            return;
        }
        let e = self.trusted_event(
            InterfaceId::CommandEvent,
            "command",
            false,
            true,
            super::events::EventExt::Command {
                command: command.clone(),
                source: Some(h),
            },
        );
        // (composed: it reaches a shadow host whose shadow root's element is the target)
        self.ev_mut(e, |ev| ev.composed = true);
        if !self.dispatch(cx, e, target, false) || custom || !self.dom().is_alive(target_el) {
            return;
        }
        let has_popover = self.popover_state(target_el).is_some();
        let popover_open = self.popover_showing(target_el);
        if popover_command {
            if !has_popover {
                return;
            }
            let show = match command.as_str() {
                "toggle-popover" => !popover_open,
                "show-popover" if !popover_open => true,
                "hide-popover" if popover_open => false,
                _ => return,
            };
            let _ = if show {
                self.show_popover_steps(cx, target_el, false, Some(button))
            } else {
                self.hide_popover_from(cx, target_el, button);
                Ok(())
            };
            return;
        }
        // (dialog commands are ignored on an open popover)
        if has_popover && popover_open {
            return;
        }
        let open = attr(&self.dom(), target_el, &local_name!("open")).is_some();
        let value = attr(&self.dom(), button, &local_name!("value")).map(str::to_owned);
        match command.as_str() {
            "close" if open => {
                self.close_the_dialog(cx, target_el, value.as_deref(), Some(button));
            }
            "request-close" if open => {
                self.request_close_dialog(cx, target_el, value.as_deref(), Some(button));
            }
            "show-modal" if !open && self.dom().is_connected(target_el) => {
                let _ = self.show_modal_dialog(cx, target_el, Some(button));
            }
            _ => {}
        }
    }

    /// HTML §3.2.7 "rendered text fragment": text nodes with `br` elements for line breaks.
    fn rendered_text_fragment(&self, el: NodeId, input: &str) -> NodeId {
        let doc = self.dom().node_document(el);
        let mut dom = self.dom_mut();
        let fragment = dom.create_document_fragment(doc);
        let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
        for (i, line) in normalized.split('\n').enumerate() {
            if i > 0 {
                let br = dom.create_html_element(doc, local_name!("br"));
                dom.insert(br, fragment, None, true);
            }
            if !line.is_empty() {
                let t = dom.create_text_node(doc, line);
                dom.insert(t, fragment, None, true);
            }
        }
        fragment
    }
}

impl HTMLOrSVGOrMathMLElementImpl for Engine {
    fn dataset(&self, this: Handle) -> Handle {
        self.alloc(Object::StringMap {
            element: self.n(this),
        })
    }
    fn nonce(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.nonce_of(self.n(this)))
    }
    fn set_nonce(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        // (the [[CryptographicNonce]] slot only: the content attribute keeps its value)
        self.hidden_nonces
            .borrow_mut()
            .insert(self.n(this), value.to_owned());
        Ok(())
    }
    fn tab_index(&self, this: Handle) -> i32 {
        let el = self.n(this);
        if let Some(v) = self.dom().attr(el, &ns!(), &local_name!("tabindex")) {
            if let Some(i) = rt::parse_integer(v) {
                if (i32::MIN as i64..=i32::MAX as i64).contains(&i) {
                    return i as i32;
                }
            }
        }
        // (HTML §6.6.3: 0 for a, area, button, frame, iframe, input, object, select, textarea, SVG
        // a, and a summary that is the summary for its parent details; whether a link has an
        // `href` does not matter here)
        let dom = self.dom();
        let default_focusable = is(&dom, el, &local_name!("a"))
            || is(&dom, el, &local_name!("area"))
            || is(&dom, el, &local_name!("button"))
            || is(&dom, el, &local_name!("input"))
            || is(&dom, el, &local_name!("object"))
            || is(&dom, el, &local_name!("select"))
            || is(&dom, el, &local_name!("textarea"))
            || is(&dom, el, &local_name!("iframe"))
            || is(&dom, el, &local_name!("frame"))
            || dom.is_element(el)
                && *dom.namespace(el) == ns!(svg)
                && dom.local_name(el) == &local_name!("a")
            || is(&dom, el, &local_name!("summary"))
                && dom.parent(el).is_some_and(|p| {
                    is(&dom, p, &local_name!("details"))
                        && dom
                            .element_children(p)
                            .find(|&c| is(&dom, c, &local_name!("summary")))
                            == Some(el)
                });
        if default_focusable {
            0
        } else {
            -1
        }
    }
    /// HTML §6.6.6 `focus(options)`: the focusing steps once at a time per element ("locked for
    /// focus"), indicating focus per `focusVisible`, then the focused element scrolled into view
    /// unless `preventScroll`. It is centered only when not already wholly in view, as browsers do
    /// (the spec's "center" would move a visible element). Nor is it scrolled when it already had
    /// the focus before the call, as browsers do: pages that call `focus()` on the focused field
    /// after every re-render would otherwise force a style and layout update each time.
    fn focus(&self, cx: &mut Cx, this: Handle, options: FocusOptions) -> rt::Result<()> {
        let el = self.n(this);
        if self.locked_for_focus.borrow().contains(&el) {
            return Ok(());
        }
        self.locked_for_focus.borrow_mut().push(el);
        let before = if options.prevent_scroll {
            None
        } else {
            let doc = self.dom().node_document(el);
            self.focused_element(doc)
        };
        let outer = self.focus_visible_option.replace(options.focus_visible);
        self.focus_element(cx, el);
        self.focus_visible_option.set(outer);
        self.locked_for_focus.borrow_mut().retain(|&e| e != el);
        if !options.prevent_scroll {
            let doc = self.dom().node_document(el);
            let focused = self.focused_element(doc).filter(|&f| {
                let dom = self.dom();
                Some(f) != before
                    && dom.is_alive(f)
                    && dom.is_shadow_including_inclusive_ancestor_of(el, f)
            });
            if let Some(f) = focused {
                self.view_scroll_into_view(f, "find", "find");
            }
        }
        Ok(())
    }
    fn blur(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.blur_element(cx, self.n(this));
        Ok(())
    }
}

impl ElementContentEditableImpl for Engine {
    fn content_editable(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        DomStr::Borrowed(
            match attr(&dom, el, &LocalName::from("contenteditable"))
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("") | Some("true") => "true",
                Some("false") => "false",
                Some("plaintext-only") => "plaintext-only",
                _ => "inherit",
            },
        )
    }
    fn set_content_editable(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        match value.to_ascii_lowercase().as_str() {
            "inherit" => self.remove_attr(cx, el, "contenteditable"),
            "true" => self.set_attr(cx, el, "contenteditable", "true"),
            "false" => self.set_attr(cx, el, "contenteditable", "false"),
            "plaintext-only" => self.set_attr(cx, el, "contenteditable", "plaintext-only"),
            _ => {
                return Err(exception(
                    ExceptionName::SyntaxError,
                    "Not a valid contentEditable value",
                ))
            }
        }
        Ok(())
    }
    fn is_content_editable(&self, this: Handle) -> bool {
        let el = self.n(this);
        let dom = self.dom();
        if !dom.is_connected(el) {
            return false;
        }
        let doc = dom.node_document(el);
        if self.doc_state(doc, |s| s.design_mode) {
            drop(dom);
            return self.is_editable(el);
        }
        for a in dom.inclusive_ancestors(el).filter(|&a| dom.is_element(a)) {
            match attr(&dom, a, &LocalName::from("contenteditable"))
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("") | Some("true") | Some("plaintext-only") => return true,
                Some("false") => return false,
                _ => {}
            }
        }
        false
    }
    fn virtual_keyboard_policy(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        DomStr::Borrowed(
            match attr(&dom, el, &LocalName::from("virtualkeyboardpolicy"))
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("auto") => "auto",
                Some("manual") => "manual",
                _ => "",
            },
        )
    }
    fn set_virtual_keyboard_policy(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: &str,
    ) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "virtualkeyboardpolicy", value);
        Ok(())
    }
}

impl PopoverTargetAttributesImpl for Engine {
    fn popover_target_element(&self, this: Handle) -> Option<Handle> {
        self.element_ref(self.n(this), "popovertarget")
    }
    fn set_popover_target_element(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<Handle>,
    ) -> rt::Result<()> {
        self.set_element_ref(cx, self.n(this), "popovertarget", value);
        Ok(())
    }
}

impl Engine {
    /// Element reflection (HTML §2.6.1, "reflect" for `Element?`): an explicitly set element if it is
    /// still a descendant of the element's shadow-including ancestors, else the element whose ID is the
    /// attribute.
    pub(crate) fn element_ref(&self, el: NodeId, attr_name: &'static str) -> Option<Handle> {
        let explicit = self
            .html
            .borrow()
            .element_refs
            .get(&(el, attr_name))
            .and_then(|v| v.first().copied());
        let dom = self.dom();
        if let Some(e) = explicit {
            if dom.is_alive(e) && dom.is_shadow_including_inclusive_ancestor_of(dom.root(e), el) {
                return Some(node_handle(e));
            }
            return None;
        }
        let id = attr(&dom, el, &LocalName::from(attr_name))?;
        if id.is_empty() {
            return None;
        }
        let root = dom.root(el);
        dom.descendants(root)
            .find(|&d| dom.is_element(d) && attr(&dom, d, &local_name!("id")) == Some(id))
            .map(node_handle)
    }

    pub(crate) fn set_element_ref(
        &self,
        cx: &mut Cx,
        el: NodeId,
        attr_name: &'static str,
        value: Option<Handle>,
    ) {
        match value {
            None => {
                self.html.borrow_mut().element_refs.remove(&(el, attr_name));
                self.remove_attr(cx, el, attr_name);
            }
            Some(h) => {
                let target = self.n(h);
                self.html
                    .borrow_mut()
                    .element_refs
                    .insert((el, attr_name), vec![target]);
                self.set_attr(cx, el, attr_name, "");
            }
        }
    }
}

macro_rules! aria_refs {
    ($($get:ident, $set:ident, $attr:expr;)*) => {
        impl ARIAMixinImpl for Engine {
            fn aria_active_descendant_element(&self, this: Handle) -> Option<Handle> {
                if self.is_internals(this) {
                    return self
                        .internals_refs(this, "aria-activedescendant")
                        .and_then(|v| v.first().copied())
                        .map(node_handle);
                }
                self.element_ref(self.n(this), "aria-activedescendant")
            }
            fn set_aria_active_descendant_element(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
                if self.is_internals(this) {
                    let v = value.map(|h| vec![self.n(h)]);
                    self.set_internals_refs(this, "aria-activedescendant", v);
                    return Ok(());
                }
                self.set_element_ref(cx, self.n(this), "aria-activedescendant", value);
                Ok(())
            }
            $(
            fn $get(&self, this: Handle) -> Option<Vec<Handle>> {
                if self.is_internals(this) {
                    return self
                        .internals_refs(this, $attr)
                        .map(|v| v.into_iter().map(node_handle).collect());
                }
                let el = self.n(this);
                self.html.borrow().element_refs.get(&(el, $attr)).map(|v| v.iter().copied().map(node_handle).collect())
            }
            fn $set(&self, cx: &mut Cx, this: Handle, value: Option<Vec<Handle>>) -> rt::Result<()> {
                if self.is_internals(this) {
                    let v = value.map(|v| v.into_iter().map(|h| self.n(h)).collect());
                    self.set_internals_refs(this, $attr, v);
                    return Ok(());
                }
                let el = self.n(this);
                match value {
                    None => {
                        self.html.borrow_mut().element_refs.remove(&(el, $attr));
                        self.remove_attr(cx, el, $attr);
                    }
                    Some(v) => {
                        let nodes: Vec<NodeId> = v.into_iter().map(|h| self.n(h)).collect();
                        self.html.borrow_mut().element_refs.insert((el, $attr), nodes);
                        self.set_attr(cx, el, $attr, "");
                    }
                }
                Ok(())
            }
            )*
        }
    };
}

aria_refs! {
    aria_controls_elements, set_aria_controls_elements, "aria-controls";
    aria_described_by_elements, set_aria_described_by_elements, "aria-describedby";
    aria_details_elements, set_aria_details_elements, "aria-details";
    aria_error_message_elements, set_aria_error_message_elements, "aria-errormessage";
    aria_flow_to_elements, set_aria_flow_to_elements, "aria-flowto";
    aria_labelled_by_elements, set_aria_labelled_by_elements, "aria-labelledby";
    aria_owns_elements, set_aria_owns_elements, "aria-owns";
}

impl ARIANotifyMixinImpl for Engine {
    fn aria_notify(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _a: &str,
        _o: AriaNotificationOptions,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl ActivationBlockersMixinImpl for Engine {
    fn is_valid(&self, _this: Handle) -> bool {
        false
    }
    fn invalid_reason(&self, _this: Handle) -> ActivationBlockersMixinBlockerReason {
        ActivationBlockersMixinBlockerReason::Empty
    }
}

impl PowerfulFeatureObserverImpl for Engine {
    fn initial_permission_status(&self, _this: Handle) -> PermissionState {
        PermissionState::Prompt
    }
    fn permission_status(&self, _this: Handle) -> PermissionState {
        PermissionState::Prompt
    }
}

impl RegionImpl for Engine {
    fn region_overset(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("empty")
    }
    fn get_region_flow_ranges(
        &self,
        _cx: &mut Cx,
        _this: Handle,
    ) -> rt::Result<Option<Vec<Handle>>> {
        Ok(None)
    }
}

// ----- simple elements ------------------------------------------------------------------------------------

macro_rules! empty_impls {
    ($($t:path),* $(,)?) => { $( impl $t for Engine {} )* };
}

empty_impls!(
    HTMLBodyElementImpl,
    HTMLBRElementImpl,
    HTMLDListElementImpl,
    HTMLDataElementImpl,
    HTMLDetailsElementImpl,
    HTMLDirectoryElementImpl,
    HTMLDivElementImpl,
    HTMLFontElementImpl,
    HTMLFrameSetElementImpl,
    HTMLHRElementImpl,
    HTMLHeadElementImpl,
    HTMLHeadingElementImpl,
    HTMLHtmlElementImpl,
    HTMLLIElementImpl,
    HTMLLinkElementImpl,
    HTMLMenuElementImpl,
    HTMLMetaElementImpl,
    HTMLModElementImpl,
    HTMLOListElementImpl,
    HTMLParagraphElementImpl,
    HTMLParamElementImpl,
    HTMLPictureElementImpl,
    HTMLPreElementImpl,
    HTMLQuoteElementImpl,
    HTMLSelectedContentElementImpl,
    HTMLSourceElementImpl,
    HTMLSpanElementImpl,
    HTMLTableCaptionElementImpl,
    HTMLTableColElementImpl,
    HTMLTimeElementImpl,
    HTMLUListElementImpl,
    HTMLUnknownElementImpl,
    g::HTMLAreaElementImpl,
    g::HTMLGeolocationElementImpl,
    g::HTMLOptGroupElementImpl,
    g::GlobalEventHandlersImpl,
    g::WindowEventHandlersImpl,
);

impl HTMLTemplateElementImpl for Engine {
    fn content(&self, this: Handle) -> Handle {
        let el = self.n(this);
        node_handle(
            self.dom()
                .template_contents(el)
                .expect("a template's contents"),
        )
    }
    fn shadow_root_slot_assignment(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        DomStr::Borrowed(
            match attr(&dom, el, &LocalName::from("shadowrootslotassignment"))
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("manual") => "manual",
                _ => "named",
            },
        )
    }
    fn set_shadow_root_slot_assignment(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: &str,
    ) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "shadowrootslotassignment", value);
        Ok(())
    }
}

impl HTMLTitleElementImpl for Engine {
    fn text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dom().child_text_content(self.n(this)))
    }
    fn set_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.dom_mut().string_replace_all(value, self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
}

impl HTMLStyleElementImpl for Engine {
    fn disabled(&self, this: Handle) -> bool {
        self.style_element_disabled(this)
    }
    fn set_disabled(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_style_element_disabled(cx, this, value)
    }
}

impl Engine {
    /// HTML §4.2.6 `HTMLStyleElement.disabled` (and SVG 2's `SVGStyleElement.disabled`): the
    /// disabled flag of the element's associated CSS style sheet; false without one.
    pub(crate) fn style_element_disabled(&self, this: Handle) -> bool {
        use snowlark_bindings::generated::{LinkStyleImpl, StyleSheetImpl};
        LinkStyleImpl::sheet(self, this).is_some_and(|s| StyleSheetImpl::disabled(self, s))
    }

    /// Sets the disabled flag of the element's associated CSS style sheet; nothing without one.
    pub(crate) fn set_style_element_disabled(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: bool,
    ) -> rt::Result<()> {
        use snowlark_bindings::generated::{LinkStyleImpl, StyleSheetImpl};
        match LinkStyleImpl::sheet(self, this) {
            Some(s) => StyleSheetImpl::set_disabled(self, cx, s, value),
            None => Ok(()),
        }
    }
}

impl HTMLScriptElementImpl for Engine {
    fn src(&self, this: Handle) -> TrustedScriptURLOrString {
        let el = self.n(this);
        let v = self
            .dom()
            .attr(el, &ns!(), &local_name!("src"))
            .map(str::to_owned);
        TrustedScriptURLOrString::String(match v {
            None => String::new(),
            Some(v) => self.base_url_of(el).join(&v).map(String::from).unwrap_or(v),
        })
    }
    fn set_src(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: TrustedScriptURLOrString,
    ) -> rt::Result<()> {
        let doc = self.dom().node_document(self.n(this));
        let v = self.tt_compliant(
            cx,
            value.into(),
            super::trusted_types::TtKind::ScriptUrl,
            doc,
            "HTMLScriptElement src",
        )?;
        self.set_attr(cx, self.n(this), "src", &v);
        Ok(())
    }
    fn async_(&self, this: Handle) -> bool {
        let el = self.n(this);
        self.script_force_async(el) || self.dom().attr(el, &ns!(), &local_name!("async")).is_some()
    }
    fn set_async(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let el = self.n(this);
        self.set_script_force_async(el, false);
        if value {
            self.set_attr(cx, el, "async", "");
        } else {
            self.remove_attr(cx, el, "async");
        }
        Ok(())
    }
    fn inner_text(&self, this: Handle) -> TrustedScriptOrNullEmptyString {
        TrustedScriptOrNullEmptyString::String(self.dom().descendant_text_content(self.n(this)))
    }
    fn set_inner_text(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: TrustedScriptOrNullEmptyString,
    ) -> rt::Result<()> {
        let doc = self.dom().node_document(self.n(this));
        let v = self.tt_compliant(
            cx,
            value.into(),
            super::trusted_types::TtKind::Script,
            doc,
            "HTMLScriptElement innerText",
        )?;
        HTMLElementImpl::set_inner_text(self, cx, this, &v)
    }
    fn text_content(&self, this: Handle) -> Option<TrustedScriptOrString> {
        Some(TrustedScriptOrString::String(
            self.dom().descendant_text_content(self.n(this)),
        ))
    }
    fn set_text_content(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<TrustedScriptOrString>,
    ) -> rt::Result<()> {
        let doc = self.dom().node_document(self.n(this));
        let v = match value {
            None => String::new(),
            Some(v) => self.tt_compliant(
                cx,
                v.into(),
                super::trusted_types::TtKind::Script,
                doc,
                "HTMLScriptElement textContent",
            )?,
        };
        self.dom_mut().set_text_content(self.n(this), &v);
        self.after_mutation(cx);
        Ok(())
    }
    fn text(&self, this: Handle) -> TrustedScriptOrString {
        TrustedScriptOrString::String(self.dom().child_text_content(self.n(this)))
    }
    fn set_text(&self, cx: &mut Cx, this: Handle, value: TrustedScriptOrString) -> rt::Result<()> {
        let doc = self.dom().node_document(self.n(this));
        let v = self.tt_compliant(
            cx,
            value.into(),
            super::trusted_types::TtKind::Script,
            doc,
            "HTMLScriptElement text",
        )?;
        self.dom_mut().string_replace_all(&v, self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
    fn static_supports(&self, _cx: &mut Cx, type_: &str) -> rt::Result<bool> {
        Ok(matches!(type_, "classic" | "module" | "importmap"))
    }
}

impl Engine {}

impl HTMLAnchorElementImpl for Engine {
    fn text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dom().descendant_text_content(self.n(this)))
    }
    fn set_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.dom_mut().string_replace_all(value, self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
    fn attribution_source_id(&self, _this: Handle) -> u32 {
        0
    }
    fn set_attribution_source_id(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: u32,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl HTMLBaseElementImpl for Engine {
    /// HTML §4.2.3: the `href` attribute parsed against the document's fallback base URL.
    fn href(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let dom = self.dom();
        let doc = dom.node_document(el);
        let fallback = self.fallback_base_url(&dom, doc);
        let href = attr(&dom, el, &local_name!("href")).unwrap_or("");
        DomStr::Owned(match fallback.join(href).ok() {
            Some(u) => u.into(),
            None => href.to_owned(),
        })
    }
}

impl HTMLHyperlinkElementUtilsImpl for Engine {
    fn href(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        match self.hyperlink_url(el) {
            Some(u) => DomStr::Owned(u.into()),
            None => self.attr_str(el, "href").unwrap_or(DomStr::Borrowed("")),
        }
    }
}

impl Engine {
    /// HTML §4.6.10 "reinitialize url": the `href` attribute parsed against the document base URL.
    fn hyperlink_url(&self, el: NodeId) -> Option<url::Url> {
        let href = {
            let dom = self.dom();
            // (SVG 2 §16.2: an SVG `a` element's `href`, else its deprecated `xlink:href`)
            dom.attr(el, &ns!(), &local_name!("href"))
                .or_else(|| {
                    super::svg::is_svg_element(&dom, el)
                        .then(|| dom.attr(el, &ns!(xlink), &local_name!("href")))
                        .flatten()
                })?
                .to_owned()
        };
        self.encoding_parse_url(el, &href)
    }

    /// "Update href": the attribute becomes the URL's serialization (an SVG `a` element's
    /// `xlink:href` when that is the one it has).
    fn update_href(&self, cx: &mut Cx, el: NodeId, url: &url::Url) {
        let xlink_only = {
            let dom = self.dom();
            super::svg::is_svg_element(&dom, el)
                && dom.attr(el, &ns!(), &local_name!("href")).is_none()
                && dom.attr(el, &ns!(xlink), &local_name!("href")).is_some()
        };
        if xlink_only {
            self.dom_mut().set_attribute_value(
                el,
                local_name!("href"),
                ns!(xlink),
                Some("xlink".into()),
                url.as_str(),
            );
            self.after_mutation(cx);
            return;
        }
        self.set_attr(cx, el, "href", url.as_str());
    }

    fn with_hyperlink(&self, cx: &mut Cx, el: NodeId, f: impl FnOnce(&mut url::Url)) {
        if let Some(mut u) = self.hyperlink_url(el) {
            if u.cannot_be_a_base() && false {
                return;
            }
            f(&mut u);
            self.update_href(cx, el, &u);
        }
    }
}

impl HyperlinkElementUtilsImpl for Engine {
    fn origin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_default(),
        )
    }
    fn protocol(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .map(|u| format!("{}:", u.scheme()))
                .unwrap_or_else(|| ":".into()),
        )
    }
    fn set_protocol(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_protocol(u, value));
        Ok(())
    }
    fn username(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .map(|u| u.username().to_owned())
                .unwrap_or_default(),
        )
    }
    fn set_username(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| {
            let _ = u.set_username(value);
        });
        Ok(())
    }
    fn password(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .and_then(|u| u.password().map(str::to_owned))
                .unwrap_or_default(),
        )
    }
    fn set_password(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| {
            let _ = u.set_password(Some(value));
        });
        Ok(())
    }
    fn host(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .map(|u| super::url::host_with_port(&u))
                .unwrap_or_default(),
        )
    }
    fn set_host(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_host(u, value));
        Ok(())
    }
    fn hostname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default(),
        )
    }
    fn set_hostname(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_hostname(u, value));
        Ok(())
    }
    fn port(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .and_then(|u| u.port().map(|p| p.to_string()))
                .unwrap_or_default(),
        )
    }
    fn set_port(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_port(u, value));
        Ok(())
    }
    fn pathname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .map(|u| u.path().to_owned())
                .unwrap_or_default(),
        )
    }
    fn set_pathname(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_pathname(u, value));
        Ok(())
    }
    fn search(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .and_then(|u| u.query().filter(|q| !q.is_empty()).map(|q| format!("?{q}")))
                .unwrap_or_default(),
        )
    }
    fn set_search(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_search(u, value));
        Ok(())
    }
    fn hash(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.hyperlink_url(self.n(this))
                .and_then(|u| {
                    u.fragment()
                        .filter(|f| !f.is_empty())
                        .map(|f| format!("#{f}"))
                })
                .unwrap_or_default(),
        )
    }
    fn set_hash(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.with_hyperlink(cx, self.n(this), |u| super::url::set_hash(u, value));
        Ok(())
    }
}

impl HTMLMapElementImpl for Engine {
    fn areas(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::Areas)
    }
}

impl HTMLDataListElementImpl for Engine {
    fn options(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::DataListOptions)
    }
}

impl HTMLMarqueeElementImpl for Engine {
    fn loop_(&self, this: Handle) -> i32 {
        let el = self.n(this);
        match self
            .dom()
            .attr(el, &ns!(), &LocalName::from("loop"))
            .and_then(rt::parse_integer)
        {
            Some(v) if v == -1 || (v > 0 && v <= i32::MAX as i64) => v as i32,
            _ => -1,
        }
    }
    fn set_loop(&self, cx: &mut Cx, this: Handle, value: i32) -> rt::Result<()> {
        if value == -1 || value > 0 {
            self.set_attr(cx, self.n(this), "loop", &value.to_string());
        }
        Ok(())
    }
    fn start(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn stop(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}

// ----- tables (HTML §4.9) ------------------------------------------------------------------------------

impl Engine {
    fn first_child_named(&self, parent: NodeId, local: &LocalName) -> Option<NodeId> {
        let dom = self.dom();
        dom.children(parent).find(|&c| is(&dom, c, local))
    }

    fn create_html(&self, doc_of: NodeId, local: LocalName) -> NodeId {
        let doc = self.dom().node_document(doc_of);
        self.dom_mut().create_html_element(doc, local)
    }

    fn table_set_child(
        &self,
        cx: &mut Cx,
        table: NodeId,
        local: LocalName,
        value: Option<Handle>,
        position: impl FnOnce(&Dom) -> Option<NodeId>,
    ) -> rt::Result<()> {
        if let Some(v) = value {
            if !self.dom().is_html_element_named(self.n(v), &local) {
                return Err(exception(
                    ExceptionName::HierarchyRequestError,
                    "The element has the wrong type",
                ));
            }
        }
        if let Some(old) = self.first_child_named(table, &local) {
            self.dom_mut().remove_node(old);
        }
        if let Some(v) = value {
            let before = position(&self.dom());
            let r = self.dom_mut().insert_before(table, self.n(v), before);
            r.map_err(super::dom_err)?;
        }
        self.after_mutation(cx);
        Ok(())
    }

    fn insert_row_into(
        &self,
        cx: &mut Cx,
        parent: NodeId,
        rows: Vec<NodeId>,
        index: i32,
    ) -> rt::Result<NodeId> {
        if index < -1 || index > rows.len() as i32 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        }
        let tr = self.create_html(parent, local_name!("tr"));
        let r = if index == -1 || index == rows.len() as i32 {
            self.dom_mut().append_child(parent, tr)
        } else {
            let before = rows[index as usize];
            let p = self.dom().parent(before).unwrap_or(parent);
            self.dom_mut().insert_before(p, tr, Some(before))
        };
        r.map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(tr)
    }

    fn rows_of(&self, table: NodeId, filter: Filter) -> Vec<NodeId> {
        let h = self.collection(table, filter);
        let n = self.coll_len(h);
        (0..n)
            .filter_map(|i| self.coll_item(h, i).and_then(super::handle_node))
            .collect()
    }
}

impl HTMLTableElementImpl for Engine {
    fn caption(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.first_child_named(self.n(this), &local_name!("caption")))
    }
    fn set_caption(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let table = self.n(this);
        self.table_set_child(cx, table, local_name!("caption"), value, |dom| {
            dom.first_child(table)
        })
    }
    fn t_head(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.first_child_named(self.n(this), &local_name!("thead")))
    }
    fn set_t_head(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let table = self.n(this);
        self.table_set_child(cx, table, local_name!("thead"), value, |dom| {
            dom.children(table).find(|&c| {
                dom.is_element(c)
                    && !is(dom, c, &local_name!("caption"))
                    && !is(dom, c, &local_name!("colgroup"))
            })
        })
    }
    fn t_foot(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.first_child_named(self.n(this), &local_name!("tfoot")))
    }
    fn set_t_foot(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let table = self.n(this);
        self.table_set_child(cx, table, local_name!("tfoot"), value, |_| None)
    }
    fn t_bodies(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::TBodies)
    }
    fn rows(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::TableRows)
    }
    fn create_caption(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let table = self.n(this);
        if let Some(c) = self.first_child_named(table, &local_name!("caption")) {
            return Ok(node_handle(c));
        }
        let c = self.create_html(table, local_name!("caption"));
        let first = self.dom().first_child(table);
        self.dom_mut()
            .insert_before(table, c, first)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(node_handle(c))
    }
    fn delete_caption(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if let Some(c) = self.first_child_named(self.n(this), &local_name!("caption")) {
            self.dom_mut().remove_node(c);
            self.after_mutation(cx);
        }
        Ok(())
    }
    fn create_t_head(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let table = self.n(this);
        if let Some(c) = self.first_child_named(table, &local_name!("thead")) {
            return Ok(node_handle(c));
        }
        let h = self.create_html(table, local_name!("thead"));
        let before = {
            let dom = self.dom();
            dom.children(table).find(|&c| {
                dom.is_element(c)
                    && !is(&dom, c, &local_name!("caption"))
                    && !is(&dom, c, &local_name!("colgroup"))
            })
        };
        self.dom_mut()
            .insert_before(table, h, before)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(node_handle(h))
    }
    fn delete_t_head(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if let Some(c) = self.first_child_named(self.n(this), &local_name!("thead")) {
            self.dom_mut().remove_node(c);
            self.after_mutation(cx);
        }
        Ok(())
    }
    fn create_t_foot(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let table = self.n(this);
        if let Some(c) = self.first_child_named(table, &local_name!("tfoot")) {
            return Ok(node_handle(c));
        }
        let f = self.create_html(table, local_name!("tfoot"));
        self.dom_mut()
            .append_child(table, f)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(node_handle(f))
    }
    fn delete_t_foot(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if let Some(c) = self.first_child_named(self.n(this), &local_name!("tfoot")) {
            self.dom_mut().remove_node(c);
            self.after_mutation(cx);
        }
        Ok(())
    }
    fn create_t_body(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let table = self.n(this);
        let b = self.create_html(table, local_name!("tbody"));
        let last_body = {
            let dom = self.dom();
            dom.children(table)
                .filter(|&c| is(&dom, c, &local_name!("tbody")))
                .last()
        };
        let before = last_body.and_then(|l| self.dom().next_sibling(l));
        self.dom_mut()
            .insert_before(table, b, before)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(node_handle(b))
    }
    fn insert_row(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<Handle> {
        let table = self.n(this);
        let rows = self.rows_of(table, Filter::TableRows);
        if index < -1 || index > rows.len() as i32 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        }
        if rows.is_empty() {
            let last_body = {
                let dom = self.dom();
                dom.children(table)
                    .filter(|&c| is(&dom, c, &local_name!("tbody")))
                    .last()
            };
            let parent = match last_body {
                Some(b) => b,
                None => {
                    let b = self.create_html(table, local_name!("tbody"));
                    self.dom_mut()
                        .append_child(table, b)
                        .map_err(super::dom_err)?;
                    b
                }
            };
            let tr = self.create_html(table, local_name!("tr"));
            self.dom_mut()
                .append_child(parent, tr)
                .map_err(super::dom_err)?;
            self.after_mutation(cx);
            return Ok(node_handle(tr));
        }
        if index == -1 || index == rows.len() as i32 {
            let last = *rows.last().unwrap();
            let parent = self.dom().parent(last).unwrap_or(table);
            let tr = self.create_html(table, local_name!("tr"));
            self.dom_mut()
                .append_child(parent, tr)
                .map_err(super::dom_err)?;
            self.after_mutation(cx);
            return Ok(node_handle(tr));
        }
        self.insert_row_into(cx, table, rows, index)
            .map(node_handle)
    }
    fn delete_row(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<()> {
        let rows = self.rows_of(self.n(this), Filter::TableRows);
        let target = if index == -1 {
            rows.last().copied()
        } else if index < 0 || index as usize >= rows.len() {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        } else {
            Some(rows[index as usize])
        };
        if let Some(r) = target {
            self.dom_mut().remove_node(r);
            self.after_mutation(cx);
        }
        Ok(())
    }
}

impl HTMLTableSectionElementImpl for Engine {
    fn rows(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::SectionRows)
    }
    fn insert_row(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<Handle> {
        let s = self.n(this);
        let rows = self.rows_of(s, Filter::SectionRows);
        self.insert_row_into(cx, s, rows, index).map(node_handle)
    }
    fn delete_row(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<()> {
        let rows = self.rows_of(self.n(this), Filter::SectionRows);
        let target = if index == -1 {
            rows.last().copied()
        } else if index < 0 || index as usize >= rows.len() {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        } else {
            Some(rows[index as usize])
        };
        if let Some(r) = target {
            self.dom_mut().remove_node(r);
            self.after_mutation(cx);
        }
        Ok(())
    }
}

impl HTMLTableRowElementImpl for Engine {
    fn row_index(&self, this: Handle) -> i32 {
        let tr = self.n(this);
        let table = {
            let dom = self.dom();
            let p = dom.parent(tr);
            match p {
                Some(p) if is(&dom, p, &local_name!("table")) => Some(p),
                Some(p)
                    if is(&dom, p, &local_name!("tbody"))
                        || is(&dom, p, &local_name!("thead"))
                        || is(&dom, p, &local_name!("tfoot")) =>
                {
                    dom.parent(p)
                        .filter(|&t| is(&dom, t, &local_name!("table")))
                }
                _ => None,
            }
        };
        let Some(table) = table else {
            return -1;
        };
        self.rows_of(table, Filter::TableRows)
            .iter()
            .position(|&r| r == tr)
            .map_or(-1, |i| i as i32)
    }
    fn section_row_index(&self, this: Handle) -> i32 {
        let tr = self.n(this);
        let Some(parent) = self.dom().parent(tr) else {
            return -1;
        };
        // The parent's `rows`: a table's (its own and its sections' rows) or a section's.
        let filter = if self
            .dom()
            .is_html_element_named(parent, &local_name!("table"))
        {
            Filter::TableRows
        } else {
            Filter::SectionRows
        };
        self.rows_of(parent, filter)
            .iter()
            .position(|&r| r == tr)
            .map_or(-1, |i| i as i32)
    }
    fn cells(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::Cells)
    }
    fn insert_cell(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<Handle> {
        let tr = self.n(this);
        let cells = self.rows_of(tr, Filter::Cells);
        if index < -1 || index > cells.len() as i32 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        }
        let td = self.create_html(tr, local_name!("td"));
        let before = if index == -1 || index as usize == cells.len() {
            None
        } else {
            Some(cells[index as usize])
        };
        self.dom_mut()
            .insert_before(tr, td, before)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        Ok(node_handle(td))
    }
    fn delete_cell(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<()> {
        let cells = self.rows_of(self.n(this), Filter::Cells);
        let target = if index == -1 {
            cells.last().copied()
        } else if index < 0 || index as usize >= cells.len() {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The index is out of range",
            ));
        } else {
            Some(cells[index as usize])
        };
        if let Some(c) = target {
            self.dom_mut().remove_node(c);
            self.after_mutation(cx);
        }
        Ok(())
    }
}

impl HTMLTableCellElementImpl for Engine {
    fn cell_index(&self, this: Handle) -> i32 {
        let td = self.n(this);
        let Some(tr) = self.dom().parent(td) else {
            return -1;
        };
        if !self.dom().is_html_element_named(tr, &local_name!("tr")) {
            return -1;
        }
        self.rows_of(tr, Filter::Cells)
            .iter()
            .position(|&c| c == td)
            .map_or(-1, |i| i as i32)
    }
}
