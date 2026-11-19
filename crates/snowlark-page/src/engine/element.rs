// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Element` (DOM §4.9) and the parts of it other specifications add (DOM Parsing, CSSOM View, Pointer
//! Events). Geometry members answer through the layout hook, which returns zeros until layout exists.

use super::view::{finite, logical};
use snowlark_bindings::generated::{
    BooleanOrScrollIntoViewOptions, CheckVisibilityOptions, ElementImpl, FocusableAreasOption,
    GetHTMLOptions, PointerLockOptions, ScrollBehavior, ScrollResult, ScrollToOptions,
    SetHTMLOptions, ShadowRootInit as ShadowRootInitIdl, ShadowRootMode, SlotAssignmentMode,
    SpatialNavigationDirection, SpatialNavigationSearchOptions, TrustedHTMLOrNullEmptyString,
    TrustedHTMLOrString, TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise};
use snowlark_dom::{local_name, ns, AdjacentPosition, LocalName, NodeId, SerializeOptions};

use super::collections::Filter;
use super::sanitizer::SanitizerInput;
use super::{exception, handle_node, node_handle, DomResultExt, Engine, Object};

/// A promise already resolved with undefined.
pub(crate) fn resolved(cx: &mut Cx) -> rt::Result<JsPromise> {
    let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
    p.resolve(cx, ());
    Ok(p)
}

/// The promise a scroll method returns: `pending` still holds it when no smooth scroll took it,
/// and then the scroll is complete already (CSSOM View: resolved with `interrupted` false).
pub(crate) fn scroll_promise(cx: &mut Cx, p: JsPromise, pending: Option<JsPromise>) -> JsPromise {
    if pending.is_some() {
        p.resolve(cx, ScrollResult { interrupted: false });
    }
    p
}

/// `Element.scroll()`/`scrollTo()`/`scrollBy()` with a behavior: the scroll and its promise.
fn element_scroll(
    e: &Engine,
    cx: &mut Cx,
    el: NodeId,
    x: Option<f64>,
    y: Option<f64>,
    behavior: ScrollBehavior,
) -> rt::Result<JsPromise> {
    let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
    let mut pending = Some(p.clone());
    e.view_scroll_element_with(cx, el, x, y, behavior, &mut pending);
    Ok(scroll_promise(cx, p, pending))
}

/// A promise already rejected with a DOMException.
pub(crate) fn rejected(
    cx: &mut Cx,
    name: ExceptionName,
    message: &'static str,
) -> rt::Result<JsPromise> {
    let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
    p.reject(cx, exception(name, message));
    Ok(p)
}

impl Engine {
    pub(crate) fn tag_collection(&self, root: NodeId, qualified: &str) -> Handle {
        self.collection(
            root,
            Filter::TagName {
                qualified: qualified.to_owned(),
                lower: qualified.to_ascii_lowercase(),
            },
        )
    }

    pub(crate) fn tag_ns_collection(
        &self,
        root: NodeId,
        namespace: Option<&str>,
        local: &str,
    ) -> Handle {
        let namespace = match namespace {
            Some("*") => None,
            other => Some(snowlark_dom::Dom::namespace_atom(other)),
        };
        let local = (local != "*").then(|| LocalName::from(local));
        self.collection(root, Filter::TagNameNs { namespace, local })
    }

    pub(crate) fn class_collection(&self, root: NodeId, classes: &str) -> Handle {
        let classes = super::attrs::ordered_set(classes)
            .into_iter()
            .map(str::to_owned)
            .collect();
        self.collection(root, Filter::ClassNames(classes))
    }

    /// The `innerHTML` setter (HTML §13.3): fragment parsing, then replace all.
    pub(crate) fn set_inner_html_steps(
        &self,
        cx: &mut Cx,
        node: NodeId,
        markup: &str,
    ) -> rt::Result<()> {
        self.parsed_insertion(cx, |dom| dom.set_inner_html(node, markup))
            .js()
    }

    fn serialize(&self, node: NodeId, options: GetHTMLOptions) -> String {
        let roots: Vec<NodeId> = options
            .shadow_roots
            .iter()
            .filter_map(|h| handle_node(*h))
            .collect();
        self.dom().get_html(
            node,
            SerializeOptions {
                serializable_shadow_roots: options.serializable_shadow_roots,
                shadow_roots: &roots,
                scripting_enabled: true,
            },
        )
    }

    fn insert_adjacent(
        &self,
        cx: &mut Cx,
        element: NodeId,
        where_: &str,
        node: NodeId,
    ) -> rt::Result<Option<NodeId>> {
        let position = AdjacentPosition::parse(where_).js()?;
        let r = {
            let mut dom = self.dom_mut();
            match position {
                AdjacentPosition::BeforeBegin => match dom.parent(element) {
                    None => Ok(None),
                    Some(p) => dom.insert_before(p, node, Some(element)).map(Some),
                },
                AdjacentPosition::AfterBegin => {
                    let first = dom.first_child(element);
                    dom.insert_before(element, node, first).map(Some)
                }
                AdjacentPosition::BeforeEnd => dom.append_child(element, node).map(Some),
                AdjacentPosition::AfterEnd => match dom.parent(element) {
                    None => Ok(None),
                    Some(p) => {
                        let next = dom.next_sibling(element);
                        dom.insert_before(p, node, next).map(Some)
                    }
                },
            }
        }
        .js();
        self.after_mutation(cx);
        r
    }
}

impl ElementImpl for Engine {
    fn start_view_transition(
        &self,
        cx: &mut Cx,
        this: Handle,
        callback_options: snowlark_bindings::generated::ViewTransitionUpdateCallbackOrStartViewTransitionOptions,
    ) -> rt::Result<Handle> {
        Engine::start_view_transition(self, cx, self.n(this), callback_options)
    }
    fn active_view_transition(&self, this: Handle) -> Option<Handle> {
        Engine::active_view_transition(self, self.n(this))
    }
    fn namespace_uri(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        if dom.namespace(id).is_empty() {
            return None;
        }
        Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
            &**d.namespace(id)
        })))
    }

    fn prefix(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        dom.prefix(id)?;
        Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
            d.prefix(id).map_or("", |p| &**p)
        })))
    }

    fn local_name(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.local_name(id))
    }

    fn tag_name(&self, this: Handle) -> DomStr<'_> {
        self.element_tag_name(self.n(this))
    }

    fn id(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "id")
            .unwrap_or(DomStr::Borrowed(""))
    }

    fn set_id(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "id", value);
        Ok(())
    }

    fn class_name(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "class")
            .unwrap_or(DomStr::Borrowed(""))
    }

    fn set_class_name(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "class", value);
        Ok(())
    }

    fn class_list(&self, this: Handle) -> Handle {
        self.alloc(Object::TokenList {
            element: self.n(this),
            attr: local_name!("class"),
        })
    }

    fn slot(&self, this: Handle) -> DomStr<'_> {
        self.attr_str(self.n(this), "slot")
            .unwrap_or(DomStr::Borrowed(""))
    }

    fn set_slot(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "slot", value);
        Ok(())
    }

    fn attributes(&self, this: Handle) -> Handle {
        self.alloc(Object::AttrMap {
            element: self.n(this),
        })
    }

    fn shadow_root(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().open_shadow_root(self.n(this)))
    }

    fn custom_element_registry(&self, this: Handle) -> Option<Handle> {
        let doc = self.dom().node_document(self.n(this));
        self.window_of_document(doc)
            .map(|w| self.registry_handle(w))
    }

    fn part(&self, this: Handle) -> Handle {
        self.alloc(Object::TokenList {
            element: self.n(this),
            attr: LocalName::from("part"),
        })
    }

    fn scroll_top(&self, this: Handle) -> f64 {
        self.view_scroll_position(self.n(this)).y as f64
    }
    fn set_scroll_top(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        // ("scroll an element" with behavior `auto`: the element's `scroll-behavior` decides)
        self.view_scroll_element_with(
            cx,
            self.n(this),
            None,
            Some(finite(value)),
            ScrollBehavior::Auto,
            &mut None,
        );
        Ok(())
    }
    fn scroll_left(&self, this: Handle) -> f64 {
        self.view_scroll_position(self.n(this)).x as f64
    }
    fn set_scroll_left(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.view_scroll_element_with(
            cx,
            self.n(this),
            Some(finite(value)),
            None,
            ScrollBehavior::Auto,
            &mut None,
        );
        Ok(())
    }
    fn scroll_width(&self, this: Handle) -> i32 {
        self.view_scroll_size(self.n(this)).0
    }
    fn scroll_height(&self, this: Handle) -> i32 {
        self.view_scroll_size(self.n(this)).1
    }
    fn client_top(&self, this: Handle) -> i32 {
        self.view_client(self.n(this)).0
    }
    fn client_left(&self, this: Handle) -> i32 {
        self.view_client(self.n(this)).1
    }
    fn client_width(&self, this: Handle) -> i32 {
        self.view_client(self.n(this)).2
    }
    fn client_height(&self, this: Handle) -> i32 {
        self.view_client(self.n(this)).3
    }
    fn current_css_zoom(&self, _this: Handle) -> f64 {
        1.0
    }

    fn inner_html(&self, this: Handle) -> TrustedHTMLOrNullEmptyString {
        TrustedHTMLOrNullEmptyString::String(self.fragment_serialize(self.n(this), false))
    }

    fn set_inner_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: TrustedHTMLOrNullEmptyString,
    ) -> rt::Result<()> {
        let markup = self.tt_html(cx, value, self.n(this), "Element innerHTML")?;
        self.set_inner_html_steps(cx, self.n(this), &markup)
    }

    fn outer_html(&self, this: Handle) -> TrustedHTMLOrNullEmptyString {
        TrustedHTMLOrNullEmptyString::String(self.fragment_serialize(self.n(this), true))
    }

    fn set_outer_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: TrustedHTMLOrNullEmptyString,
    ) -> rt::Result<()> {
        let markup = self.tt_html(cx, value, self.n(this), "Element outerHTML")?;
        let node = self.n(this);
        self.parsed_insertion(cx, |dom| dom.set_outer_html(node, &markup))
            .js()
    }

    fn has_attributes(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.dom().has_attributes(self.n(this)))
    }

    fn get_attribute_names(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<String>> {
        Ok(self
            .dom()
            .get_attribute_names(self.n(this))
            .into_iter()
            .map(|n| n.into_owned())
            .collect())
    }

    fn get_attribute(
        &self,
        _cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let id = self.n(this);
        let dom = self.dom();
        if dom.get_attribute(id, qualified_name).is_none() {
            return Ok(None);
        }
        Ok(Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
            d.get_attribute(id, qualified_name).unwrap_or("")
        }))))
    }

    fn get_attribute_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let id = self.n(this);
        let dom = self.dom();
        if dom.get_attribute_ns(id, namespace, local_name).is_none() {
            return Ok(None);
        }
        Ok(Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
            d.get_attribute_ns(id, namespace, local_name).unwrap_or("")
        }))))
    }

    fn set_attribute(
        &self,
        cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
        value: TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString,
    ) -> rt::Result<()> {
        // (an HTML element's attribute names are lowercased first, as setAttribute does)
        let lowered = if self.dom().namespace(self.n(this)) == &snowlark_dom::ns!(html) {
            qualified_name.to_ascii_lowercase()
        } else {
            qualified_name.to_owned()
        };
        let value = self.tt_attribute(
            cx,
            self.n(this),
            "",
            &lowered,
            value,
            "Element setAttribute",
        )?;
        let r = self
            .dom_mut()
            .set_attribute(self.n(this), qualified_name, &value)
            .js();
        self.after_mutation(cx);
        r
    }

    fn set_attribute_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        qualified_name: &str,
        value: TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString,
    ) -> rt::Result<()> {
        let local = qualified_name
            .split_once(':')
            .map_or(qualified_name, |(_, l)| l);
        let value = self.tt_attribute(
            cx,
            self.n(this),
            namespace.unwrap_or(""),
            local,
            value,
            "Element setAttributeNS",
        )?;
        let r = self
            .dom_mut()
            .set_attribute_ns(self.n(this), namespace, qualified_name, &value)
            .js();
        self.after_mutation(cx);
        r
    }

    fn remove_attribute(&self, cx: &mut Cx, this: Handle, qualified_name: &str) -> rt::Result<()> {
        self.dom_mut()
            .remove_attribute(self.n(this), qualified_name);
        self.after_mutation(cx);
        Ok(())
    }

    fn remove_attribute_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<()> {
        self.dom_mut()
            .remove_attribute_ns(self.n(this), namespace, local_name);
        self.after_mutation(cx);
        Ok(())
    }

    fn toggle_attribute(
        &self,
        cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
        force: Option<bool>,
    ) -> rt::Result<bool> {
        let r = self
            .dom_mut()
            .toggle_attribute(self.n(this), qualified_name, force)
            .js();
        self.after_mutation(cx);
        r
    }

    fn has_attribute(&self, _cx: &mut Cx, this: Handle, qualified_name: &str) -> rt::Result<bool> {
        Ok(self.dom().has_attribute(self.n(this), qualified_name))
    }

    fn has_attribute_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<bool> {
        Ok(self
            .dom()
            .has_attribute_ns(self.n(this), namespace, local_name))
    }

    fn get_attribute_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
    ) -> rt::Result<Option<Handle>> {
        Ok(self.opt_node(
            self.dom_mut()
                .get_attribute_node(self.n(this), qualified_name),
        ))
    }

    fn get_attribute_node_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Option<Handle>> {
        Ok(self.opt_node(
            self.dom_mut()
                .get_attribute_node_ns(self.n(this), namespace, local_name),
        ))
    }

    fn set_attribute_node(
        &self,
        cx: &mut Cx,
        this: Handle,
        attr: Handle,
    ) -> rt::Result<Option<Handle>> {
        let r = self
            .dom_mut()
            .set_attribute_node(self.n(this), self.n(attr))
            .js();
        self.after_mutation(cx);
        r.map(|o| o.map(node_handle))
    }

    fn set_attribute_node_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        attr: Handle,
    ) -> rt::Result<Option<Handle>> {
        self.set_attribute_node(cx, this, attr)
    }

    fn remove_attribute_node(&self, cx: &mut Cx, this: Handle, attr: Handle) -> rt::Result<Handle> {
        let r = self
            .dom_mut()
            .remove_attribute_node(self.n(this), self.n(attr))
            .js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn attach_shadow(
        &self,
        cx: &mut Cx,
        this: Handle,
        init: ShadowRootInitIdl,
    ) -> rt::Result<Handle> {
        let element = self.n(this);
        // DOM §4.9 "attach a shadow root" step 3: a custom element whose definition disables shadow.
        if self.shadow_disabled(element) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The custom element definition disables shadow roots",
            ));
        }
        let init = snowlark_dom::ShadowRootInit {
            mode: match init.mode {
                ShadowRootMode::Open => snowlark_dom::ShadowRootMode::Open,
                ShadowRootMode::Closed => snowlark_dom::ShadowRootMode::Closed,
            },
            delegates_focus: init.delegates_focus,
            slot_assignment: match init.slot_assignment {
                SlotAssignmentMode::Named => snowlark_dom::SlotAssignmentMode::Named,
                SlotAssignmentMode::Manual => snowlark_dom::SlotAssignmentMode::Manual,
            },
            clonable: init.clonable,
            serializable: init.serializable,
        };
        let r = self.dom_mut().attach_shadow(element, init).js();
        if r.is_ok() {
            self.render_shadow_attached(element);
        }
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn closest(&self, cx: &mut Cx, this: Handle, selectors: &str) -> rt::Result<Option<Handle>> {
        let parsed = self.parsed_selectors(selectors)?;
        self.sync_form_states_if(cx, parsed.reads_form_state);
        let el = self.n(this);
        let found = {
            let dom = self.dom();
            // (an element's inclusive ancestor elements are in its tree)
            self.first_matching_cached(&dom, el, dom.inclusive_ancestors(el), &parsed, Some(el))
        };
        Ok(self.opt_node(found))
    }

    fn matches(&self, cx: &mut Cx, this: Handle, selectors: &str) -> rt::Result<bool> {
        let parsed = self.parsed_selectors(selectors)?;
        self.sync_form_states_if(cx, parsed.reads_form_state);
        let el = self.n(this);
        let dom = self.dom();
        Ok(self.matches_cached(&dom, el, &parsed, Some(el)))
    }

    fn webkit_matches_selector(
        &self,
        cx: &mut Cx,
        this: Handle,
        selectors: &str,
    ) -> rt::Result<bool> {
        self.matches(cx, this, selectors)
    }

    fn get_elements_by_tag_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
    ) -> rt::Result<Handle> {
        Ok(self.tag_collection(self.n(this), qualified_name))
    }

    fn get_elements_by_tag_name_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Handle> {
        Ok(self.tag_ns_collection(self.n(this), namespace, local_name))
    }

    fn get_elements_by_class_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        class_names: &str,
    ) -> rt::Result<Handle> {
        Ok(self.class_collection(self.n(this), class_names))
    }

    fn insert_adjacent_element(
        &self,
        cx: &mut Cx,
        this: Handle,
        where_: &str,
        element: Handle,
    ) -> rt::Result<Option<Handle>> {
        self.insert_adjacent(cx, self.n(this), where_, self.n(element))
            .map(|o| o.map(node_handle))
    }

    fn insert_adjacent_text(
        &self,
        cx: &mut Cx,
        this: Handle,
        where_: &str,
        data: &str,
    ) -> rt::Result<()> {
        let el = self.n(this);
        let doc = self.dom().node_document(el);
        let text = self.dom_mut().create_text_node(doc, data);
        self.insert_adjacent(cx, el, where_, text).map(drop)
    }

    fn get_spatial_navigation_container(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let doc = self.dom().node_document(self.n(this));
        Ok(node_handle(self.dom().document_element(doc).unwrap_or(doc)))
    }

    fn focusable_areas(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _option: FocusableAreasOption,
    ) -> rt::Result<Vec<Handle>> {
        Ok(Vec::new())
    }

    fn spatial_navigation_search(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _dir: SpatialNavigationDirection,
        _options: SpatialNavigationSearchOptions,
    ) -> rt::Result<Option<Handle>> {
        Ok(None)
    }

    fn get_client_rects(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let rects = self
            .view_client_rects(self.n(this))
            .into_iter()
            .map(|r| self.alloc(Object::DomRect(r)))
            .collect();
        Ok(self.alloc(Object::DomRectList(rects)))
    }

    fn get_bounding_client_rect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.view_bounding_rect(self.n(this));
        Ok(self.alloc(Object::DomRect(r)))
    }

    fn check_visibility(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _options: CheckVisibilityOptions,
    ) -> rt::Result<bool> {
        Ok(self.dom().is_connected(self.n(this)))
    }

    /// CSSOM View §6.1 `scrollIntoView(arg)`.
    fn scroll_into_view(
        &self,
        cx: &mut Cx,
        this: Handle,
        arg: BooleanOrScrollIntoViewOptions,
    ) -> rt::Result<JsPromise> {
        let (block, inline, behavior) = match arg {
            BooleanOrScrollIntoViewOptions::Boolean(true) => {
                ("start", "nearest", ScrollBehavior::Auto)
            }
            BooleanOrScrollIntoViewOptions::Boolean(false) => {
                ("end", "nearest", ScrollBehavior::Auto)
            }
            BooleanOrScrollIntoViewOptions::ScrollIntoViewOptions(o) => {
                (logical(o.block), logical(o.inline), o.behavior)
            }
        };
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let mut pending = Some(p.clone());
        self.view_scroll_into_view_with(cx, self.n(this), block, inline, behavior, &mut pending);
        Ok(scroll_promise(cx, p, pending))
    }

    fn scroll_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        let el = self.n(this);
        element_scroll(
            self,
            cx,
            el,
            options.left.map(finite),
            options.top.map(finite),
            options.behavior,
        )
    }

    fn scroll_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        let el = self.n(this);
        element_scroll(
            self,
            cx,
            el,
            Some(finite(x)),
            Some(finite(y)),
            ScrollBehavior::Auto,
        )
    }

    fn scroll_to_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        ElementImpl::scroll_options(self, cx, this, options)
    }

    fn scroll_to_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        ElementImpl::scroll_x_y(self, cx, this, x, y)
    }

    fn scroll_by_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        let el = self.n(this);
        let at = self.view_scroll_position(el);
        element_scroll(
            self,
            cx,
            el,
            Some(at.x as f64 + options.left.map_or(0.0, finite)),
            Some(at.y as f64 + options.top.map_or(0.0, finite)),
            options.behavior,
        )
    }

    fn scroll_by_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        let el = self.n(this);
        let at = self.view_scroll_position(el);
        element_scroll(
            self,
            cx,
            el,
            Some(at.x as f64 + finite(x)),
            Some(at.y as f64 + finite(y)),
            ScrollBehavior::Auto,
        )
    }

    fn get_html(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: GetHTMLOptions,
    ) -> rt::Result<DomStr<'_>> {
        Ok(DomStr::Owned(self.serialize(self.n(this), options)))
    }

    fn insert_adjacent_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        position: &str,
        string: TrustedHTMLOrString,
    ) -> rt::Result<()> {
        let markup = self.tt_html(cx, string, self.n(this), "Element insertAdjacentHTML")?;
        let position = AdjacentPosition::parse(position).js()?;
        let node = self.n(this);
        self.parsed_insertion(cx, |dom| dom.insert_adjacent_html(node, position, &markup))
            .js()
    }

    fn request_fullscreen(
        &self,
        cx: &mut Cx,
        this: Handle,
        _options: snowlark_bindings::generated::FullscreenOptions,
    ) -> rt::Result<JsPromise> {
        self.request_fullscreen(cx, self.n(this))
    }
    fn set_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.set_html_steps(cx, self.n(this), html, InsertMode::safe(_o))
    }
    fn append_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 4)
    }
    fn prepend_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 3)
    }
    fn set_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Element setHTMLUnsafe")?;
        self.set_html_steps(cx, self.n(this), &m, mode)
    }
    fn append_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Element appendHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 4)
    }
    fn prepend_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Element prependHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 3)
    }

    /// Pointer Events 3 `setPointerCapture`: the mouse (pointer 1) is the only active pointer.
    fn set_pointer_capture(&self, _cx: &mut Cx, this: Handle, pointer_id: i32) -> rt::Result<()> {
        if pointer_id != 1 {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No active pointer with that id",
            ));
        }
        if !self.dom().is_connected(self.n(this)) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The element is not connected",
            ));
        }
        self.set_pointer_capture_target(self.n(this));
        Ok(())
    }

    fn release_pointer_capture(
        &self,
        _cx: &mut Cx,
        this: Handle,
        pointer_id: i32,
    ) -> rt::Result<()> {
        if pointer_id != 1 {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No active pointer with that id",
            ));
        }
        self.release_pointer_capture_target(self.n(this));
        Ok(())
    }

    fn has_pointer_capture(&self, _cx: &mut Cx, this: Handle, pointer_id: i32) -> rt::Result<bool> {
        Ok(pointer_id == 1 && self.has_pointer_capture_target(self.n(this)))
    }

    fn request_pointer_lock(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _options: PointerLockOptions,
    ) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotSupportedError,
            "Pointer lock is not available",
        )
    }
}

impl Engine {
    /// `Element.tagName` without allocating for the common case: HTML elements in an HTML document
    /// with a known name map to a static uppercase string.
    pub(crate) fn element_tag_name(&self, id: NodeId) -> DomStr<'_> {
        let dom = self.dom();
        if dom.is_html_element_in_html_document(id)
            && dom.element(id).is_some_and(|e| e.name().prefix.is_none())
        {
            if let Some(s) = super::html::upper_tag(dom.local_name(id)) {
                return DomStr::Borrowed(s);
            }
        }
        match dom.tag_name(id) {
            std::borrow::Cow::Owned(s) => DomStr::Owned(s),
            std::borrow::Cow::Borrowed(_) => {
                DomStr::Guard(std::cell::Ref::map(dom, |d| match d.element(id) {
                    Some(e) if e.name().prefix.is_none() => &*e.name().local,
                    _ => "",
                }))
            }
        }
    }
}

impl Engine {
    /// The fragment serializing algorithm (HTML §13.3 / DOM Parsing): HTML serialization in HTML
    /// documents, XML serialization (well-formed when it can be) in XML documents. `outer` includes
    /// the node itself.
    pub(crate) fn fragment_serialize(&self, node: NodeId, outer: bool) -> String {
        let dom = self.dom();
        if dom.is_html_document(dom.node_document(node)) {
            return if outer {
                dom.outer_html(node)
            } else {
                dom.inner_html(node)
            };
        }
        let r = if outer {
            dom.serialize_xml(node, true)
                .or_else(|_| dom.serialize_xml(node, false))
        } else {
            dom.serialize_xml_children(node, true)
                .or_else(|_| dom.serialize_xml_children(node, false))
        };
        r.unwrap_or_default()
    }

    /// HTML "parse HTML for insertion" and "the fragment parsing algorithm steps": parses `markup`
    /// with `target`'s parser context element (the target, or a shadow root's or template
    /// contents' host), with declarative shadow roots, and sanitizes the result with the
    /// configuration the options give ("get a sanitizer config from options": the safe methods
    /// always sanitize and remove what is unsafe). `None` when there is nothing to insert (a safe
    /// parse into a script).
    pub(crate) fn parse_for_insertion(
        &self,
        target: NodeId,
        markup: &str,
        mode: InsertMode,
    ) -> rt::Result<Option<NodeId>> {
        let context = {
            let dom = self.dom();
            if dom.is_element(target) {
                target
            } else {
                dom.host(target).unwrap_or(target)
            }
        };
        let (safe, run_scripts, input) = match mode {
            InsertMode::Safe(input) => (true, false, input),
            InsertMode::Unsafe { run_scripts, input } => (false, run_scripts, input),
        };
        if safe {
            let dom = self.dom();
            let ns = dom.namespace(context);
            if &**dom.local_name(context) == "script" && (*ns == ns!(html) || *ns == ns!(svg)) {
                return Ok(None);
            }
        }
        let configuration = self.sanitizer_config_from_options(input, safe)?;
        let fragment = if run_scripts {
            self.dom_mut()
                .parse_fragment_running_scripts(context, markup, true)
        } else {
            self.dom_mut().parse_fragment(
                context,
                markup,
                true,
                snowlark_dom::ParseOptions::default(),
            )
        };
        if let Some(c) = &configuration {
            self.sanitize_tree(fragment, c);
        }
        Ok(Some(fragment))
    }

    /// HTML "target for HTML insertion": a `template`'s contents, or the node itself (an element or
    /// a shadow root).
    fn html_insertion_target(&self, node: NodeId) -> NodeId {
        self.dom().template_contents(node).unwrap_or(node)
    }

    /// HTML "parent for HTML insertion" of `node`: `None` when it has no parent.
    fn html_insertion_parent(&self, node: NodeId) -> rt::Result<Option<NodeId>> {
        let dom = self.dom();
        let Some(parent) = dom.parent(node) else {
            return Ok(None);
        };
        let template = dom.is_html_element_named(parent, &local_name!("template"));
        let fragment_with_host = dom.is_document_fragment(parent) && dom.host(parent).is_some();
        if !template && (dom.is_element(parent) || fragment_with_host) {
            return Ok(Some(parent));
        }
        Err(exception(
            ExceptionName::HierarchyRequestError,
            "The node's parent cannot take parsed HTML",
        ))
    }

    /// HTML "filter and set HTML" on `node`'s target for HTML insertion (`setHTML` and
    /// `setHTMLUnsafe` of an Element or a ShadowRoot).
    pub(crate) fn set_html_steps(
        &self,
        cx: &mut Cx,
        node: NodeId,
        markup: &str,
        mode: InsertMode,
    ) -> rt::Result<()> {
        let target = self.html_insertion_target(node);
        if let Some(fragment) = self.parse_for_insertion(target, markup, mode)? {
            self.dom_mut().replace_all(Some(fragment), target);
            self.after_mutation(cx);
        }
        Ok(())
    }

    /// The HTML insertion methods that insert relative to `node`: 0 before, 1 after, 2 replacing
    /// it (from its parent for HTML insertion), 3 prepending, 4 appending (to its target for HTML
    /// insertion): HTML "filter and pre-insert HTML" and "filter and replace with HTML".
    pub(crate) fn insert_html_steps(
        &self,
        cx: &mut Cx,
        node: NodeId,
        markup: &str,
        mode: InsertMode,
        where_: u8,
    ) -> rt::Result<()> {
        let (parent, child) = match where_ {
            3 | 4 => {
                let target = self.html_insertion_target(node);
                let first = (where_ == 3)
                    .then(|| self.dom().first_child(target))
                    .flatten();
                (target, first)
            }
            _ => {
                let Some(parent) = self.html_insertion_parent(node)? else {
                    return Ok(());
                };
                let child = match where_ {
                    0 | 2 => Some(node),
                    _ => self.dom().next_sibling(node),
                };
                (parent, child)
            }
        };
        if self
            .dom()
            .is_html_element_named(parent, &local_name!("html"))
        {
            return Err(exception(
                ExceptionName::HierarchyRequestError,
                "Parsed HTML cannot go into an html element",
            ));
        }
        let Some(fragment) = self.parse_for_insertion(parent, markup, mode)? else {
            return Ok(());
        };
        let r = if where_ == 2 {
            self.dom_mut()
                .replace_child(parent, fragment, node)
                .map(drop)
        } else {
            self.dom_mut()
                .insert_before(parent, fragment, child)
                .map(drop)
        };
        self.after_mutation(cx);
        r.js()
    }
}

/// The fragment parser mode of an HTML insertion method (HTML "fragment parser mode") with its
/// options: Safe sanitizes (with the options' `sanitizer`, the default one when there is none),
/// Unsafe sanitizes only with a `sanitizer` and may run scripts once inserted (`runScripts`).
pub(crate) enum InsertMode {
    Safe(Option<SanitizerInput>),
    Unsafe {
        run_scripts: bool,
        input: Option<SanitizerInput>,
    },
}

impl InsertMode {
    pub(crate) fn safe(options: SetHTMLOptions) -> InsertMode {
        InsertMode::Safe(Some(options.sanitizer.into()))
    }
}
