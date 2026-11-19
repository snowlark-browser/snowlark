// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `ElementInternals` (HTML §4.13.7.?, "element internals") and `CustomStateSet` (the custom states
//! `:state()` matches): what `attachInternals()` gives a custom element. Its ARIA members are kept on
//! the internals object, not as content attributes.

use std::collections::HashMap;

use snowlark_bindings::generated::{
    CustomStateSetImpl, ElementInternalsImpl, FileOrStringOrFormData, ValidityStateFlags,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_dom::{ns, CustomElementState, NodeId};

use super::collections::Filter;
use super::{exception, node_handle, Engine, Object};

/// A form-associated custom element's submission value: a string, a File, or a list of entries.
#[derive(Clone)]
pub enum SubmissionValue {
    Str(String),
    File(Handle),
    Entries(Vec<(String, super::fetch::FormValue)>),
}

/// An ElementInternals object's state.
#[derive(Default)]
pub struct InternalsData {
    pub element: Option<NodeId>,
    /// ARIA attributes set through the internals (role, ariaLabel...), by content attribute name.
    pub aria: HashMap<String, String>,
    /// ARIA element references set through the internals.
    pub refs: HashMap<&'static str, Vec<NodeId>>,
    pub states: Vec<String>,
    pub states_object: Option<Handle>,
    pub flags: Option<ValidityFlags>,
    pub validation_message: String,
    /// The element's submission value (HTML §4.13.7.4 `setFormValue`).
    pub form_value: Option<SubmissionValue>,
}

/// The validity flags `setValidity` set.
#[derive(Clone, Copy, Default)]
pub struct ValidityFlags {
    pub value_missing: bool,
    pub type_mismatch: bool,
    pub pattern_mismatch: bool,
    pub too_long: bool,
    pub too_short: bool,
    pub range_underflow: bool,
    pub range_overflow: bool,
    pub step_mismatch: bool,
    pub bad_input: bool,
    pub custom_error: bool,
}

impl ValidityFlags {
    fn any(&self) -> bool {
        self.value_missing
            || self.type_mismatch
            || self.pattern_mismatch
            || self.too_long
            || self.too_short
            || self.range_underflow
            || self.range_overflow
            || self.step_mismatch
            || self.bad_input
            || self.custom_error
    }
}

impl Engine {
    pub(crate) fn internals(&self, h: Handle) -> std::cell::RefMut<'_, InternalsData> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::ElementInternals(i) => &mut **i,
            _ => panic!("not an ElementInternals"),
        })
    }

    /// Whether a handle names an ElementInternals object (for the ARIA members they share with
    /// elements).
    pub(crate) fn is_internals(&self, h: Handle) -> bool {
        super::handle_node(h).is_none() && matches!(&*self.obj(h), Object::ElementInternals(_))
    }

    /// The attached internals of an element, if any.
    pub(crate) fn internals_of(&self, el: NodeId) -> Option<Handle> {
        self.html.borrow().internals.get(&el).copied()
    }

    /// A form-associated custom element's submission value.
    pub(crate) fn submission_value(&self, el: NodeId) -> Option<SubmissionValue> {
        self.internals_of(el)
            .and_then(|h| self.internals(h).form_value.clone())
    }

    /// The custom states of an element (what `:state(name)` matches).
    pub(crate) fn has_custom_state(&self, el: NodeId, name: &str) -> bool {
        self.internals_of(el)
            .is_some_and(|h| self.internals(h).states.iter().any(|s| s == name))
    }

    /// The validity flags `setValidity` gave an element's internals.
    pub(crate) fn internals_flags(&self, el: NodeId) -> Option<ValidityFlags> {
        self.internals_of(el).and_then(|h| self.internals(h).flags)
    }

    /// Whether a custom element's definition says it is form-associated.
    pub(crate) fn is_form_associated_custom(&self, el: NodeId) -> bool {
        self.custom
            .borrow()
            .element_definition
            .get(&el)
            .is_some_and(|d| d.form_associated)
    }

    fn internals_target(&self, h: Handle) -> NodeId {
        self.internals(h)
            .element
            .expect("internals have an element")
    }

    /// HTML `attachInternals()`.
    pub(crate) fn attach_internals_steps(&self, el: NodeId) -> rt::Result<Handle> {
        let not_supported = |m: &'static str| Err(exception(ExceptionName::NotSupportedError, m));
        // 1.
        if self.dom().is_value(el).is_some() {
            return not_supported("Customized built-in elements cannot have internals");
        }
        // 2-3.
        let def = self.custom.borrow().element_definition.get(&el).cloned();
        let def = match def {
            Some(d) => Some(d),
            None => {
                let (doc, local) = {
                    let dom = self.dom();
                    (dom.node_document(el), dom.local_name(el).to_string())
                };
                self.look_up_definition(doc, &ns!(html), &local, None)
            }
        };
        let Some(def) = def else {
            return not_supported("The element is not a custom element");
        };
        if def.disable_internals {
            return not_supported("Internals are disabled for this custom element");
        }
        // 4.
        if self.internals_of(el).is_some() {
            return not_supported("The internals were attached already");
        }
        // 5.
        if !matches!(
            self.dom().custom_element_state(el),
            CustomElementState::Precustomized | CustomElementState::Custom
        ) {
            return not_supported("The element is not being constructed or upgraded");
        }
        let h = self.alloc(Object::ElementInternals(Box::new(InternalsData {
            element: Some(el),
            ..Default::default()
        })));
        self.html.borrow_mut().internals.insert(el, h);
        Ok(h)
    }

    /// An ARIA string member read through internals.
    pub(crate) fn internals_aria(&self, h: Handle, name: &str) -> Option<DomStr<'_>> {
        self.internals(h).aria.get(name).cloned().map(DomStr::Owned)
    }

    pub(crate) fn set_internals_aria(&self, h: Handle, name: &str, value: Option<&str>) {
        let mut i = self.internals(h);
        match value {
            Some(v) => {
                i.aria.insert(name.to_owned(), v.to_owned());
            }
            None => {
                i.aria.remove(name);
            }
        }
    }

    pub(crate) fn internals_refs(&self, h: Handle, attr: &'static str) -> Option<Vec<NodeId>> {
        self.internals(h).refs.get(attr).cloned()
    }

    pub(crate) fn set_internals_refs(&self, h: Handle, attr: &'static str, v: Option<Vec<NodeId>>) {
        let mut i = self.internals(h);
        match v {
            Some(v) => {
                i.refs.insert(attr, v);
            }
            None => {
                i.refs.remove(attr);
            }
        }
    }

    fn require_form_associated(&self, h: Handle) -> rt::Result<NodeId> {
        let el = self.internals_target(h);
        if !self.is_form_associated_custom(el) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "The element is not a form-associated custom element",
            ));
        }
        Ok(el)
    }
}

impl ElementInternalsImpl for Engine {
    fn shadow_root(&self, this: Handle) -> Option<Handle> {
        let el = self.internals_target(this);
        let dom = self.dom();
        let sr = dom.shadow_root(el)?;
        // Only a shadow root "available to element internals" (made by the element's constructor or
        // declaratively) is visible; every one is, here.
        Some(node_handle(sr))
    }
    fn form(&self, this: Handle) -> Option<Handle> {
        let el = self.internals_target(this);
        if !self.is_form_associated_custom(el) {
            return None;
        }
        self.form_owner_of(el).map(node_handle)
    }
    fn will_validate(&self, this: Handle) -> bool {
        let el = self.internals_target(this);
        self.is_form_associated_custom(el) && !super::html::is_disabled(&self.dom(), el)
    }
    fn validity(&self, this: Handle) -> Handle {
        let el = self.internals_target(this);
        self.alloc(Object::ValidityState { element: el })
    }
    fn validation_message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.internals(this).validation_message.clone())
    }
    fn labels(&self, this: Handle) -> Handle {
        let el = self.internals_target(this);
        self.collection(el, Filter::Labels(el))
    }
    fn states(&self, this: Handle) -> Handle {
        if let Some(h) = self.internals(this).states_object {
            return h;
        }
        let h = self.alloc(Object::CustomStateSet { internals: this });
        self.internals(this).states_object = Some(h);
        h
    }
    fn set_form_value(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: Option<FileOrStringOrFormData>,
        _state: Option<FileOrStringOrFormData>,
    ) -> rt::Result<()> {
        self.require_form_associated(this)?;
        // (a FormData's entry list is copied; a File stays the same object)
        let v = match value {
            Some(FileOrStringOrFormData::String(s)) => Some(SubmissionValue::Str(s)),
            Some(FileOrStringOrFormData::File(f)) => Some(SubmissionValue::File(f)),
            Some(FileOrStringOrFormData::FormData(f)) => {
                Some(SubmissionValue::Entries(self.form_data(f).clone()))
            }
            None => None,
        };
        self.internals(this).form_value = v;
        Ok(())
    }
    fn set_validity(
        &self,
        _cx: &mut Cx,
        this: Handle,
        flags: ValidityStateFlags,
        message: Option<&str>,
        _anchor: Option<Handle>,
    ) -> rt::Result<()> {
        self.require_form_associated(this)?;
        let f = ValidityFlags {
            value_missing: flags.value_missing,
            type_mismatch: flags.type_mismatch,
            pattern_mismatch: flags.pattern_mismatch,
            too_long: flags.too_long,
            too_short: flags.too_short,
            range_underflow: flags.range_underflow,
            range_overflow: flags.range_overflow,
            step_mismatch: flags.step_mismatch,
            bad_input: flags.bad_input,
            custom_error: flags.custom_error,
        };
        if f.any() && message.is_none_or(str::is_empty) {
            return Err(rt::Error::Type(
                "A validation message is required when a flag is set".into(),
            ));
        }
        let mut i = self.internals(this);
        i.flags = Some(f);
        i.validation_message = if f.any() {
            message.unwrap_or("").to_owned()
        } else {
            String::new()
        };
        Ok(())
    }
    fn check_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        let el = self.require_form_associated(this)?;
        let invalid = self.internals(this).flags.is_some_and(|f| f.any());
        if invalid {
            self.fire_event(cx, node_handle(el), "invalid", false, true);
        }
        Ok(!invalid)
    }
    fn report_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        ElementInternalsImpl::check_validity(self, cx, this)
    }
}

impl CustomStateSetImpl for Engine {
    fn setlike_at(&self, this: Handle, index: u32) -> Option<String> {
        let i = self.state_set_internals(this);
        self.internals(i).states.get(index as usize).cloned()
    }
    fn setlike_size(&self, this: Handle) -> u32 {
        let i = self.state_set_internals(this);
        self.internals(i).states.len() as u32
    }
    fn setlike_has(&self, this: Handle, value: &str) -> bool {
        let i = self.state_set_internals(this);
        self.internals(i).states.iter().any(|s| s == value)
    }
    fn setlike_add(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let i = self.state_set_internals(this);
        let added = {
            let mut d = self.internals(i);
            let added = !d.states.iter().any(|s| s == value);
            if added {
                d.states.push(value.to_owned());
            }
            added
        };
        if added {
            self.custom_states_changed(i);
        }
        Ok(())
    }
    fn setlike_delete(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<bool> {
        let i = self.state_set_internals(this);
        let removed = {
            let mut d = self.internals(i);
            let before = d.states.len();
            d.states.retain(|s| s != value);
            d.states.len() != before
        };
        if removed {
            self.custom_states_changed(i);
        }
        Ok(removed)
    }
    fn setlike_clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let i = self.state_set_internals(this);
        let had = {
            let mut d = self.internals(i);
            let had = !d.states.is_empty();
            d.states.clear();
            had
        };
        if had {
            self.custom_states_changed(i);
        }
        Ok(())
    }
}

impl Engine {
    /// The custom states of internals `i` changed: the DOM's copy (what `:state()` matches) follows,
    /// and the element's style.
    fn custom_states_changed(&self, i: Handle) {
        let (element, states) = {
            let d = self.internals(i);
            (d.element, d.states.clone())
        };
        let Some(el) = element else {
            return;
        };
        self.dom_mut().set_custom_states(el, states);
        self.with_node_renderer(el, |r, st| {
            r.custom_states_changed(el);
            st.dirty = true;
        });
    }

    fn state_set_internals(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::CustomStateSet { internals } => *internals,
            _ => panic!("not a CustomStateSet"),
        }
    }
}
