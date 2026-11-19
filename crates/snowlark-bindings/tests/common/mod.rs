// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A mock engine for the bindings tests: a few element handles with attributes, DOMExceptions, and a log
//! of the operations called. Interfaces it does not implement use the `stubs` feature's defaults.

#![allow(dead_code)]

use snowlark_bindings::generated::*;
use snowlark_bindings::rt::{
    self, Cx, DomStr, Error, ExceptionName, Handle, JsObject, JsPromise, Result,
};
use std::cell::RefCell;

/// Exceptions live above this handle value; nodes below.
const EXCEPTION_BASE: u64 = 1 << 40;

pub struct MockNode {
    pub interface: InterfaceId,
    /// The local name of an element, the type of an event.
    pub local_name: String,
    /// Attributes of an element; flags of an event.
    pub attrs: Vec<(String, String)>,
    /// The members of a collection.
    pub items: Vec<Handle>,
    /// The element a DOMStringMap belongs to.
    pub owner: Option<Handle>,
}

impl MockNode {
    fn new(interface: InterfaceId) -> MockNode {
        MockNode {
            interface,
            local_name: String::new(),
            attrs: Vec::new(),
            items: Vec::new(),
            owner: None,
        }
    }
}

#[derive(Default)]
pub struct Mock {
    pub nodes: RefCell<Vec<MockNode>>,
    pub exceptions: RefCell<Vec<(String, String)>>,
    pub log: RefCell<Vec<String>>,
    pub listeners: RefCell<Vec<(Handle, String, EventListener)>>,
    pub handlers: RefCell<Vec<(Handle, &'static str, JsObject)>>,
    /// `nodeType` panics once this many more reads have run (a fault in engine code).
    pub node_type_panics_after: std::cell::Cell<Option<u32>>,
}

impl Mock {
    pub fn add(&self, node: MockNode) -> Handle {
        let mut nodes = self.nodes.borrow_mut();
        nodes.push(node);
        Handle::new(nodes.len() as u64 - 1)
    }

    pub fn add_element(&self, interface: InterfaceId, local_name: &str) -> Handle {
        self.add(MockNode {
            local_name: local_name.into(),
            ..MockNode::new(interface)
        })
    }

    pub fn add_collection(&self, interface: InterfaceId, items: Vec<Handle>) -> Handle {
        self.add(MockNode {
            items,
            ..MockNode::new(interface)
        })
    }

    fn items(&self, h: Handle) -> Vec<Handle> {
        self.nodes.borrow()[h.get() as usize].items.clone()
    }

    fn owner(&self, h: Handle) -> Handle {
        self.nodes.borrow()[h.get() as usize].owner.unwrap()
    }

    pub fn attr(&self, h: Handle, name: &str) -> Option<String> {
        self.nodes.borrow()[h.get() as usize]
            .attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

impl rt::Host for Mock {
    fn interface_of(&self, handle: Handle) -> InterfaceId {
        if handle.get() >= EXCEPTION_BASE {
            InterfaceId::DOMException
        } else {
            self.nodes.borrow()[handle.get() as usize].interface
        }
    }

    fn push_ce_reactions(&self) {
        self.log.borrow_mut().push("ce:push".into());
    }

    fn pop_ce_reactions(&self, _cx: &mut Cx) {
        self.log.borrow_mut().push("ce:pop".into());
    }

    fn html_constructor(
        &self,
        _cx: &mut Cx,
        _interface: InterfaceId,
        _new_target: &JsObject,
    ) -> Result<Handle> {
        Err(Error::Type("Illegal constructor".into()))
    }
}

impl rt::Reflect for Mock {
    fn get_content_attribute(&self, element: Handle, local_name: &str) -> Option<DomStr<'_>> {
        self.attr(element, local_name).map(DomStr::Owned)
    }

    fn set_content_attribute(
        &self,
        _cx: &mut Cx,
        element: Handle,
        local_name: &str,
        value: &str,
    ) -> Result<()> {
        let mut nodes = self.nodes.borrow_mut();
        let attrs = &mut nodes[element.get() as usize].attrs;
        match attrs.iter_mut().find(|(n, _)| n == local_name) {
            Some(a) => a.1 = value.into(),
            None => attrs.push((local_name.into(), value.into())),
        }
        Ok(())
    }

    fn remove_content_attribute(
        &self,
        _cx: &mut Cx,
        element: Handle,
        local_name: &str,
    ) -> Result<()> {
        self.nodes.borrow_mut()[element.get() as usize]
            .attrs
            .retain(|(n, _)| n != local_name);
        Ok(())
    }

    fn resolve_url(&self, _element: Handle, url: &str) -> Option<String> {
        Some(format!("https://example.test/{url}"))
    }

    fn token_list(&self, _element: Handle, _local_name: &str) -> Handle {
        unimplemented!()
    }
}

impl rt::EventHandlers for Mock {
    fn get_event_handler(
        &self,
        _cx: &mut Cx,
        this: Handle,
        event: &'static str,
    ) -> Result<Option<JsObject>> {
        Ok(self
            .handlers
            .borrow()
            .iter()
            .find(|(h, e, _)| *h == this && *e == event)
            .map(|(_, _, f)| f.clone()))
    }

    fn set_event_handler(
        &self,
        _cx: &mut Cx,
        this: Handle,
        event: &'static str,
        value: Option<JsObject>,
    ) -> Result<()> {
        self.log.borrow_mut().push(format!("handler:{event}"));
        let mut handlers = self.handlers.borrow_mut();
        handlers.retain(|(h, e, _)| !(*h == this && *e == event));
        if let Some(f) = value {
            handlers.push((this, event, f));
        }
        Ok(())
    }
}

impl NodeImpl for Mock {
    fn node_type(&self, _this: Handle) -> u16 {
        if let Some(n) = self.node_type_panics_after.get() {
            if n == 0 {
                panic!("nodeType failed");
            }
            self.node_type_panics_after.set(Some(n - 1));
        }
        1
    }

    fn node_name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.nodes.borrow()[this.get() as usize]
                .local_name
                .to_ascii_uppercase(),
        )
    }
}

impl ElementImpl for Mock {
    fn tag_name(&self, this: Handle) -> DomStr<'_> {
        self.node_name(this)
    }

    fn set_attribute(
        &self,
        cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
        value: TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString,
    ) -> Result<()> {
        if qualified_name.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(Error::dom(
                ExceptionName::InvalidCharacterError,
                "The qualified name is not valid",
            ));
        }
        let TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString::String(v) = value else {
            return Err(Error::Type(
                "trusted types are not supported by the mock".into(),
            ));
        };
        rt::Reflect::set_content_attribute(self, cx, this, qualified_name, &v)
    }

    fn scroll_options(
        &self,
        cx: &mut Cx,
        _this: Handle,
        options: ScrollToOptions,
    ) -> Result<JsPromise> {
        self.log.borrow_mut().push(format!(
            "scroll(options left={:?} top={:?} behavior={})",
            options.left,
            options.top,
            options.behavior.as_str()
        ));
        JsPromise::new(cx).ok_or(Error::Pending)
    }

    fn scroll_x_y(&self, cx: &mut Cx, _this: Handle, x: f64, y: f64) -> Result<JsPromise> {
        self.log.borrow_mut().push(format!("scroll(x={x} y={y})"));
        JsPromise::new(cx).ok_or(Error::Pending)
    }
}

impl HTMLCollectionImpl for Mock {
    fn length(&self, this: Handle) -> u32 {
        self.items(this).len() as u32
    }

    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.items(this).get(index as usize).copied()
    }

    fn named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        self.items(this)
            .into_iter()
            .find(|h| self.attr(*h, "id").as_deref() == Some(name))
    }

    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        self.items(this)
            .into_iter()
            .filter_map(|h| self.attr(h, "id"))
            .collect()
    }
}

impl NodeListImpl for Mock {
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.items(this).get(index as usize).copied()
    }

    fn length(&self, this: Handle) -> u32 {
        self.items(this).len() as u32
    }
}

impl HTMLOrSVGOrMathMLElementImpl for Mock {
    fn dataset(&self, this: Handle) -> Handle {
        self.log.borrow_mut().push("dataset".into());
        self.add(MockNode {
            owner: Some(this),
            ..MockNode::new(InterfaceId::DOMStringMap)
        })
    }
}

impl DOMStringMapImpl for Mock {
    fn named_getter(&self, this: Handle, name: &str) -> Option<DomStr<'_>> {
        self.attr(self.owner(this), &format!("data-{name}"))
            .map(DomStr::Owned)
    }

    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        let el = self.owner(this);
        self.nodes.borrow()[el.get() as usize]
            .attrs
            .iter()
            .filter_map(|(n, _)| n.strip_prefix("data-").map(str::to_string))
            .collect()
    }

    fn named_setter(&self, cx: &mut Cx, this: Handle, name: &str, value: &str) -> Result<()> {
        let el = self.owner(this);
        rt::Reflect::set_content_attribute(self, cx, el, &format!("data-{name}"), value)
    }

    fn named_deleter(&self, cx: &mut Cx, this: Handle, name: &str) -> Result<()> {
        let el = self.owner(this);
        rt::Reflect::remove_content_attribute(self, cx, el, &format!("data-{name}"))
    }
}

impl EventTargetImpl for Mock {
    fn add_event_listener(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        callback: Option<EventListener>,
        _options: AddEventListenerOptionsOrBoolean,
    ) -> Result<()> {
        if let Some(cb) = callback {
            self.listeners.borrow_mut().push((this, type_.into(), cb));
        }
        Ok(())
    }

    /// Calls the matching listeners with `this` as the callback this value; a listener's exception
    /// propagates (a real engine reports it instead).
    fn dispatch_event(&self, cx: &mut Cx, this: Handle, event: Handle) -> Result<bool> {
        let ty = EventImpl::type_(self, event).into_owned();
        let matching: Vec<EventListener> = self
            .listeners
            .borrow()
            .iter()
            .filter(|(t, ty2, _)| *t == this && *ty2 == ty)
            .map(|(_, _, cb)| cb.clone())
            .collect();
        for cb in matching {
            cb.handle_event(cx, Some(&this), event)?;
        }
        Ok(true)
    }
}

impl EventImpl for Mock {
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.nodes.borrow()[this.get() as usize].local_name.clone())
    }

    fn bubbles(&self, this: Handle) -> bool {
        self.attr(this, "bubbles").is_some()
    }

    fn is_trusted(&self, _this: Handle) -> bool {
        false
    }

    fn constructor(&self, _cx: &mut Cx, type_: &str, init: EventInit) -> Result<Handle> {
        let mut node = MockNode::new(InterfaceId::Event);
        node.local_name = type_.into();
        if init.bubbles {
            node.attrs.push(("bubbles".into(), String::new()));
        }
        Ok(self.add(node))
    }
}

impl DOMExceptionImpl for Mock {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.exceptions.borrow()[(this.get() - EXCEPTION_BASE) as usize]
                .1
                .clone(),
        )
    }

    fn message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.exceptions.borrow()[(this.get() - EXCEPTION_BASE) as usize]
                .0
                .clone(),
        )
    }

    fn code(&self, this: Handle) -> u16 {
        ExceptionName::legacy_code(&DOMExceptionImpl::name(self, this))
    }

    fn constructor(&self, _cx: &mut Cx, message: &str, name: &str) -> Result<Handle> {
        let mut e = self.exceptions.borrow_mut();
        e.push((message.into(), name.into()));
        Ok(Handle::new(EXCEPTION_BASE + e.len() as u64 - 1))
    }
}

macro_rules! stub {
    (NodeImpl) => {};
    (ElementImpl) => {};
    (DOMExceptionImpl) => {};
    (HTMLCollectionImpl) => {};
    (NodeListImpl) => {};
    (HTMLOrSVGOrMathMLElementImpl) => {};
    (DOMStringMapImpl) => {};
    (EventImpl) => {};
    (EventTargetImpl) => {};
    (WindowImpl) => {
        // The global's named properties: none.
        impl WindowImpl for Mock {
            fn named_getter(&self, _this: Handle, _name: &str) -> Option<Handle> {
                None
            }
            fn supported_property_names(&self, _this: Handle) -> Vec<String> {
                Vec::new()
            }
        }
    };
    ($t:ident) => {
        impl $t for Mock {}
    };
}
snowlark_bindings::for_each_trait!(stub);

/// Evaluates `src`, returning the completion value as a string or the exception's.
pub fn eval(scope: &mut v8::PinScope, src: &str) -> std::result::Result<String, String> {
    v8::tc_scope!(let tc, scope);
    let code = v8::String::new(tc, src).unwrap();
    match v8::Script::compile(tc, code, None).and_then(|s| s.run(tc)) {
        Some(v) => Ok(v.to_rust_string_lossy(tc)),
        None => Err(tc
            .exception()
            .map(|e| e.to_rust_string_lossy(tc))
            .unwrap_or_else(|| "unknown".into())),
    }
}
