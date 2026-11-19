// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Permissions Policy for the features the engine gates: each document's inherited policy, fixed
//! when it becomes its navigable's active document ("create a permissions policy for a navigable"),
//! from its container's `allow` attribute ("process permissions policy attributes") and each
//! feature's default allowlist; and "allowed to use".

use rustc_hash::FxHashMap;
use snowlark_dom::{local_name, ns, NodeId};

use super::Engine;

/// A policy-controlled feature the engine checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Fullscreen API: default allowlist `'self'`; `allowfullscreen` on an iframe is `fullscreen *`.
    Fullscreen,
    /// Gamepad: default allowlist `*`.
    Gamepad,
    /// Keyboard Map: default allowlist `'self'`.
    KeyboardMap,
    /// Battery Status: default allowlist `'self'`.
    Battery,
}

impl Feature {
    const ALL: [Feature; 4] = [
        Feature::Fullscreen,
        Feature::Gamepad,
        Feature::KeyboardMap,
        Feature::Battery,
    ];

    /// The feature's token in an `allow` attribute.
    fn token(self) -> &'static str {
        match self {
            Feature::Fullscreen => "fullscreen",
            Feature::Gamepad => "gamepad",
            Feature::KeyboardMap => "keyboard-map",
            Feature::Battery => "battery",
        }
    }

    /// The default allowlist is `*` (otherwise `'self'`).
    fn default_all(self) -> bool {
        matches!(self, Feature::Gamepad)
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Each document's inherited policy: a bit per enabled feature.
#[derive(Default)]
pub struct PolicyState {
    enabled: FxHashMap<NodeId, u8>,
}

impl Engine {
    /// Fixes the inherited policy of `doc`, the new active document of its navigable, for every
    /// feature ("define an inherited policy for feature in container at origin").
    pub(crate) fn define_inherited_policy(&self, doc: NodeId) {
        let bits = match self.container_of_document(doc) {
            None => u8::MAX,
            Some(container) if !self.dom().is_alive(container) => 0,
            Some(container) => {
                let parent = self.dom().node_document(container);
                let parent_bits = self.policy_bits(parent);
                let origin = self.origin_of_document(doc);
                Feature::ALL
                    .iter()
                    .filter(|f| {
                        parent_bits & f.bit() != 0
                            && self.container_allows(**f, container, parent, &origin)
                    })
                    .fold(0, |acc, f| acc | f.bit())
            }
        };
        self.permissions_policy
            .borrow_mut()
            .enabled
            .insert(doc, bits);
    }

    /// The inherited policy of `doc`: as fixed, or, for a document never given one, enabled
    /// for a top-level document only.
    fn policy_bits(&self, doc: NodeId) -> u8 {
        let known = self.permissions_policy.borrow().enabled.get(&doc).copied();
        known.unwrap_or_else(|| {
            if self.container_of_document(doc).is_none() {
                u8::MAX
            } else {
                0
            }
        })
    }

    /// "Allowed to use": `doc` is fully active and its policy enables `feature` for its origin.
    pub(crate) fn allowed_to_use(&self, doc: NodeId, feature: Feature) -> bool {
        self.is_fully_active(doc) && self.policy_bits(doc) & feature.bit() != 0
    }

    /// Steps 4-8 of "define an inherited policy": the container policy (the `allow` attribute,
    /// then `allowfullscreen` as `fullscreen *`), else the feature's default allowlist.
    fn container_allows(
        &self,
        feature: Feature,
        container: NodeId,
        parent: NodeId,
        origin: &str,
    ) -> bool {
        let dom = self.dom();
        let parent_origin = self.origin_of_document(parent);
        // ("the declared origin": the `src` URL's origin; without one, or for an `about:` URL,
        // the parent's. A `src` whose origin is opaque, a `data:` URL, matches the opaque origin
        // of the document it brings.)
        let src_matches = || {
            let src = dom
                .attr(container, &ns!(), &local_name!("src"))
                .and_then(|s| url::Url::parse(dom.document_url(parent)).ok()?.join(s).ok())
                .filter(|u| u.scheme() != "about");
            match src {
                Some(u) if matches!(u.scheme(), "http" | "https") => {
                    origin == u.origin().ascii_serialization()
                }
                Some(u) if !u.origin().is_tuple() => origin.starts_with("opaque:"),
                _ => origin == parent_origin,
            }
        };
        if let Some(allow) = dom.attr(container, &ns!(), &local_name!("allow")) {
            for directive in allow.split(';') {
                let mut tokens = directive.split_ascii_whitespace();
                if tokens.next() != Some(feature.token()) {
                    continue;
                }
                let list: Vec<&str> = tokens.collect();
                // (an empty allowlist in an `allow` attribute is `'src'`)
                let list = if list.is_empty() { vec!["'src'"] } else { list };
                return list.iter().any(|item| match *item {
                    "*" => true,
                    "'none'" => false,
                    "'self'" => origin == parent_origin,
                    "'src'" => src_matches(),
                    url => url::Url::parse(url)
                        .is_ok_and(|u| u.origin().ascii_serialization() == origin),
                });
            }
        }
        if feature == Feature::Fullscreen
            && dom.is_html_element_named(container, &local_name!("iframe"))
            && dom
                .attr(container, &ns!(), &local_name!("allowfullscreen"))
                .is_some()
        {
            return true;
        }
        feature.default_all() || origin == parent_origin
    }

    /// Documents that are gone lose their policies.
    pub(crate) fn policy_forget(&self, docs: &[NodeId]) {
        self.permissions_policy
            .borrow_mut()
            .enabled
            .retain(|d, _| !docs.contains(d));
    }
}
