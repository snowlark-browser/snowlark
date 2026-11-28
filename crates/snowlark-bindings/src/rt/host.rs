// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the engine provides besides the per-interface traits: the most derived interface of an object,
//! the custom element reactions stack, `[HTMLConstructor]`, and the content attribute primitives that
//! the generated reflection rules (HTML §2.6.1) are written over.

use super::*;
use crate::generated::InterfaceId;

/// Engine services the generated code needs that no single interface owns.
pub trait Host {
    /// The most derived interface `handle`'s object implements; decides which prototype its wrapper gets.
    fn interface_of(&self, handle: Handle) -> InterfaceId;

    /// `[CEReactions]`, before the member runs: push an element queue on the custom element reactions
    /// stack (HTML §4.13.6).
    fn push_ce_reactions(&self);

    /// `[CEReactions]`, after the member ran (also when it threw): pop the element queue and invoke its
    /// reactions.
    fn pop_ce_reactions(&self, cx: &mut Cx);

    /// `[HTMLConstructor]` (HTML §3.2.3): `new.target` is the constructor the script called; the engine
    /// looks it up in the custom element registry and returns the element, or throws.
    fn html_constructor(
        &self,
        cx: &mut Cx,
        interface: InterfaceId,
        new_target: &JsObject,
    ) -> Result<Handle>;

    /// The realm `handle`'s wrapper belongs in (its relevant realm), named by the handle of that
    /// realm's global object, or of an object of that realm (its wrapper's realm, or failing that its
    /// own `realm_of`); `None` for the realm of the code creating the wrapper, which is the object's
    /// whenever it is made by a binding of that realm or in a callback or task that entered it. Only
    /// consulted when [`register_realm`](super::register_realm) registered more than one realm.
    fn realm_of(&self, handle: Handle) -> Option<Handle> {
        let _ = handle;
        None
    }

    /// WindowProxy [[GetOwnProperty]] for an array index (HTML §7.2.3.3): the WindowProxy of the
    /// `index`th document-tree child navigable of `window`, if there is one.
    fn window_child(&self, window: Handle, index: u32) -> Option<Handle> {
        let _ = (window, index);
        None
    }

    /// `IsPlatformObjectSameOrigin` (HTML §7.2.1.3.2): whether code of the realm whose global object
    /// is `accessing` may touch `target`, a Window or a Location, as its own. Answers without running
    /// script, and with no when the engine cannot tell.
    fn may_access(&self, accessing: Handle, target: Handle) -> bool {
        let _ = (accessing, target);
        false
    }

    /// A number naming `object`, a Window or a Location, for as long as the engine knows it, and never
    /// anything else after that: what a cross-origin function holds of its object.
    fn cross_origin_token(&self, object: Handle) -> u64 {
        let _ = object;
        0
    }

    /// The object of a [`Host::cross_origin_token`], while it exists.
    fn cross_origin_object(&self, token: u64) -> Option<Handle> {
        let _ = token;
        None
    }

    /// The WindowProxy of the child of `window` whose target name is `name`, when `name` is in
    /// `window`'s document-tree child navigable target name property set (HTML §7.2.3.3).
    fn cross_origin_named_child(&self, window: Handle, name: &str) -> Option<Handle> {
        let _ = (window, name);
        None
    }

    /// When `handle` is a Window that a navigation replaced: the Window its browsing context shows
    /// now, if it still has one.
    fn active_window(&self, handle: Handle) -> Option<Option<Handle>> {
        let _ = handle;
        None
    }

    /// Whether `this`, a WebGL context or the object of one of its extensions, is lost (WebGL 1.0
    /// §5.14, §5.15.2: the context's webgl context lost flag is set, or the extension was disabled by
    /// a loss): its methods not marked `[WebGLHandlesContextLoss]` then return their type's default
    /// without running.
    fn webgl_context_lost(&self, this: Handle) -> bool {
        let _ = this;
        false
    }
}

/// The attribute primitives of an element, for reflected IDL attributes. Names are local names with a
/// null namespace, as reflection uses (HTML §2.6.1).
pub trait Reflect {
    /// Gets the value of the attribute, if the element has one.
    fn get_content_attribute(&self, element: Handle, local_name: &str) -> Option<DomStr<'_>>;

    /// Sets the attribute (as `setAttribute` would, for the null namespace).
    fn set_content_attribute(
        &self,
        cx: &mut Cx,
        element: Handle,
        local_name: &str,
        value: &str,
    ) -> Result<()>;

    /// Removes the attribute if present.
    fn remove_content_attribute(
        &self,
        cx: &mut Cx,
        element: Handle,
        local_name: &str,
    ) -> Result<()>;

    /// Parses a URL against the element's node document's base URL and serializes it (for
    /// `[ReflectURL]`); `None` when parsing fails.
    fn resolve_url(&self, element: Handle, url: &str) -> Option<String>;

    /// The `DOMTokenList` associated with the element's attribute (`relList`, `sandbox`, `sizes`...); the
    /// same list each time.
    fn token_list(&self, element: Handle, local_name: &str) -> Handle;
}

/// Event handler IDL attributes (HTML §8.1.8.1). Every `onfoo` attribute of every interface comes here
/// with its event type (`"foo"`), because they all share the same getter and setter steps; the engine
/// decides which object's event handler map `this` names (a `body` element's `onload` is the window's).
pub trait EventHandlers {
    /// The getter: the event handler's callback, after compiling an uncompiled one; `None` for null.
    fn get_event_handler(
        &self,
        cx: &mut Cx,
        this: Handle,
        event: &'static str,
    ) -> Result<Option<JsObject>>;

    /// The setter: `None` deactivates the handler; otherwise the value is the callback object
    /// (converted as a `[LegacyTreatNonObjectAsNull]` callback function).
    fn set_event_handler(
        &self,
        cx: &mut Cx,
        this: Handle,
        event: &'static str,
        value: Option<JsObject>,
    ) -> Result<()>;
}
