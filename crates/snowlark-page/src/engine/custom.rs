// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Custom elements (HTML §4.13): the registry, definitions, element creation with and without the
//! synchronous custom elements flag, `[HTMLConstructor]`, upgrades, and the custom element reactions
//! stack with its element queues and backup element queue.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use snowlark_bindings::generated::{
    CustomElementConstructor, CustomElementConstructorOrUndefined, CustomElementRegistryImpl,
    ElementDefinitionOptions,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsPromise, ToJs};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{ns, CustomElementState, LocalName, Namespace, NodeId, QualName};

use super::held::Held;
use super::{exception, handle_node, node_handle, DomResultExt, Engine, Object};

/// A thrown exception and its message.
type Thrown = (v8::Global<v8::Value>, Option<v8::Global<v8::Message>>);

/// A definition's JS values.
pub struct DefinitionJs {
    pub constructor: Held<v8::Object>,
    // Held for the definition's lifetime as the spec's definition holds it; nothing reads it yet.
    pub prototype: Held<v8::Object>,
    pub connected: Option<Held<v8::Object>>,
    pub disconnected: Option<Held<v8::Object>>,
    pub adopted: Option<Held<v8::Object>>,
    pub attribute_changed: Option<Held<v8::Object>>,
    // (connectedMoveCallback is not implemented yet)
    pub connected_move: Option<Held<v8::Object>>,
}

impl Definition {
    /// The constructor (none once its realm is gone).
    pub fn constructor(&self, scope: &v8::PinScope<'_, '_>) -> Option<JsObject> {
        self.js.borrow().constructor.get(scope)
    }

    /// Whether `c` is the definition's constructor.
    pub fn is_constructor(&self, scope: &v8::PinScope<'_, '_>, c: &JsObject) -> bool {
        self.js.borrow().constructor.is(scope, c)
    }

    /// From now on `owner`'s wrapper keeps the definition's JS values (engine/held.rs).
    pub fn trace(&self, scope: &mut v8::PinScope<'_, '_>, owner: Handle) {
        let mut js = self.js.borrow_mut();
        let js = &mut *js;
        js.constructor.trace(scope, owner);
        js.prototype.trace(scope, owner);
        for h in [
            &mut js.connected,
            &mut js.disconnected,
            &mut js.adopted,
            &mut js.attribute_changed,
            &mut js.connected_move,
        ]
        .into_iter()
        .flatten()
        {
            h.trace(scope, owner);
        }
    }
}

/// A custom element definition (HTML §4.13.4).
pub struct Definition {
    /// The CustomElementRegistry (one per window) the definition is in.
    pub registry: Handle,
    pub name: String,
    pub local_name: String,
    /// The constructor, prototype and lifecycle callbacks (engine/held.rs: kept by the registry's
    /// Window once its browsing context is discarded).
    pub js: RefCell<DefinitionJs>,
    pub observed_attributes: Vec<String>,
    // Form association and ElementInternals are not implemented yet; the definition records them
    // so that `define()` reads the constructor's properties in spec order.
    #[allow(dead_code)]
    pub form_associated: bool,
    #[allow(dead_code)]
    pub disable_internals: bool,
    pub disable_shadow: bool,
    /// The construction stack: `None` is the "already constructed" marker.
    pub construction_stack: std::cell::RefCell<Vec<Option<NodeId>>>,
}

/// A custom element reaction (HTML §4.13.6).
pub enum CeReaction {
    Upgrade(Rc<Definition>),
    Callback {
        callback: JsObject,
        args: Vec<CbArg>,
    },
}

/// An argument of a lifecycle callback.
#[derive(Clone)]
pub enum CbArg {
    Str(String),
    OptStr(Option<String>),
    Node(Handle),
}

impl ToJs for CbArg {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        match self {
            CbArg::Str(s) => s.to_js(scope),
            CbArg::OptStr(s) => s.to_js(scope),
            CbArg::Node(h) => h.to_js(scope),
        }
    }
}

/// The custom element state of one event loop.
#[derive(Default)]
pub struct CustomElements {
    /// The custom element reactions stack: element queues.
    pub stack: Vec<Vec<NodeId>>,
    pub backup: Vec<NodeId>,
    pub processing_backup: bool,
    pub definitions: Vec<Rc<Definition>>,
    pub element_definition: rustc_hash::FxHashMap<NodeId, Rc<Definition>>,
    pub queues: rustc_hash::FxHashMap<NodeId, VecDeque<CeReaction>>,
    pub when_defined: HashMap<(Handle, String), Held<v8::PromiseResolver>>,
    pub definition_running: bool,
    /// Elements whose wrapper interface differs from their local name's (none yet; kept for
    /// customized built-ins that are upgraded in place).
    pub interface_override: rustc_hash::FxHashMap<NodeId, InterfaceId>,
}

impl Engine {
    /// A window's CustomElementRegistry.
    pub(crate) fn registry_handle(&self, window: Handle) -> Handle {
        self.window_singleton(window, "customElements", || Object::CustomElementRegistry)
    }

    /// The registry of `doc`'s window, if `doc` is an active document.
    fn registry_of_document(&self, doc: NodeId) -> Option<Handle> {
        self.window_of_document(doc)
            .map(|w| self.registry_handle(w))
    }

    /// The window whose registry `registry` is.
    fn window_of_registry(&self, registry: Handle) -> Option<Handle> {
        let f = self.frames.borrow();
        f.singletons
            .iter()
            .find(|((_, k), h)| *k == "customElements" && **h == registry)
            .map(|((w, _), _)| *w)
    }

    /// HTML §4.13.4 "look up a custom element definition".
    pub(crate) fn look_up_definition(
        &self,
        document: NodeId,
        namespace: &Namespace,
        local_name: &str,
        is: Option<&str>,
    ) -> Option<Rc<Definition>> {
        if self.custom.borrow().definitions.is_empty() {
            return None;
        }
        if *namespace != ns!(html) || !self.is_active_document(document) {
            return None;
        }
        let registry = self.registry_of_document(document)?;
        let ce = self.custom.borrow();
        ce.definitions
            .iter()
            .find(|d| d.registry == registry && d.name == local_name && d.local_name == local_name)
            .or_else(|| {
                is.and_then(|is| {
                    ce.definitions.iter().find(|d| {
                        d.registry == registry && d.name == is && d.local_name == local_name
                    })
                })
            })
            .cloned()
    }

    pub(crate) fn shadow_disabled(&self, el: NodeId) -> bool {
        self.custom
            .borrow()
            .element_definition
            .get(&el)
            .is_some_and(|d| d.disable_shadow)
    }

    // ----- reactions ------------------------------------------------------------------------------------

    /// "Enqueue an element on the appropriate element queue".
    fn enqueue_element(&self, cx: &mut Cx, el: NodeId) {
        let schedule = {
            let mut ce = self.custom.borrow_mut();
            if let Some(top) = ce.stack.last_mut() {
                top.push(el);
                false
            } else {
                ce.backup.push(el);
                if ce.processing_backup {
                    false
                } else {
                    ce.processing_backup = true;
                    true
                }
            }
        };
        if schedule {
            self.queue_internal_microtask(
                cx,
                super::reactions::Microtask::CustomElementBackupQueue,
            );
        }
    }

    /// The backup element queue's microtask.
    pub(crate) fn process_backup_queue(&self, cx: &mut Cx) {
        let queue = std::mem::take(&mut self.custom.borrow_mut().backup);
        self.invoke_reactions(cx, queue);
        self.custom.borrow_mut().processing_backup = false;
    }

    /// "Enqueue a custom element callback reaction".
    pub(crate) fn enqueue_callback(&self, cx: &mut Cx, el: NodeId, name: &str, args: Vec<CbArg>) {
        let Some(def) = self.custom.borrow().element_definition.get(&el).cloned() else {
            return;
        };
        let callback = {
            let js = def.js.borrow();
            match name {
                "connectedCallback" => js.connected.clone(),
                "disconnectedCallback" => js.disconnected.clone(),
                "adoptedCallback" => js.adopted.clone(),
                "attributeChangedCallback" => {
                    let attr = match args.first() {
                        Some(CbArg::Str(a)) => a.clone(),
                        _ => String::new(),
                    };
                    if !def.observed_attributes.contains(&attr) {
                        return;
                    }
                    js.attribute_changed.clone()
                }
                _ => None,
            }
        };
        let Some(callback) = callback.and_then(|c| c.get(cx.scope())) else {
            return;
        };
        self.custom
            .borrow_mut()
            .queues
            .entry(el)
            .or_default()
            .push_back(CeReaction::Callback { callback, args });
        self.enqueue_element(cx, el);
    }

    /// "Enqueue a custom element upgrade reaction".
    fn enqueue_upgrade(&self, cx: &mut Cx, el: NodeId, def: Rc<Definition>) {
        self.custom
            .borrow_mut()
            .queues
            .entry(el)
            .or_default()
            .push_back(CeReaction::Upgrade(def));
        self.enqueue_element(cx, el);
    }

    /// "Try to upgrade an element".
    pub(crate) fn try_upgrade(&self, cx: &mut Cx, el: NodeId) {
        let def = {
            let dom = self.dom();
            let Some(e) = dom.element(el) else {
                return;
            };
            if !matches!(
                dom.custom_element_state(el),
                CustomElementState::Undefined | CustomElementState::Uncustomized
            ) {
                return;
            }
            let doc = dom.node_document(el);
            self.look_up_definition(
                doc,
                &e.name().ns,
                &e.name().local,
                dom.is_value(el).map(|v| &**v),
            )
        };
        if let Some(def) = def {
            self.enqueue_upgrade(cx, el, def);
        }
    }

    /// "Invoke custom element reactions" in an element queue.
    pub(crate) fn invoke_reactions(&self, cx: &mut Cx, queue: Vec<NodeId>) {
        for el in queue {
            loop {
                let reaction = self
                    .custom
                    .borrow_mut()
                    .queues
                    .get_mut(&el)
                    .and_then(|q| q.pop_front());
                let Some(reaction) = reaction else {
                    break;
                };
                match reaction {
                    CeReaction::Upgrade(def) => {
                        if let Err(e) = self.upgrade(cx, el, def) {
                            self.report_error(cx, e);
                        }
                    }
                    CeReaction::Callback { callback, args } => {
                        self.call_function(cx, &callback, Some(node_handle(el)), &args);
                    }
                }
            }
        }
    }

    /// Calls a JS function with `this` and arguments, reporting what it throws.
    pub(crate) fn call_function(
        &self,
        cx: &mut Cx,
        f: &JsObject,
        this: Option<Handle>,
        args: &[CbArg],
    ) -> Option<v8::Global<v8::Value>> {
        rt::in_callback_realm(cx, f, None, |cx| self.call_in_realm(cx, f, this, args))
    }

    /// [`Engine::call_function`], in the callback's realm.
    fn call_in_realm(
        &self,
        cx: &mut Cx,
        f: &JsObject,
        this: Option<Handle>,
        args: &[CbArg],
    ) -> Option<v8::Global<v8::Value>> {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let func = v8::Local::new(tc, f.global());
        let Ok(func) = v8::Local::<v8::Function>::try_from(func) else {
            return None;
        };
        let recv: v8::Local<v8::Value> = match this {
            Some(h) => rt::wrap(tc, h)
                .map(Into::into)
                .unwrap_or_else(|| v8::undefined(tc).into()),
            None => v8::undefined(tc).into(),
        };
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(a.to_js(tc).unwrap_or_else(|| v8::undefined(tc).into()));
        }
        match func.call(tc, recv, &argv) {
            Some(v) => Some(v8::Global::new(tc, v)),
            None => {
                if let Some(e) = tc.exception() {
                    let e = v8::Global::new(tc, e);
                    let m = tc.message().map(|m| v8::Global::new(tc, m));
                    tc.reset();
                    let mut cx2 = Cx::new(tc);
                    self.report_exception(&mut cx2, e, m);
                }
                None
            }
        }
    }

    /// Reports an engine error (a pending JS exception, or one to create) at the window.
    pub(crate) fn report_error(&self, cx: &mut Cx, e: rt::Error) {
        let scope = cx.scope();
        let value = match e {
            rt::Error::Pending => return,
            other => rt::error_value(scope, other),
        };
        if let Some(v) = value {
            let g = v8::Global::new(scope, v);
            self.report_exception(cx, g, None);
        }
    }

    /// HTML §4.13.5 "upgrade an element".
    fn upgrade(&self, cx: &mut Cx, el: NodeId, def: Rc<Definition>) -> rt::Result<()> {
        // 1.
        let state = self.dom().custom_element_state(el);
        if !matches!(
            state,
            CustomElementState::Undefined | CustomElementState::Uncustomized
        ) {
            return Ok(());
        }
        // 2-3.
        self.custom
            .borrow_mut()
            .element_definition
            .insert(el, def.clone());
        self.set_custom_element_state(el, CustomElementState::Failed);
        // 4.
        let attrs: Vec<(String, String, Option<String>)> = {
            let dom = self.dom();
            dom.attrs(el)
                .iter()
                .map(|a| {
                    (
                        a.local_name().to_string(),
                        a.value().to_owned(),
                        (!a.namespace().is_empty()).then(|| a.namespace().to_string()),
                    )
                })
                .collect()
        };
        for (name, value, namespace) in attrs {
            self.enqueue_callback(
                cx,
                el,
                "attributeChangedCallback",
                vec![
                    CbArg::Str(name),
                    CbArg::OptStr(None),
                    CbArg::OptStr(Some(value)),
                    CbArg::OptStr(namespace),
                ],
            );
        }
        // 5.
        if self.dom().is_connected(el) {
            self.enqueue_callback(cx, el, "connectedCallback", Vec::new());
        }
        // 6.
        def.construction_stack.borrow_mut().push(Some(el));
        // 8.
        let result = (|| {
            if def.disable_shadow && self.dom().shadow_root(el).is_some() {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "The definition disables shadow, but the element has a shadow root",
                ));
            }
            self.set_custom_element_state(el, CustomElementState::Precustomized);
            let constructor = def
                .constructor(cx.scope())
                .ok_or_else(|| rt::Error::Type("The definition's realm is gone".into()))?;
            let constructed = self.construct(cx, &constructor)?;
            if handle_node(constructed) != Some(el) {
                return Err(rt::Error::Type(
                    "The custom element constructor did not produce the element being upgraded"
                        .into(),
                ));
            }
            Ok(())
        })();
        def.construction_stack.borrow_mut().pop();
        if let Err(e) = result {
            let mut ce = self.custom.borrow_mut();
            ce.element_definition.remove(&el);
            ce.queues.remove(&el);
            return Err(e);
        }
        // 10.
        self.set_custom_element_state(el, CustomElementState::Custom);
        Ok(())
    }

    /// `Construct(C)` with no arguments, returning the wrapper's handle. A JS exception stays pending.
    fn construct(&self, cx: &mut Cx, c: &JsObject) -> rt::Result<Handle> {
        let scope = cx.scope();
        let f = v8::Local::new(scope, c.global());
        let Ok(f) = v8::Local::<v8::Function>::try_from(f) else {
            return Err(rt::Error::Type("The constructor is not callable".into()));
        };
        let obj = f.new_instance(scope, &[]).ok_or(rt::Error::Pending)?;
        rt::unwrap(
            &obj,
            snowlark_bindings::generated::ids::NODE,
            snowlark_bindings::generated::ids::NODE_END,
        )
        .ok_or_else(|| rt::Error::Type("The constructor did not return a node".into()))
    }

    /// `Construct(C)` with exceptions caught: the handle, or the exception.
    fn construct_catching(&self, cx: &mut Cx, c: &JsObject) -> Result<Handle, Option<Thrown>> {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let mut cx2 = Cx::new(tc);
        let r = self.construct(&mut cx2, c);
        match r {
            Ok(h) => Ok(h),
            Err(rt::Error::Pending) => {
                let e = tc.exception().map(|e| v8::Global::new(tc, e));
                let m = tc.message().map(|m| v8::Global::new(tc, m));
                tc.reset();
                Err(e.map(|e| (e, m)))
            }
            Err(other) => {
                let v = rt::error_value(tc, other).map(|v| v8::Global::new(tc, v));
                Err(v.map(|v| (v, None)))
            }
        }
    }

    // ----- creating elements ----------------------------------------------------------------------------

    /// `createElement` / `createElementNS`: validation, then "create an element" with the synchronous
    /// custom elements flag set.
    pub(crate) fn create_element_api(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        namespace: Option<Option<&str>>,
        name: &str,
        is: Option<String>,
        ns_variant: bool,
    ) -> rt::Result<NodeId> {
        let qname = if ns_variant {
            snowlark_dom::names::validate_and_extract(
                namespace.flatten(),
                name,
                snowlark_dom::names::NameContext::Element,
            )
            .js()?
        } else {
            if !snowlark_dom::names::is_valid_element_local_name(name) {
                return Err(super::dom_err(snowlark_dom::DomError::InvalidCharacter));
            }
            let dom = self.dom();
            let html = dom.is_html_document(doc);
            let local = if html && name.bytes().any(|b| b.is_ascii_uppercase()) {
                LocalName::from(name.to_ascii_lowercase())
            } else {
                LocalName::from(name)
            };
            let namespace = if html || dom.content_type(doc) == "application/xhtml+xml" {
                ns!(html)
            } else {
                ns!()
            };
            QualName::new(None, namespace, local)
        };
        self.create_an_element(cx, doc, qname, is, true)
    }

    /// DOM §4.9 "create an element".
    pub(crate) fn create_an_element(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        name: QualName,
        is: Option<String>,
        synchronous: bool,
    ) -> rt::Result<NodeId> {
        let def = self.look_up_definition(doc, &name.ns, &name.local, is.as_deref());
        match def {
            // 6: a customized built-in element.
            Some(def) if def.name != *name.local => {
                let el = self.dom_mut().create_element_with_name(
                    doc,
                    name,
                    is.as_deref().map(LocalName::from),
                );
                self.set_custom_element_state(el, CustomElementState::Undefined);
                if synchronous {
                    if let Err(e) = self.upgrade(cx, el, def) {
                        self.report_error(cx, e);
                        self.set_custom_element_state(el, CustomElementState::Failed);
                    }
                } else {
                    self.enqueue_upgrade(cx, el, def);
                }
                Ok(el)
            }
            // 7: an autonomous custom element.
            Some(def) => {
                if synchronous {
                    let Some(constructor) = def.constructor(cx.scope()) else {
                        return Ok(self.failed_element(doc, name));
                    };
                    match self.construct_catching(cx, &constructor) {
                        Ok(h) => {
                            let el = handle_node(h).expect("a node");
                            if let Err(e) = self.check_constructed(doc, el, &name) {
                                self.report_error(cx, e);
                                return Ok(self.failed_element(doc, name));
                            }
                            // 7.1.5: the namespace prefix.
                            self.dom_mut().set_element_prefix(el, name.prefix.clone());
                            Ok(el)
                        }
                        Err(e) => {
                            if let Some((e, m)) = e {
                                self.report_exception(cx, e, m);
                            }
                            Ok(self.failed_element(doc, name))
                        }
                    }
                } else {
                    let el = self.dom_mut().create_element_with_name(doc, name, None);
                    self.set_custom_element_state(el, CustomElementState::Undefined);
                    self.enqueue_upgrade(cx, el, def);
                    Ok(el)
                }
            }
            None => Ok(self.dom_mut().create_element_with_name(
                doc,
                name,
                is.as_deref().map(LocalName::from),
            )),
        }
    }

    /// The checks of "create an element" step 7.1.4 on a constructed element.
    fn check_constructed(&self, doc: NodeId, el: NodeId, name: &QualName) -> rt::Result<()> {
        let dom = self.dom();
        let not_supported = |m: &'static str| exception(ExceptionName::NotSupportedError, m);
        if dom.element(el).is_none() || dom.namespace(el) != &ns!(html) {
            return Err(rt::Error::Type(
                "The constructor did not return an HTMLElement".into(),
            ));
        }
        if dom.has_attributes(el) {
            return Err(not_supported("The constructed element has attributes"));
        }
        if dom.has_child_nodes(el) {
            return Err(not_supported("The constructed element has children"));
        }
        if dom.parent(el).is_some() {
            return Err(not_supported("The constructed element has a parent"));
        }
        if dom.node_document(el) != doc {
            return Err(not_supported(
                "The constructed element is in another document",
            ));
        }
        if *dom.local_name(el) != name.local {
            return Err(not_supported(
                "The constructed element has another local name",
            ));
        }
        Ok(())
    }

    fn failed_element(&self, doc: NodeId, name: QualName) -> NodeId {
        let el = self.dom_mut().create_element_with_name(doc, name, None);
        self.set_custom_element_state(el, CustomElementState::Failed);
        self.custom
            .borrow_mut()
            .interface_override
            .insert(el, InterfaceId::HTMLUnknownElement);
        el
    }

    /// HTML §3.2.3 `[HTMLConstructor]` steps.
    pub(crate) fn html_constructor_steps(
        &self,
        cx: &mut Cx,
        interface: InterfaceId,
        new_target: &JsObject,
    ) -> rt::Result<Handle> {
        let current_doc = self.current_document(cx)?;
        let current_window = self.current_window(cx);
        let scope = cx.scope();
        // 2: NewTarget is the active function object.
        let active = rt::state(scope)
            .template(scope, interface as u16)
            .and_then(|t| t.get_function(scope));
        let nt = v8::Local::new(scope, new_target.global());
        if active.is_some_and(|a| a == nt) {
            return Err(rt::Error::Type("Illegal constructor".into()));
        }
        // 3.
        let registry = current_window.map(|w| self.registry_handle(w));
        let def = self
            .custom
            .borrow()
            .definitions
            .iter()
            .find(|d| Some(d.registry) == registry && d.is_constructor(scope, new_target))
            .cloned()
            .ok_or_else(|| {
                rt::Error::Type("Illegal constructor: no custom element definition".into())
            })?;
        // 5-6.
        if def.name == def.local_name {
            if interface != InterfaceId::HTMLElement {
                return Err(rt::Error::Type(
                    "An autonomous custom element must extend HTMLElement".into(),
                ));
            }
        } else {
            let qn = QualName::new(None, ns!(html), LocalName::from(def.local_name.as_str()));
            if super::html::element_interface(&qn) != interface {
                return Err(rt::Error::Type(
                    "The custom element extends an element of another interface".into(),
                ));
            }
        }
        // 8.
        let proto_key = rt::name(scope, "prototype");
        let proto = nt.get(scope, proto_key.into()).ok_or(rt::Error::Pending)?;
        let top = def.construction_stack.borrow().last().copied();
        let el = match top {
            // 7: not an upgrade: a new element.
            None => {
                let doc = current_doc;
                let is = (def.name != def.local_name).then(|| LocalName::from(def.name.as_str()));
                let el = self.dom_mut().create_element_with_name(
                    doc,
                    QualName::new(None, ns!(html), LocalName::from(def.local_name.as_str())),
                    is,
                );
                self.custom
                    .borrow_mut()
                    .element_definition
                    .insert(el, def.clone());
                self.set_custom_element_state(el, CustomElementState::Custom);
                el
            }
            // 10.
            Some(None) => {
                return Err(rt::Error::Type(
                    "The custom element was already constructed".into(),
                ))
            }
            Some(Some(el)) => {
                // 12.
                if let Some(last) = def.construction_stack.borrow_mut().last_mut() {
                    *last = None;
                }
                el
            }
        };
        // 7.1 / 11: the prototype comes from NewTarget.
        let h = node_handle(el);
        let scope = cx.scope();
        let w = rt::wrap(scope, h).ok_or(rt::Error::Pending)?;
        if proto.is_object() {
            w.set_prototype(scope, proto).ok_or(rt::Error::Pending)?;
        } else {
            // Get(NewTarget, "prototype") is not an object: the realm's interface prototype stays.
        }
        Ok(h)
    }

    /// Enqueues upgrade reactions for the elements of a freshly cloned or parsed subtree that have a
    /// definition (DOM §4.4 "clone a node" creates elements with the synchronous flag unset).
    pub(crate) fn upgrade_subtree(&self, cx: &mut Cx, root: NodeId) {
        let candidates: Vec<NodeId> = {
            let dom = self.dom();
            dom.shadow_including_inclusive_descendants(root)
                .filter(|&d| {
                    dom.element(d).is_some()
                        && dom.custom_element_state(d) == CustomElementState::Undefined
                })
                .collect()
        };
        for c in candidates {
            self.try_upgrade(cx, c);
        }
    }

    /// `cloneNode` / `importNode`: DOM "clone a node", then the reactions it implies.
    pub(crate) fn clone_with_steps(
        &self,
        cx: &mut Cx,
        node: NodeId,
        document: Option<NodeId>,
        subtree: bool,
    ) -> rt::Result<NodeId> {
        // (the copies are new and no script runs while they are made: "clone a node" creates
        // elements without synchronous custom elements, so none has a wrapper yet, as with
        // fragment parsing)
        let copy = self.parsed_insertion(cx, |dom| {
            let doc = document.unwrap_or_else(|| dom.node_document(node));
            dom.clone_a_node(node, doc, subtree, None)
        });
        self.upgrade_subtree(cx, copy);
        Ok(copy)
    }
}

impl CustomElementRegistryImpl for Engine {
    /// HTML §4.13.4 `define(name, constructor, options)`.
    fn define(
        &self,
        cx: &mut Cx,
        this: Handle,
        name: &str,
        constructor: CustomElementConstructor,
        options: ElementDefinitionOptions,
    ) -> rt::Result<()> {
        let c = constructor.0;
        // 1.
        {
            let scope = cx.scope();
            let f = v8::Local::new(scope, c.global());
            let is_ctor = v8::Local::<v8::Function>::try_from(f).is_ok_and(|f| f.is_constructor());
            if !is_ctor {
                return Err(rt::Error::Type(
                    "The constructor is not a constructor".into(),
                ));
            }
        }
        // 2.
        if !snowlark_dom::names::is_valid_custom_element_name(name) {
            return Err(exception(
                ExceptionName::SyntaxError,
                "Not a valid custom element name",
            ));
        }
        // 3-4.
        {
            let ce = self.custom.borrow();
            if ce
                .definitions
                .iter()
                .any(|d| d.registry == this && d.name == name)
            {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "The name is already defined",
                ));
            }
            if ce
                .definitions
                .iter()
                .any(|d| d.registry == this && d.is_constructor(cx.scope(), &c))
            {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "The constructor is already defined",
                ));
            }
        }
        // 5-6.
        let mut local_name = name.to_owned();
        if let Some(extends) = &options.extends {
            if snowlark_dom::names::is_valid_custom_element_name(extends) {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "extends names a custom element",
                ));
            }
            let qn = QualName::new(None, ns!(html), LocalName::from(extends.as_str()));
            if super::html::element_interface(&qn) == InterfaceId::HTMLUnknownElement {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "extends names an unknown element",
                ));
            }
            local_name = extends.clone();
        }
        // 7-8.
        if self.custom.borrow().definition_running {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "A definition is already running",
            ));
        }
        self.custom.borrow_mut().definition_running = true;
        let read = self.read_definition(cx, &c);
        self.custom.borrow_mut().definition_running = false;
        let mut def = read?;
        def.registry = this;
        def.name = name.to_owned();
        def.local_name = local_name.clone();
        let def = Rc::new(def);
        // 13.
        self.custom.borrow_mut().definitions.push(def.clone());
        // 14-16: upgrade candidates in the window's document.
        if let Some(doc) = self
            .window_of_registry(this)
            .and_then(|w| self.document_of_window(w))
        {
            let candidates: Vec<NodeId> = {
                let dom = self.dom();
                dom.shadow_including_descendants(doc)
                    .filter(|&d| {
                        dom.element(d).is_some_and(|e| {
                            e.name().ns == ns!(html) && *e.name().local == *local_name
                        }) && (options.extends.is_none()
                            || dom.is_value(d).is_some_and(|v| **v == *name))
                    })
                    .collect()
            };
            for el in candidates {
                self.enqueue_upgrade(cx, el, def.clone());
            }
        }
        // 17.
        let promise = self
            .custom
            .borrow_mut()
            .when_defined
            .remove(&(this, name.to_owned()));
        let promise = promise.and_then(|p| p.get(cx.scope()));
        if let (Some(p), Some(c)) = (promise, def.constructor(cx.scope())) {
            p.resolve(cx, c);
        }
        Ok(())
    }

    fn get(
        &self,
        cx: &mut Cx,
        this: Handle,
        name: &str,
    ) -> rt::Result<CustomElementConstructorOrUndefined> {
        Ok(
            match self
                .custom
                .borrow()
                .definitions
                .iter()
                .find(|d| d.registry == this && d.name == name)
            {
                Some(d) => match d.constructor(cx.scope()) {
                    Some(c) => CustomElementConstructorOrUndefined::CustomElementConstructor(
                        CustomElementConstructor(c),
                    ),
                    None => CustomElementConstructorOrUndefined::Undefined(()),
                },
                None => CustomElementConstructorOrUndefined::Undefined(()),
            },
        )
    }

    fn get_name(
        &self,
        cx: &mut Cx,
        this: Handle,
        constructor: CustomElementConstructor,
    ) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .custom
            .borrow()
            .definitions
            .iter()
            .find(|d| d.registry == this && d.is_constructor(cx.scope(), &constructor.0))
            .map(|d| DomStr::Owned(d.name.clone())))
    }

    fn when_defined(&self, cx: &mut Cx, this: Handle, name: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if !snowlark_dom::names::is_valid_custom_element_name(name) {
            p.reject(
                cx,
                exception(
                    ExceptionName::SyntaxError,
                    "Not a valid custom element name",
                ),
            );
            return Ok(p);
        }
        let defined = self
            .custom
            .borrow()
            .definitions
            .iter()
            .find(|d| d.registry == this && d.name == name)
            .and_then(|d| d.constructor(cx.scope()));
        if let Some(c) = defined {
            p.resolve(cx, c);
            return Ok(p);
        }
        let key = (this, name.to_owned());
        let existing = self.custom.borrow().when_defined.get(&key).cloned();
        if let Some(e) = existing.and_then(|e| e.get(cx.scope())) {
            return Ok(e);
        }
        self.custom
            .borrow_mut()
            .when_defined
            .insert(key, Held::promise(p.clone()));
        Ok(p)
    }

    fn upgrade(&self, cx: &mut Cx, this: Handle, root: Handle) -> rt::Result<()> {
        let root = self.n(root);
        let candidates: Vec<NodeId> = {
            let dom = self.dom();
            dom.shadow_including_inclusive_descendants(root)
                .filter(|&d| dom.element(d).is_some())
                .collect()
        };
        for c in candidates {
            self.try_upgrade_with_document(cx, c, this);
        }
        Ok(())
    }

    fn initialize(&self, _cx: &mut Cx, _this: Handle, _root: Handle) -> rt::Result<()> {
        Ok(())
    }

    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::CustomElementRegistry))
    }
}

impl Engine {
    /// `customElements.upgrade(root)` tries elements in any document (the registry's global is the
    /// window's, so the lookup uses the window's document's registry).
    fn try_upgrade_with_document(&self, cx: &mut Cx, el: NodeId, registry: Handle) {
        let def = {
            let dom = self.dom();
            let Some(e) = dom.element(el) else {
                return;
            };
            if !matches!(
                dom.custom_element_state(el),
                CustomElementState::Undefined | CustomElementState::Uncustomized
            ) {
                return;
            }
            let Some(doc) = self
                .window_of_registry(registry)
                .and_then(|w| self.document_of_window(w))
            else {
                return;
            };
            self.look_up_definition(
                doc,
                &e.name().ns,
                &e.name().local,
                dom.is_value(el).map(|v| &**v),
            )
        };
        if let Some(def) = def {
            self.enqueue_upgrade(cx, el, def);
        }
    }

    /// HTML §4.13.4 `define` steps 10.1-10.7: what the constructor and its prototype say.
    fn read_definition(&self, cx: &mut Cx, c: &JsObject) -> rt::Result<Definition> {
        let scope = cx.scope();
        let ctor = v8::Local::new(scope, c.global());

        let proto = get_property(scope, ctor, "prototype")?;
        let Ok(proto) = v8::Local::<v8::Object>::try_from(proto) else {
            return Err(rt::Error::Type(
                "The constructor's prototype is not an object".into(),
            ));
        };
        let connected = lifecycle_callback(scope, proto, "connectedCallback")?;
        let disconnected = lifecycle_callback(scope, proto, "disconnectedCallback")?;
        let adopted = lifecycle_callback(scope, proto, "adoptedCallback")?;
        let attribute_changed = lifecycle_callback(scope, proto, "attributeChangedCallback")?;
        let connected_move = lifecycle_callback(scope, proto, "connectedMoveCallback")?;
        let mut observed = Vec::new();
        if attribute_changed.is_some() {
            let v = get_property(scope, ctor, "observedAttributes")?;
            if !v.is_undefined() {
                observed = self.string_sequence(scope, v)?;
            }
        }
        let mut disabled = Vec::new();
        let v = get_property(scope, ctor, "disabledFeatures")?;
        if !v.is_undefined() {
            disabled = self.string_sequence(scope, v)?;
        }
        let fa = get_property(scope, ctor, "formAssociated")?;
        let form_associated = fa.boolean_value(scope);
        Ok(Definition {
            registry: Handle::new(1),
            name: String::new(),
            local_name: String::new(),
            js: RefCell::new(DefinitionJs {
                constructor: Held::object(c.clone()),
                prototype: Held::object(JsObject::new(scope, proto)),
                connected: connected.map(Held::object),
                disconnected: disconnected.map(Held::object),
                adopted: adopted.map(Held::object),
                attribute_changed: attribute_changed.map(Held::object),
                connected_move: connected_move.map(Held::object),
            }),
            observed_attributes: observed,
            form_associated,
            disable_internals: disabled.iter().any(|d| d == "internals"),
            disable_shadow: disabled.iter().any(|d| d == "shadow"),
            construction_stack: Default::default(),
        })
    }

    /// Converts a JS value to `sequence<DOMString>`.
    fn string_sequence(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> rt::Result<Vec<String>> {
        let mut out = Vec::new();
        rt::iterate(scope, v, |scope, item| {
            out.push(rt::to_dom_string(scope, item)?);
            Some(())
        })
        .ok_or(rt::Error::Pending)?;
        Ok(out)
    }
}

/// `Get(o, key)`, with a thrown exception as `Error::Pending`.
fn get_property<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    o: v8::Local<'s, v8::Object>,
    key: &str,
) -> rt::Result<v8::Local<'s, v8::Value>> {
    let k = rt::name(scope, key);
    o.get(scope, k.into()).ok_or(rt::Error::Pending)
}

/// A lifecycle callback of a custom element's prototype: undefined, or a function.
fn lifecycle_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<'s, v8::Object>,
    name: &str,
) -> rt::Result<Option<JsObject>> {
    let v = get_property(scope, proto, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    match v8::Local::<v8::Function>::try_from(v) {
        Ok(f) => Ok(Some(JsObject::new(scope, f.into()))),
        Err(_) => Err(rt::Error::Type(format!("{name} is not a function").into())),
    }
}

impl Engine {
    /// Sets an element's custom element state, and its `:defined` state in the style engine (HTML
    /// §4.16.3: `:defined` matches an element whose custom element state is "uncustomized" or
    /// "custom"), so the rules that test it apply again.
    pub(crate) fn set_custom_element_state(&self, el: NodeId, state: CustomElementState) {
        let was = self.dom().custom_element_state(el);
        self.dom_mut().set_custom_element_state(el, state);
        // (a custom element of a form-associated definition is a listed element from now on)
        let face = state == CustomElementState::Custom && self.is_form_associated_custom(el);
        self.dom_mut().set_form_associated_custom(el, face);
        let defined = |s| {
            matches!(
                s,
                CustomElementState::Uncustomized | CustomElementState::Custom
            )
        };
        if defined(was) != defined(state) {
            self.set_element_state(el, snowlark_style::ElementState::DEFINED, defined(state));
        }
    }
}
