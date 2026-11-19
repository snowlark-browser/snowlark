// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shadow roots (DOM §4.8, §4.9 "attach a shadow root"). Slots and slot assignment come later.

use html5ever::ns;

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::names;
use crate::node::{
    CustomElementState, NodeData, NodeId, ShadowRootData, ShadowRootMode, SlotAssignmentMode,
};

/// `ShadowRootInit` (DOM §4.9).
#[derive(Clone, Copy, Debug)]
pub struct ShadowRootInit {
    pub mode: ShadowRootMode,
    pub delegates_focus: bool,
    pub slot_assignment: SlotAssignmentMode,
    pub clonable: bool,
    pub serializable: bool,
}

impl ShadowRootInit {
    /// An init dictionary with `mode` and every other member at its default.
    pub fn new(mode: ShadowRootMode) -> ShadowRootInit {
        ShadowRootInit {
            mode,
            delegates_focus: false,
            slot_assignment: SlotAssignmentMode::Named,
            clonable: false,
            serializable: false,
        }
    }
}

impl Dom {
    /// `Element.attachShadow(init)`: DOM §4.9 "attach a shadow root". The custom element definition's
    /// "disable shadow" check (step 3) belongs to the custom elements layer.
    pub fn attach_shadow(&mut self, element: NodeId, init: ShadowRootInit) -> DomResult<NodeId> {
        let e = self.el(element);
        // 1-2.
        if e.name.ns != ns!(html) || !names::is_valid_shadow_host_name(&e.name.local) {
            return Err(DomError::NotSupported);
        }
        // 4. An existing declarative root of the same mode is emptied and reused.
        if let Some(current) = e.shadow_root {
            let data = self.shadow_root_data(current).expect("shadow root");
            if !data.declarative || data.mode != init.mode {
                return Err(DomError::NotSupported);
            }
            let children: Vec<NodeId> = self.children(current).collect();
            for c in children {
                self.remove(c, false);
            }
            if let NodeData::ShadowRoot(s) = &mut self.n_mut(current).data {
                s.declarative = false;
            }
            return Ok(current);
        }
        // 5-12.
        Ok(self.create_shadow_root(element, init))
    }

    /// The shadow root of an SVG `use` element (SVG 2 §5.6): closed, and made here rather than by
    /// `attachShadow`, which `use` does not allow.
    pub fn use_shadow_root(&mut self, element: NodeId) -> NodeId {
        match self.el(element).shadow_root {
            Some(root) => root,
            None => self.create_shadow_root(element, ShadowRootInit::new(ShadowRootMode::Closed)),
        }
    }

    /// Steps 5-12 of "attach a shadow root", without the checks.
    pub(crate) fn create_shadow_root(&mut self, element: NodeId, init: ShadowRootInit) -> NodeId {
        let document = self.node_document(element);
        self.shadow_roots_created += 1;
        let available = matches!(
            self.el(element).custom_state,
            CustomElementState::Precustomized | CustomElementState::Custom
        );
        let shadow = self.alloc(
            Some(document),
            NodeData::ShadowRoot(Box::new(ShadowRootData {
                host: element,
                mode: init.mode,
                delegates_focus: init.delegates_focus,
                slot_assignment: init.slot_assignment,
                clonable: init.clonable,
                serializable: init.serializable,
                declarative: false,
                available_to_element_internals: available,
            })),
        );
        if self.is_connected(element) {
            self.set_connected(shadow, true);
        }
        self.element_mut(element).shadow_root = Some(shadow);
        self.bump_tree(element);
        shadow
    }

    /// `Element.shadowRoot`: the shadow root if it is open.
    pub fn open_shadow_root(&self, element: NodeId) -> Option<NodeId> {
        self.shadow_root(element).filter(|&s| {
            self.shadow_root_data(s)
                .is_some_and(|d| d.mode == ShadowRootMode::Open)
        })
    }

    pub(crate) fn set_shadow_root_declarative(&mut self, shadow: NodeId, declarative: bool) {
        if let NodeData::ShadowRoot(s) = &mut self.n_mut(shadow).data {
            s.declarative = declarative;
        }
    }

    pub(crate) fn set_shadow_root_available_to_internals(&mut self, shadow: NodeId) {
        if let NodeData::ShadowRoot(s) = &mut self.n_mut(shadow).data {
            s.available_to_element_internals = true;
        }
    }
}
