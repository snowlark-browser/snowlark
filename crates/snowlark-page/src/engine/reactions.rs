// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What happens after a DOM operation: the DOM's queued [`Reaction`]s become custom element reactions,
//! HTML's insertion/removal/attribute change/cloning steps (scripts, event handler content attributes,
//! slots), and mutation records reach their observers. Plus the engine's internal microtasks.

use snowlark_bindings::rt::{self, Cx};
use snowlark_dom::{
    local_name, ns, subtree, Attr, CustomElementState, Dom, ElementData, NodeId, Reaction,
    ReactionFilter,
};

use super::custom::CbArg;
use super::{node_handle, Engine};

/// The engine's own microtasks (queued with V8's microtask queue so they interleave with promise jobs
/// in the order HTML requires).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Microtask {
    /// HTML §4.13.6: invoke the reactions of the backup element queue.
    CustomElementBackupQueue = 1,
    /// DOM §4.3.2 "notify mutation observers".
    MutationObservers = 2,
    /// HTML §4.8.4.3.4 "update the image data" step 6's microtask.
    ImageData = 3,
    /// HTML §4.8.4.4 `decode()` step 2's microtask.
    ImageDecode = 4,
}

fn microtask_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let kind = args.data().int32_value(scope).unwrap_or(0);
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let mut cx = Cx::new(scope);
    match kind {
        1 => engine.process_backup_queue(&mut cx),
        2 => engine.notify_mutation_observers(&mut cx),
        3 => engine.img_microtask(),
        4 => engine.img_decode_microtask(&mut cx),
        _ => {}
    }
}

/// The reactions the engine needs besides those for the nodes a mutation is about: the descendant
/// elements that have insertion, removing or adopting steps of their own (custom elements, scripts,
/// frames, `link`, and the renderer's), and the attributes of new elements that have attribute
/// change steps (event handler content attributes; custom elements). What else a subtree holds that
/// matters (wrappers, the focused element, slots, live ranges) the engine finds from the reported
/// node.
pub(crate) const REACTION_FILTER: ReactionFilter = ReactionFilter {
    descendant: descendant_bits,
    new_attribute: wants_new_attribute_reaction,
};

/// The [`REACTION_FILTER`]'s answer for a descendant element: [`subtree::REPORT`] for the elements
/// with insertion, removing or adopting steps of their own (custom elements, scripts, frames,
/// `link`, media, and the renderer's: see `snowlark_render::renderer::wants_descendant_reaction`),
/// and [`super::forms::SUBTREE_FORM_STATE`] for those with a form state (or, an undefined custom
/// element, that may come to have one). One match on the name: it runs for every element of every
/// inserted and removed subtree.
fn descendant_bits(e: &ElementData) -> u8 {
    use super::forms::SUBTREE_FORM_STATE as FORM;
    const REPORT: u8 = subtree::REPORT;
    let face = if e.flags() & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0 {
        FORM
    } else {
        0
    };
    match e.custom_state() {
        CustomElementState::Undefined => return REPORT | FORM,
        CustomElementState::Custom => return REPORT | face,
        _ => {}
    }
    let name = e.name();
    let own = if name.ns == ns!(html) {
        match name.local {
            local_name!("script")
            | local_name!("iframe")
            | local_name!("frame")
            | local_name!("embed")
            | local_name!("link")
            | local_name!("style")
            | local_name!("video")
            | local_name!("audio")
            | local_name!("source")
            | local_name!("track")
            | local_name!("img")
            | local_name!("canvas")
            | local_name!("details") => REPORT,
            local_name!("object") => REPORT | FORM,
            local_name!("button")
            | local_name!("fieldset")
            | local_name!("input")
            | local_name!("output")
            | local_name!("select")
            | local_name!("textarea")
            | local_name!("option")
            | local_name!("optgroup")
            | local_name!("form") => FORM,
            _ => 0,
        }
    } else if name.ns == ns!(svg)
        && (super::svg::smil::is_animation_element(e)
            || matches!(&*name.local, "script" | "image" | "use" | "style"))
    {
        REPORT
    } else {
        0
    };
    own | face
}

/// Whether a descendant element gets an `Inserted`, `Removed` or `Adopted` reaction of its own.
#[cfg(test)]
fn wants_descendant_reaction(e: &ElementData) -> bool {
    descendant_bits(e) & subtree::REPORT != 0
}

fn wants_new_attribute_reaction(e: &ElementData, a: &Attr) -> bool {
    a.local_name().starts_with("on")
        || e.custom_state() != CustomElementState::Uncustomized
        // (a parsed `details` element's `open`: its attribute change steps queue `toggle`)
        || (e.name().ns == ns!(html)
            && e.name().local == local_name!("details")
            && &**a.local_name() == "open")
}

/// What a node is to [`Engine::react`], read once per reaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Not an element.
    Other,
    /// An HTML element with steps of its own, or one that is (or may become) a custom element:
    /// every step applies.
    Element,
    /// An HTML element with no steps of its own: only the steps every element has (event handler
    /// content attributes, `slot`/`name`, `nonce`, `popover`, `autofocus`, slot assignment,
    /// focus fixup, SVG `use` and animation bookkeeping) apply to it. It stays one: its name and
    /// `is` value never change, and an uncustomized element without either is never upgraded.
    PlainHtml,
    /// An uncustomized element not in the HTML namespace (SVG, MathML): none of the steps of
    /// particular HTML elements apply to it, nor custom element callbacks.
    Foreign,
}

fn kind(dom: &Dom, node: NodeId) -> Kind {
    let Some(e) = dom.element(node) else {
        return Kind::Other;
    };
    let name = e.name();
    if e.custom_state() != CustomElementState::Uncustomized
        || dom.element_flags(node) & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0
    {
        return Kind::Element;
    }
    if name.ns != ns!(html) {
        return Kind::Foreign;
    }
    // (an uncustomized element with no `is` value and no hyphen in its name matches no custom
    // element definition, so it is never upgraded nor form-associated)
    if dom.is_value(node).is_none()
        && !has_steps_of_its_own(&name.local)
        && !name.local.contains('-')
    {
        Kind::PlainHtml
    } else {
        Kind::Element
    }
}

/// The HTML elements that `react`'s steps single out by name: insertion, removing, attribute
/// change, children changed and adopting steps of their own, form states (listed elements,
/// options, forms, datalists), controls, media and images.
fn has_steps_of_its_own(local: &snowlark_dom::LocalName) -> bool {
    matches!(
        *local,
        local_name!("script")
            | local_name!("iframe")
            | local_name!("frame")
            | local_name!("object")
            | local_name!("embed")
            | local_name!("link")
            | local_name!("meta")
            | local_name!("base")
            | local_name!("img")
            | local_name!("source")
            | local_name!("track")
            | local_name!("video")
            | local_name!("audio")
            | local_name!("canvas")
            | local_name!("details")
            | local_name!("dialog")
            | local_name!("form")
            | local_name!("button")
            | local_name!("fieldset")
            | local_name!("input")
            | local_name!("output")
            | local_name!("select")
            | local_name!("textarea")
            | local_name!("option")
            | local_name!("optgroup")
            | local_name!("datalist")
    )
}

impl Engine {
    /// Queues one of the engine's microtasks.
    pub(crate) fn queue_internal_microtask(&self, cx: &mut Cx, task: Microtask) {
        let scope = cx.scope();
        let data = v8::Integer::new(scope, task as i32);
        let Some(f) = v8::Function::builder(rt::guarded(microtask_callback))
            .data(data.into())
            .build(scope)
        else {
            return;
        };
        scope.enqueue_microtask(f);
    }

    /// After a DOM operation outside `[CEReactions]` (or inside one, before its element queue pops).
    #[inline]
    pub(crate) fn after_mutation(&self, cx: &mut Cx) {
        self.process_reactions(cx);
    }

    /// Runs `op`, a DOM operation whose insertions are all of nodes it makes by fragment parsing
    /// (`innerHTML`, `outerHTML`, `insertAdjacentHTML`), then [`Engine::after_mutation`]. No script
    /// runs between their creation and their insertion (the fragment parsing algorithm constructs
    /// no custom element and runs no script), so none of them has a wrapper, and their insertion
    /// skips the wrapper GC's walk of each inserted subtree. Its removals, and reactions queued
    /// before it, are handled as always.
    pub(crate) fn parsed_insertion<R>(&self, cx: &mut Cx, op: impl FnOnce(&mut Dom) -> R) -> R {
        let start = self.dom().pending_reactions();
        let r = op(&mut self.dom_mut());
        self.parsed_from.set(Some(start));
        self.process_reactions(cx);
        self.parsed_from.set(None);
        r
    }

    /// Drains the DOM's reactions and mutation records.
    pub(crate) fn process_reactions(&self, cx: &mut Cx) {
        loop {
            // (only the operation's own batch: reactions can cause further operations)
            let parsed_from = self.parsed_from.replace(None);
            let mut reactions = self.dom_mut().take_reactions();
            let removed = self.dom_mut().take_removed_descendants();
            if !reactions.is_empty() {
                self.render_dom_changed(&reactions);
            }
            self.distribute_mutation_records(cx);
            if reactions.is_empty() {
                self.dom_mut().recycle_reactions(reactions);
                self.dom_mut().recycle_removed_descendants(removed);
                // (a slot signalled with no tree change, as by `assign()`)
                if self.pending_slot_signal.replace(false) {
                    self.queue_mutation_observer_microtask(cx);
                }
                // (the form pseudo-classes of the controls the operation changed that need a
                // script context; the others follow when something reads them)
                self.sync_form_states(Some(cx), super::forms::FormSync::Script);
                break;
            }
            let mut post_connection: Vec<NodeId> = Vec::new();
            // (the batch's details elements, whose insertion steps are yet to run; a batch that a
            // reaction's own DOM operation starts adds to the set and leaves the outer one's)
            let batch_details: Vec<NodeId> = {
                let dom = self.dom();
                reactions
                    .iter()
                    .filter_map(|r| match r {
                        Reaction::Inserted { node, .. }
                            if dom.is_alive(*node)
                                && dom.is_html_element_named(*node, &local_name!("details")) =>
                        {
                            Some(*node)
                        }
                        _ => None,
                    })
                    .collect()
            };
            self.details_pending
                .borrow_mut()
                .extend(batch_details.iter().copied());
            for (i, r) in reactions.drain(..).enumerate() {
                let parsed = parsed_from.is_some_and(|s| i >= s);
                self.react(cx, r, parsed, &removed, &mut post_connection);
            }
            self.dom_mut().recycle_reactions(reactions);
            self.dom_mut().recycle_removed_descendants(removed);
            {
                // (clearing costs the set's capacity: a set grown by one large batch, such as a
                // page's parse, is dropped rather than cleared for every small batch after it)
                let mut batch = self.inserted_batch.borrow_mut();
                if batch.capacity() > 4 * batch.len().max(16) {
                    *batch = Default::default();
                } else {
                    batch.clear();
                }
            }
            {
                let mut pending = self.details_pending.borrow_mut();
                for d in &batch_details {
                    pending.remove(d);
                }
            }
            if self.pending_slot_signal.replace(false) {
                self.queue_mutation_observer_microtask(cx);
            }
            // HTML "post-connection steps" run after all insertion steps of the operation.
            for el in post_connection {
                if !self.dom().is_alive(el) {
                    continue;
                }
                if self.is_frame_container(el) {
                    self.frame_inserted(cx, el);
                } else if self.dom().is_html_element_named(el, &local_name!("script"))
                    || (self.dom().is_script_element(el) && !self.svg.borrow().moving)
                {
                    self.script_post_connection(cx, el);
                }
            }
        }
    }

    /// One reaction of a batch; `removed` is the batch's list of removed nodes' descendants
    /// ([`snowlark_dom::Dom::take_removed_descendants`]).
    fn react(
        &self,
        cx: &mut Cx,
        r: Reaction,
        parsed: bool,
        removed: &[NodeId],
        post_connection: &mut Vec<NodeId>,
    ) {
        match r {
            Reaction::Inserted {
                node,
                connected,
                root,
                subtree: held,
            } => {
                if !self.dom().is_alive(node) {
                    return;
                }
                if root {
                    // The node and its descendants' wrappers (descendants may have no reactions of
                    // their own). Nodes just parsed have none.
                    if parsed {
                        #[cfg(debug_assertions)]
                        self.assert_unwrapped(cx, node);
                    } else {
                        self.gc_node_moved(cx, node, false);
                        if self.gc_subtree_wrapped(held) {
                            self.gc_touch_descendants(cx, node);
                        } else {
                            #[cfg(debug_assertions)]
                            {
                                let d: Vec<NodeId> =
                                    self.dom().shadow_including_descendants(node).collect();
                                self.assert_wrappers_marked(cx, d);
                            }
                        }
                    }
                    // Live ranges move for the nodes inserted into a parent (not one inserted
                    // earlier in this batch), not for their descendants.
                    let top_level = self
                        .dom()
                        .parent(node)
                        .is_some_and(|p| !self.inserted_batch.borrow().contains(&p));
                    self.inserted_batch.borrow_mut().insert(node);
                    if top_level {
                        self.live_ranges_inserted(node, 1);
                    }
                }
                let kind = kind(&self.dom(), node);
                if kind == Kind::Other {
                    return;
                }
                // (the steps of particular HTML elements and of custom elements apply only to
                // an `Element`; the others skip them)
                let particular = kind == Kind::Element;
                let plain = kind == Kind::PlainHtml;
                // (a subtree's form-state elements, if it holds any; the node's own, which only
                // those have)
                if root || particular {
                    let walk = root && held & super::forms::SUBTREE_FORM_STATE != 0;
                    self.form_state_inserted(node, walk);
                }
                if plain {
                    // (an HTML element is no animation element nor `use`)
                    self.smil_tree_changed_as(node, connected, false);
                    self.use_inserted_as(node, false);
                } else {
                    self.smil_tree_changed(node, connected);
                    self.use_inserted(node, connected);
                }
                if connected && !plain {
                    if self.dom().custom_element_state(node)
                        == snowlark_dom::CustomElementState::Custom
                    {
                        self.enqueue_callback(cx, node, "connectedCallback", Vec::new());
                    } else {
                        self.try_upgrade(cx, node);
                    }
                }
                if connected && !plain && super::svg::is_svg(&self.dom(), node, "script") {
                    post_connection.push(node);
                }
                // (an instance's image loads with its original's: no events of its own)
                if connected
                    && !plain
                    && super::svg::is_svg(&self.dom(), node, "image")
                    && !super::svg::use_tree::in_use_tree(&self.dom(), node)
                {
                    self.svg_image_update(node);
                }
                if connected
                    && particular
                    && self
                        .dom()
                        .is_html_element_named(node, &snowlark_dom::local_name!("dialog"))
                {
                    self.dialog_inserted(node);
                }
                self.html_insertion_steps(cx, node, connected, plain, post_connection);
                if particular {
                    if let Some(p) = self.dom().parent(node) {
                        self.option_tree_changed(node, p);
                    }
                    self.img_inserted(cx, node, root);
                    // HTML §4.8.11: media elements and their `<source>` and `<track>` children.
                    self.media_inserted(cx, node);
                }
            }
            Reaction::Removed {
                node,
                old_parent,
                old_previous,
                old_next,
                was_connected,
                descendants: (start, end),
                subtree: held,
            } => {
                if !self.dom().is_alive(node) {
                    return;
                }
                let kind = kind(&self.dom(), node);
                let descendants = &removed[start as usize..end as usize];
                if old_parent.is_some() {
                    // The removed node, its descendants' wrappers (descendants may have no
                    // reactions of their own; the removal collected them), and the focused
                    // element if it went with them.
                    self.gc_node_moved(cx, node, true);
                    if self.gc_subtree_wrapped(held) {
                        self.gc_touch_nodes(cx, descendants);
                    } else {
                        #[cfg(debug_assertions)]
                        {
                            let d: Vec<NodeId> = descendants
                                .iter()
                                .copied()
                                .filter(|&d| self.dom().is_alive(d))
                                .collect();
                            self.assert_wrappers_marked(cx, d);
                        }
                    }
                    self.unfocus_if_removed(node);
                }
                if let Some(p) = old_parent.filter(|&p| self.dom().is_alive(p)) {
                    if kind != Kind::Other {
                        // (the descendants are searched only when one has a form state)
                        let held_form_state = held & super::forms::SUBTREE_FORM_STATE != 0;
                        let descendants = if held_form_state { descendants } else { &[] };
                        self.form_state_removed(node, p, descendants);
                    }
                    if kind == Kind::Element {
                        self.option_tree_changed(node, p);
                    }
                    self.live_ranges_removed(node, p, old_previous);
                    self.node_iterators_removed(node, p, old_previous, old_next);
                    if was_connected {
                        self.pointer_node_removed(node, p);
                    }
                }
                if was_connected {
                    // (the fullscreen removing steps, before the top layer forgets the element)
                    self.fullscreen_removed(cx);
                }
                if was_connected && !self.html.borrow().popovers.is_empty() {
                    // (the popover removing steps)
                    self.popovers_removed(cx);
                }
                if was_connected && !self.dialogs.borrow().open_dialogs.is_empty() {
                    // (the dialog removing steps: removed open dialogs are cleaned up)
                    self.open_dialogs_prune();
                }
                if was_connected && !self.dialogs.borrow().top_layers.is_empty() {
                    // (the dialog removing steps, for top layer elements in the removed subtree)
                    self.top_layer_prune();
                }
                if kind != Kind::Other {
                    // (as for insertion: only an `Element` has steps of its own)
                    let particular = kind == Kind::Element;
                    if kind == Kind::PlainHtml {
                        self.smil_tree_changed_as(node, false, false);
                    } else {
                        self.smil_tree_changed(node, false);
                    }
                    self.use_removed(node, old_parent);
                    if particular
                        && was_connected
                        && self.dom().custom_element_state(node)
                            == snowlark_dom::CustomElementState::Custom
                    {
                        self.enqueue_callback(cx, node, "disconnectedCallback", Vec::new());
                    }
                    self.html_removing_steps(node, old_parent, particular);
                    if particular {
                        self.media_removed(node, old_parent);
                        self.img_removed(cx, node, old_parent, old_next);
                    }
                }
            }
            Reaction::AttributeChanged {
                element,
                local_name,
                namespace,
                old_value,
                new_value,
            } => {
                if !self.dom().is_alive(element) {
                    return;
                }
                // (as for insertion: only an `Element` has steps of its own)
                let kind = kind(&self.dom(), element);
                let particular = kind == Kind::Element;
                let plain = kind == Kind::PlainHtml;
                if particular
                    && self.dom().custom_element_state(element)
                        == snowlark_dom::CustomElementState::Custom
                {
                    self.enqueue_callback(
                        cx,
                        element,
                        "attributeChangedCallback",
                        vec![
                            CbArg::Str(local_name.to_string()),
                            CbArg::OptStr(old_value.clone()),
                            CbArg::OptStr(new_value.clone()),
                            CbArg::OptStr(namespace.as_ref().map(|n| n.to_string())),
                        ],
                    );
                }
                self.smil_attribute_changed(element, &local_name);
                self.use_attribute_changed(element, &local_name);
                if &*local_name == "href"
                    && !plain
                    && super::svg::is_svg(&self.dom(), element, "image")
                {
                    self.svg_image_update(element);
                }
                // (HTML §4.12.1: a script without a source that gets one is prepared)
                if &*local_name == "href"
                    && !plain
                    && old_value.is_none()
                    && super::svg::is_svg(&self.dom(), element, "script")
                {
                    self.script_post_connection(cx, element);
                }
                if particular
                    && &*local_name == "src"
                    && namespace.is_none()
                    && old_value.is_none()
                    && new_value.is_some()
                    && self
                        .dom()
                        .is_html_element_named(element, &local_name!("script"))
                {
                    self.script_post_connection(cx, element);
                }
                // (HTML §4.12.1: a script that gets an async attribute is no longer forced async)
                if particular
                    && namespace.is_none()
                    && &*local_name == "async"
                    && old_value.is_none()
                    && new_value.is_some()
                    && self
                        .dom()
                        .is_html_element_named(element, &snowlark_dom::local_name!("script"))
                {
                    self.set_script_force_async(element, false);
                }
                if namespace.is_none() {
                    // (the dialog attribute change steps: `open` added or removed)
                    if particular
                        && new_value.is_some() != old_value.is_some()
                        && &*local_name == "open"
                        && self
                            .dom()
                            .is_html_element_named(element, &snowlark_dom::local_name!("dialog"))
                    {
                        self.dialog_open_changed(element, new_value.is_some());
                    }
                    if &*local_name == "popover" {
                        self.popover_attribute_changed(
                            cx,
                            element,
                            old_value.as_deref(),
                            new_value.as_deref(),
                        );
                    }
                    self.html_attribute_changed(
                        cx,
                        element,
                        &local_name,
                        particular,
                        old_value.as_deref(),
                        new_value.as_deref(),
                    );
                    if !particular {
                        return;
                    }
                    self.form_state_attribute_changed(element);
                    self.control_attribute_changed(element);
                    self.details_attribute_changed(
                        cx,
                        element,
                        &local_name,
                        old_value.as_deref(),
                        new_value.as_deref(),
                    );
                    self.input_attribute_changed(
                        cx,
                        element,
                        &local_name,
                        old_value.as_deref(),
                        new_value.as_deref(),
                    );
                    // HTML §4.8.4.3: an `img` element's relevant mutations.
                    if super::img::is_relevant_change(
                        &local_name,
                        old_value.as_deref(),
                        new_value.as_deref(),
                    ) && self
                        .dom()
                        .is_html_element_named(element, &local_name!("img"))
                    {
                        self.update_the_image_data(cx, element);
                    }
                    self.media_attribute_changed(cx, element, &local_name, new_value.is_some());
                    if self
                        .dom()
                        .is_html_element_named(element, &local_name!("track"))
                    {
                        self.track_attribute_changed(
                            element,
                            &local_name,
                            old_value.as_deref(),
                            new_value.as_deref(),
                        );
                    }
                }
            }
            Reaction::ChildrenChanged { parent } => {
                if !self.dom().is_alive(parent) {
                    return;
                }
                self.use_children_changed(parent);
                // (a textarea's, a select's or an option's; a script's, HTML or SVG)
                let kind = kind(&self.dom(), parent);
                if kind == Kind::Element {
                    self.form_state_children_changed(parent);
                }
                if matches!(kind, Kind::Element | Kind::Foreign)
                    && self.dom().is_script_element(parent)
                    && self.dom().is_connected(parent)
                {
                    // DOM §4.2.3 "insert" runs the children changed steps (step 9) before the
                    // post-connection steps of the nodes it inserted (step 11): the script's
                    // steps go before those of its new descendants.
                    let dom = self.dom();
                    let at = post_connection
                        .iter()
                        .position(|&n| dom.is_ancestor_of(parent, n));
                    drop(dom);
                    match at {
                        Some(i) => post_connection.insert(i, parent),
                        None => post_connection.push(parent),
                    }
                }
                self.slot_children_changed(parent);
            }
            Reaction::Adopted { node, old_document } => {
                if !self.dom().is_alive(node) {
                    return;
                }
                // (controls moved from a rendered document to another: the new one's renderer has
                // not seen their state; those from a document not rendered are still marked)
                if !self.html.borrow().controls.is_empty()
                    && self.is_rendered_document(old_document)
                {
                    self.controls_resync.set(true);
                }
                if !self.dom().is_element(node) {
                    return;
                }
                // (an `img` adopted into another document: a relevant mutation)
                if self.dom().is_html_element_named(node, &local_name!("img")) {
                    self.update_the_image_data(cx, node);
                }
                if self.dom().custom_element_state(node) == snowlark_dom::CustomElementState::Custom
                {
                    let new_document = self.dom().node_document(node);
                    self.enqueue_callback(
                        cx,
                        node,
                        "adoptedCallback",
                        vec![
                            CbArg::Node(node_handle(old_document)),
                            CbArg::Node(node_handle(new_document)),
                        ],
                    );
                }
            }
            Reaction::Cloned { node, copy, .. } => {
                if self.dom().is_alive(node) && self.dom().is_alive(copy) {
                    self.html_cloning_steps(node, copy);
                }
            }
            Reaction::DataReplaced {
                node,
                offset,
                count,
                added,
            } => self.live_ranges_data_replaced(node, offset, count, added),
            Reaction::TextSplit {
                node,
                new_node,
                offset,
                parent,
                index,
            } => self.live_ranges_text_split(node, new_node, offset, parent, index),
            Reaction::TextMerged {
                from,
                into,
                at,
                parent,
                index,
            } => self.live_ranges_text_merged(from, into, at, parent, index),
        }
    }

    /// HTML's insertion steps for an element (`plain`: one of [`Kind::PlainHtml`], which has none
    /// of its own).
    fn html_insertion_steps(
        &self,
        cx: &mut Cx,
        el: NodeId,
        connected: bool,
        plain: bool,
        post_connection: &mut Vec<NodeId>,
    ) {
        let dom = self.dom();
        if connected
            && self.policy.borrow().any
            && dom.attr(el, &ns!(), &local_name!("nonce")).is_some()
        {
            drop(dom);
            self.hide_nonce(el);
        } else {
            drop(dom);
        }
        let dom = self.dom();
        // (HTML §6.6.4: an HTML or SVG element with `autofocus` becomes an autofocus candidate)
        let autofocus = connected
            && (dom.namespace(el) == &ns!(html) || dom.namespace(el) == &ns!(svg))
            && dom.attr(el, &ns!(), &local_name!("autofocus")).is_some();
        if autofocus {
            drop(dom);
            self.autofocus_inserted(el);
        } else {
            drop(dom);
        }
        if plain {
            self.slot_insertion_steps(el);
            return;
        }
        let dom = self.dom();
        if dom.namespace(el) != &ns!(html) {
            return;
        }
        let local = dom.local_name(el).clone();
        drop(dom);
        match local {
            local_name!("meta") if connected => {
                self.meta_inserted(cx, el);
                self.meta_refresh_inserted(el);
                self.meta_content_language_inserted(el);
            }
            local_name!("base") if connected && self.policy.borrow().any => {
                self.base_inserted(cx, el)
            }
            local_name!("script")
            | local_name!("iframe")
            | local_name!("frame")
            | local_name!("object")
            | local_name!("embed")
                if connected =>
            {
                post_connection.push(el)
            }
            local_name!("link") if connected => self.link_element_changed(el),
            local_name!("details") => self.details_inserted(cx, el),
            // Not a spec step: the thread's canvas device is made on a helper thread from now, so
            // that a context asked for later does not wait the device's making (about 150 ms in a
            // process without one), as Chromium's canvases find the GPU process's device made.
            local_name!("canvas") if connected => {
                snowlark_render::canvas::Backend::from_env().prepare()
            }
            _ => {}
        }
        self.slot_insertion_steps(el);
    }

    /// Focus fixup (HTML §6.6.4) for a removed subtree: a document whose focused element was in it
    /// is left without focus.
    fn unfocus_if_removed(&self, root: NodeId) {
        let dom = self.dom();
        let doc = dom.node_document(root);
        let mut html = self.html.borrow_mut();
        if let Some(s) = html.docs.get_mut(&doc) {
            // (a connected focused element is not in a disconnected subtree: no ancestor walk)
            if s.focused.is_some_and(|f| {
                dom.is_alive(f)
                    && !(dom.is_connected(f) && !dom.is_connected(root))
                    && dom.is_shadow_including_inclusive_ancestor_of(root, f)
            }) {
                s.focused = None;
            }
        }
    }

    /// HTML's removing steps for an element (`particular`: a [`Kind::Element`], which may have
    /// steps of its own).
    fn html_removing_steps(&self, el: NodeId, old_parent: Option<NodeId>, particular: bool) {
        // Focus fixup (HTML §6.6.4): a removed focused element leaves its document without focus.
        // Every shadow-including descendant of a removed node gets a reaction of its own, so the
        // focused element is met as `el` itself when it goes with an ancestor.
        let doc = self.dom().node_document(el);
        self.doc_state(doc, |s| {
            if s.focused == Some(el) {
                s.focused = None;
            }
        });
        if particular && self.is_frame_container(el) {
            self.frame_removed(el);
        }
        // (a removed `link` does not process the style sheet it was fetching: no sheet, no `load`)
        if self.dom().is_html_element_named(el, &local_name!("link")) {
            self.loader.borrow_mut().sheet_fetches.remove(&el);
        }
        self.slot_removing_steps(el, old_parent);
    }

    /// Attribute change steps of HTML elements: event handler content attributes, slots, and
    /// those of particular elements (`particular`: a [`Kind::Element`]; the others have only the
    /// steps every element has).
    fn html_attribute_changed(
        &self,
        cx: &mut Cx,
        el: NodeId,
        local: &snowlark_dom::LocalName,
        particular: bool,
        old_value: Option<&str>,
        value: Option<&str>,
    ) {
        if let Some(event) = local.strip_prefix("on") {
            let svg_event = super::svg::animation_event_type(&self.dom(), el, event);
            let html_event = || {
                super::html::event_handler_name(event)
                    .filter(|e| super::html::is_handler_content_attribute(&self.dom(), el, e))
                    .map(super::html::handler_event_type)
            };
            if let Some(event) = svg_event.or_else(html_event) {
                // (event handler content attributes apply to HTML and SVG elements)
                let is_html =
                    matches!(self.dom().namespace(el), n if *n == ns!(html) || *n == ns!(svg));
                // CSP: an event handler attribute the document's policy blocks sets nothing.
                let blocked = is_html
                    && value.is_some_and(|body| {
                        self.inline_blocked(
                            cx,
                            el,
                            snowlark_net::csp::InlineType::ScriptAttribute,
                            body,
                        )
                    });
                // (no target: a `body`'s Window-reflecting handler in an inactive document)
                let target = self.handler_target(node_handle(el), event);
                if let Some(target) = target.filter(|_| is_html && !blocked) {
                    match value {
                        Some(body) => self.set_handler(
                            target,
                            event,
                            Some(super::events::Handler::Uncompiled {
                                body: body.to_owned(),
                                element: Some(el),
                                line: 1,
                            }),
                        ),
                        None => self.set_handler(target, event, None),
                    }
                }
            }
        }
        if particular
            && matches!(&**local, "name" | "content")
            && self.dom().is_html_element_named(el, &local_name!("meta"))
        {
            self.meta_referrer(el);
        }
        match &**local {
            "slot" | "name" => self.slot_attribute_changed(el, local),
            // HTML §8.1.5.? the nonce attribute's change steps: the [[CryptographicNonce]] slot
            // follows it (kept apart only once a nonce was hidden or set through the IDL).
            "nonce" => {
                if let Some(v) = value.filter(|v| !v.is_empty()) {
                    if let Some(slot) = self.hidden_nonces.borrow_mut().get_mut(&el) {
                        *slot = v.to_owned();
                    }
                }
            }
            // (the steps below are of particular elements)
            _ if !particular => {}
            "loading" if self.dom().is_html_element_named(el, &local_name!("img")) => {
                self.img_loading_changed(el);
            }
            "loading" if self.dom().is_html_element_named(el, &local_name!("iframe")) => {
                self.frame_loading_changed(cx, el);
            }
            // (a `source` of a `picture` changed: the `img` elements after it select again)
            "srcset" | "sizes" | "media" | "type"
                if self.dom().is_html_element_named(el, &local_name!("source")) =>
            {
                self.picture_changed(cx, el);
            }
            "href"
                if self.dom().is_html_element_named(el, &local_name!("base"))
                    && self.dom().is_connected(el)
                    && self.policy.borrow().any =>
            {
                self.base_inserted(cx, el)
            }
            // (a new resource: its response decides what the element shows again)
            "src" | "data" | "type" if self.html.borrow().embed_types.contains_key(&el) => {
                self.html.borrow_mut().embed_types.remove(&el);
                if self.is_frame_container(el) && &**local != "type" {
                    self.frame_attribute_changed(cx, el, local)
                } else {
                    self.embed_redetermine(el, local);
                }
            }
            "src" | "srcdoc" | "data" if self.is_frame_container(el) => {
                self.frame_attribute_changed(cx, el, local)
            }
            // (an `object` or `embed` whose new resource is no document: its navigable goes)
            "src" | "data" | "type"
                if self.dom().is_html_element_named(el, &local_name!("object"))
                    || self.dom().is_html_element_named(el, &local_name!("embed")) =>
            {
                self.embed_redetermine(el, local)
            }
            // HTML §4.12.5: a canvas's bitmap is reset when its size attributes change.
            "width" | "height" if self.dom().is_html_element_named(el, &local_name!("canvas")) => {
                self.canvas_resized(el)
            }
            // HTML §4.2.4: a link's resource is fetched again when `href` or `rel` changes (set
            // to the value it had, it did not change).
            "href" | "rel" | "disabled"
                if self.dom().is_html_element_named(el, &local_name!("link"))
                    && self.dom().is_connected(el)
                    && (old_value != value || &**local == "disabled") =>
            {
                self.link_element_changed(el)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_dom::ParseOptions;

    /// The first element of `doc` with `id`.
    fn by_id(dom: &Dom, doc: NodeId, id: &str) -> NodeId {
        dom.inclusive_descendants(doc)
            .find(|&n| dom.is_element(n) && dom.attr(n, &ns!(), &"id".into()) == Some(id))
            .unwrap()
    }

    #[test]
    fn only_html_elements_without_steps_of_their_own_are_plain() {
        let mut dom = Dom::new();
        let doc = dom.parse_html_str(
            "<div id=div></div><p id=p>text</p><span id=is is=x-span></span><x-a id=custom></x-a>\
             <svg><g id=g /></svg>",
            ParseOptions::default(),
        );
        assert_eq!(kind(&dom, by_id(&dom, doc, "div")), Kind::PlainHtml);
        assert_eq!(kind(&dom, by_id(&dom, doc, "p")), Kind::PlainHtml);
        let text = dom.first_child(by_id(&dom, doc, "p")).unwrap();
        assert_eq!(kind(&dom, text), Kind::Other);
        assert_eq!(kind(&dom, doc), Kind::Other);
        // (an `is` value or a valid custom element name: it may be upgraded)
        assert_eq!(kind(&dom, by_id(&dom, doc, "is")), Kind::Element);
        assert_eq!(kind(&dom, by_id(&dom, doc, "custom")), Kind::Element);
        // (not HTML: its own steps are SVG's, which `react` tests for it)
        assert_eq!(kind(&dom, by_id(&dom, doc, "g")), Kind::Foreign);
        // (a form-associated custom element)
        let div = by_id(&dom, doc, "div");
        dom.set_form_associated_custom(div, true);
        assert_eq!(kind(&dom, div), Kind::Element);
    }

    /// Every element `react`'s handlers single out by name, and every one whose descendants the
    /// reaction filter asks for, has steps of its own.
    #[test]
    fn elements_with_steps_of_their_own_are_not_plain() {
        let names = [
            "script", "iframe", "frame", "object", "embed", "link", "meta", "base", "img",
            "source", "track", "video", "audio", "canvas", "details", "dialog", "form", "button",
            "fieldset", "input", "output", "select", "textarea", "option", "optgroup", "datalist",
        ];
        let mut dom = Dom::new();
        let doc = dom.parse_html_str("", ParseOptions::default());
        for name in names {
            let el = dom.create_element(doc, name, None).unwrap();
            assert_eq!(kind(&dom, el), Kind::Element, "{name}");
        }
        for name in [
            "div", "span", "li", "ul", "a", "section", "label", "slot", "template",
        ] {
            let el = dom.create_element(doc, name, None).unwrap();
            assert_eq!(kind(&dom, el), Kind::PlainHtml, "{name}");
            let data = dom.element(el).unwrap();
            assert!(!wants_descendant_reaction(data), "{name}");
        }
    }

    /// The filter's one match agrees with the predicates it stands for: the renderer's
    /// descendants, the SVG elements with steps of their own, and the elements with a form state.
    #[test]
    fn descendant_bits_cover_the_renderer_and_form_states() {
        let mut dom = Dom::new();
        let doc = dom.parse_html_str(
            "<div></div><style></style><link><input><object></object><option></option><form>             </form><x-a></x-a><span is=x-b></span><svg><style/><script/><use/><g/><animate/>             </svg><math><mi/></math>",
            ParseOptions::default(),
        );
        let mut seen = 0;
        for el in dom
            .inclusive_descendants(doc)
            .filter(|&n| dom.is_element(n))
        {
            let data = dom.element(el).unwrap();
            let bits = descendant_bits(data);
            let name = format!("{:?}", data.name());
            if snowlark_render::renderer::wants_descendant_reaction(data)
                || super::super::svg::smil::is_animation_element(data)
            {
                assert!(bits & subtree::REPORT != 0, "{name}");
            }
            let form = super::super::forms::SUBTREE_FORM_STATE;
            let undefined = data.custom_state() == CustomElementState::Undefined;
            assert_eq!(
                bits & form != 0,
                super::super::forms::form_state_bit(data) != 0,
                "{name}"
            );
            assert_eq!(
                bits & form != 0,
                undefined || is_form_state_name(data),
                "{name}"
            );
            seen += 1;
        }
        assert!(seen > 15);
    }

    fn is_form_state_name(e: &ElementData) -> bool {
        e.name().ns == ns!(html)
            && matches!(
                &*e.name().local,
                "button"
                    | "fieldset"
                    | "input"
                    | "object"
                    | "output"
                    | "select"
                    | "textarea"
                    | "option"
                    | "optgroup"
                    | "form"
            )
    }
}
