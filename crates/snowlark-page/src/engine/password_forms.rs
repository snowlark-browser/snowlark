// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page's half of the browser's password manager (`docs/password-manager.md`): it reports the
//! top-level document's login forms ([`PasswordEvent`]) and fills what the browser sends
//! ([`PasswordFill`]), the values hidden from script until the user activates the page. Off unless
//! the embedder turns it on ([`Engine::set_password_forms`]): nothing here runs in the headless
//! modes.

use snowlark_dom::{local_name, NodeId};

use super::{node_handle, Engine};
use crate::event_loop::TaskSource;
use crate::passwords::{FieldData, FormData, PasswordEvent, PasswordFill};

/// The page's password manager state.
#[derive(Default)]
pub(crate) struct PasswordForms {
    pub on: bool,
    pub events: Vec<PasswordEvent>,
    /// Fields holding a suggested value until the next user activation.
    pub gated: Vec<NodeId>,
    /// The login form the user last typed in, for a sign-in by script.
    typed: Option<Typed>,
    /// The top-level document whose `load` reported its login forms, and the forms reported
    /// since (each by its first password field).
    loaded: Option<NodeId>,
    reported: Vec<u64>,
    /// An `input` was inserted into it since: look for new login forms after layout.
    inserted: bool,
}

/// What names a reported form: its first password field.
fn form_key(form: &FormData) -> u64 {
    form.fields
        .iter()
        .find(|f| f.kind == "password")
        .map_or(0, |f| f.id)
}

/// The login form (or the fields outside any form) of the top-level document the user last
/// typed in, with its values as they were then: what a page that signs in by script (a request,
/// then a new page or the form taken away) submitted.
struct Typed {
    doc: NodeId,
    data: FormData,
    /// A request of the page succeeded, or the document changed its URL in place, since the
    /// user typed: from now on the form leaving the rendering means it was submitted.
    armed: bool,
}

/// The input types whose fields the password manager looks at.
fn text_like(kind: &str) -> bool {
    matches!(
        kind,
        "text" | "password" | "email" | "tel" | "url" | "search" | "number"
    )
}

impl Engine {
    /// Turns the page's password manager half on (the window's tabs) or off (the default).
    pub fn set_password_forms(&self, on: bool) {
        self.password_forms.borrow_mut().on = on;
    }

    fn password_forms_on(&self) -> bool {
        self.password_forms.borrow().on
    }

    /// What the page reported since the last call.
    pub fn take_password_events(&self) -> Vec<PasswordEvent> {
        std::mem::take(&mut self.password_forms.borrow_mut().events)
    }

    /// The login forms of `doc` (one per `form` element, and one for the fields outside any), with
    /// their values when `values`. `only`: just that form.
    pub(crate) fn password_form_data(
        &self,
        doc: NodeId,
        only: Option<NodeId>,
        values: bool,
    ) -> Vec<FormData> {
        let inputs: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(doc)
                .filter(|&n| dom.is_html_element_named(n, &local_name!("input")))
                .collect()
        };
        let mut forms: Vec<FormData> = Vec::new();
        for el in inputs {
            let kind = self.input_type(el).unwrap_or_default();
            if !text_like(&kind) {
                continue;
            }
            let owner = self.form_owner_of(el);
            if only.is_some() && owner != only {
                continue;
            }
            let form_bits = owner.map_or(0, NodeId::to_bits);
            let (name, id_attr, autocomplete, readonly, disabled) = {
                let dom = self.dom();
                let a = |n: &str| dom.get_attribute(el, n).unwrap_or("").to_owned();
                (
                    a("name"),
                    a("id"),
                    a("autocomplete"),
                    dom.get_attribute(el, "readonly").is_some(),
                    super::html::is_disabled(&dom, el),
                )
            };
            let field = FieldData {
                id: el.to_bits(),
                kind,
                name,
                id_attr,
                autocomplete,
                value: if values {
                    self.control_value(el)
                } else {
                    String::new()
                },
                user_typed: self.ctl_read(el, |c| c.user_edited),
                focusable: !disabled && self.being_rendered(el),
                readonly,
            };
            match forms.iter_mut().find(|f| f.form == form_bits) {
                Some(f) => f.fields.push(field),
                None => {
                    let action = owner
                        .and_then(|f| self.dom().get_attribute(f, "action").map(str::to_owned))
                        .unwrap_or_default();
                    forms.push(FormData {
                        form: form_bits,
                        action,
                        fields: vec![field],
                    });
                }
            }
        }
        forms
    }

    /// `form` is submitting (HTML "submit", past its `submit` event): report it when it is in the
    /// top-level document and has a password field.
    pub(crate) fn password_form_submitted(&self, form: NodeId) {
        if !self.password_forms_on() {
            return;
        }
        let doc = self.dom().node_document(form);
        if self.document() != Some(doc) {
            return;
        }
        if let Some(data) = self
            .password_form_data(doc, Some(form), true)
            .into_iter()
            .find(FormData::has_password_field)
        {
            let mut state = self.password_forms.borrow_mut();
            state.typed = None;
            state.events.push(PasswordEvent::Submitted(data));
        }
    }

    /// The user changed the text of `el` (or a fill became the field's value): when its form
    /// in the top-level document has a password field, that form is the one a sign-in by script
    /// would submit, with its values now. Typing in another form forgets it.
    pub(crate) fn password_field_edited(&self, el: NodeId) {
        if !self.password_forms_on() {
            return;
        }
        let doc = self.dom().node_document(el);
        if self.document() != Some(doc)
            || !self.dom().is_html_element_named(el, &local_name!("input"))
            || !text_like(&self.input_type(el).unwrap_or_default())
        {
            return;
        }
        let form = self.form_owner_of(el).map_or(0, NodeId::to_bits);
        let data = self
            .password_form_data(doc, None, true)
            .into_iter()
            .find(|f| f.form == form)
            .filter(FormData::has_password_field);
        self.password_forms.borrow_mut().typed = data.map(|data| Typed {
            doc,
            data,
            armed: false,
        });
    }

    /// Script of `doc` began a navigation of the top-level document to another document (not a
    /// link the user followed, not a form submission, which reports itself): the login form the
    /// user typed in, if any, was probably submitted. The next page judges it.
    pub(crate) fn password_probable_navigation(&self, doc: NodeId) {
        if !self.password_forms_on() || self.document() != Some(doc) {
            return;
        }
        let mut state = self.password_forms.borrow_mut();
        if let Some(t) = state.typed.take().filter(|t| t.doc == doc) {
            state.events.push(PasswordEvent::Submitted(t.data));
        }
    }

    /// One of the page's requests (`fetch`, `XMLHttpRequest`) succeeded, or the top-level
    /// document `doc` changed its URL in place (`pushState`, a fragment): if the login form the
    /// user typed in leaves the rendering from now on, it was submitted. Checked after layout
    /// ([`Engine::password_forms_after_layout`]).
    pub(crate) fn password_submission_may_follow(&self, doc: Option<NodeId>) {
        if !self.password_forms_on() {
            return;
        }
        let top = self.document();
        if doc.is_some() && doc != top {
            return;
        }
        if let Some(t) = self.password_forms.borrow_mut().typed.as_mut() {
            if Some(t.doc) == top {
                t.armed = true;
            }
        }
    }

    /// After layout: an armed login form that no longer has a field in the rendering (removed,
    /// hidden, disabled) was submitted by script without a navigation. A form of a document that
    /// is no longer the top-level one is forgotten.
    pub(crate) fn password_forms_after_layout(&self) {
        if !self.password_forms_on() {
            return;
        }
        self.password_forms_added();
        let (doc, ids) = {
            let state = self.password_forms.borrow();
            let Some(t) = state.typed.as_ref().filter(|t| t.armed) else {
                return;
            };
            let ids: Vec<u64> = t.data.fields.iter().map(|f| f.id).collect();
            (t.doc, ids)
        };
        if self.document() != Some(doc) {
            self.password_forms.borrow_mut().typed = None;
            return;
        }
        let shown = ids.into_iter().filter_map(NodeId::from_bits).any(|el| {
            let present = {
                let dom = self.dom();
                dom.is_alive(el) && dom.is_connected(el) && dom.node_document(el) == doc
            };
            present && !super::html::is_disabled(&self.dom(), el) && self.being_rendered(el)
        });
        if shown {
            return;
        }
        let mut state = self.password_forms.borrow_mut();
        if let Some(t) = state.typed.take() {
            state.events.push(PasswordEvent::SubmittedInPlace(t.data));
        }
    }

    /// `doc` fired `load`: report the top-level document's login forms (none is news too).
    pub(crate) fn password_forms_loaded(&self, doc: NodeId) {
        if !self.password_forms_on() || self.document() != Some(doc) {
            return;
        }
        let forms: Vec<FormData> = self
            .password_form_data(doc, None, false)
            .into_iter()
            .filter(FormData::has_password_field)
            .collect();
        let mut state = self.password_forms.borrow_mut();
        state.loaded = Some(doc);
        state.reported = forms.iter().map(form_key).collect();
        state.inserted = false;
        state.events.push(PasswordEvent::Rendered(forms));
    }

    /// An `input` was connected to a document: after the top-level document reported its forms
    /// at `load`, login forms script adds are looked for after layout.
    pub(crate) fn password_input_inserted(&self, el: NodeId) {
        if !self.password_forms_on() {
            return;
        }
        let doc = self.dom().node_document(el);
        let mut state = self.password_forms.borrow_mut();
        if state.loaded == Some(doc) {
            state.inserted = true;
        }
    }

    /// After layout, once inputs were inserted after `load`: the top-level document's login forms
    /// not reported before, without values ([`PasswordEvent::Added`]).
    fn password_forms_added(&self) {
        let doc = {
            let mut state = self.password_forms.borrow_mut();
            if !std::mem::take(&mut state.inserted) {
                return;
            }
            match state.loaded {
                Some(doc) => doc,
                None => return,
            }
        };
        if self.document() != Some(doc) {
            return;
        }
        let forms: Vec<FormData> = {
            let state = self.password_forms.borrow();
            let reported = state.reported.clone();
            drop(state);
            self.password_form_data(doc, None, false)
                .into_iter()
                .filter(|f| f.has_password_field() && !reported.contains(&form_key(f)))
                .collect()
        };
        if forms.is_empty() {
            return;
        }
        let mut state = self.password_forms.borrow_mut();
        state.reported.extend(forms.iter().map(form_key));
        state.events.push(PasswordEvent::Added(forms));
    }

    /// Fills a saved login the browser sent: only into the top-level document when its origin is
    /// the one the browser decided for, only into text fields of that document (the password one
    /// of type password) the user has not typed in. The values stay suggestions, invisible to
    /// script, until the next user activation. Returns whether anything was filled.
    pub fn fill_password(&self, fill: &PasswordFill) -> bool {
        if !self.password_forms_on() {
            return false;
        }
        let Some(doc) = self.document() else {
            return false;
        };
        if self.origin_of_document(doc) != fill.origin {
            return false;
        }
        let field = |bits: u64, password: bool| -> Option<NodeId> {
            let el = NodeId::from_bits(bits)?;
            let dom = self.dom();
            if !dom.is_alive(el)
                || !dom.is_connected(el)
                || dom.node_document(el) != doc
                || !dom.is_html_element_named(el, &local_name!("input"))
            {
                return None;
            }
            drop(dom);
            let kind = self.input_type(el)?;
            let ok = if password {
                kind == "password"
            } else {
                text_like(&kind) && kind != "password"
            };
            (ok && !self.ctl_read(el, |c| c.user_edited)).then_some(el)
        };
        let Some(password) = field(fill.password_field, true) else {
            return false;
        };
        let username = fill.username_field.and_then(|u| field(u, false));
        let mut filled = Vec::new();
        if let Some(u) = username {
            if !fill.username.is_empty() && self.control_value(u) != fill.username {
                filled.push((u, fill.username.clone()));
            }
        }
        filled.push((password, fill.password.clone()));
        let mut state = self.password_forms.borrow_mut();
        for (el, value) in filled {
            self.ctl(el, |c| c.suggested = Some(value));
            if !state.gated.contains(&el) {
                state.gated.push(el);
            }
        }
        true
    }

    /// A user activation in `doc`: the suggested values of the top-level document become the
    /// fields' values, as if the user had typed them (`input` and `change` fire).
    pub(crate) fn password_fields_activated(&self, doc: NodeId) {
        if self.document() != Some(doc) || self.password_forms.borrow().gated.is_empty() {
            return;
        }
        let gated = std::mem::take(&mut self.password_forms.borrow_mut().gated);
        self.queue_task(TaskSource::UserInteraction, move |e, cx| {
            for el in gated {
                if !e.dom().is_alive(el) {
                    continue;
                }
                let Some(value) = e.ctl_read(el, |c| c.suggested.clone()) else {
                    continue;
                };
                e.set_edited_text(el, value);
                e.fire_event(cx, node_handle(el), "input", true, false);
                e.fire_event(cx, node_handle(el), "change", true, false);
            }
        });
    }
}
