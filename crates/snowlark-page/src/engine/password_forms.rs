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
            self.password_forms
                .borrow_mut()
                .events
                .push(PasswordEvent::Submitted(data));
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
        self.password_forms
            .borrow_mut()
            .events
            .push(PasswordEvent::Rendered(forms));
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
