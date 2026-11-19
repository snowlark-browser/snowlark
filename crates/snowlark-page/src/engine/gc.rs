// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wrapper lifetime and freeing (ARCHITECTURE.md "Wrappers and GC"), the engine's side of
//! `snowlark_bindings::rt::gc`.
//!
//! - A node's wrapper is strong while the node is in a document that is alive (the active document
//!   of a browsing context). Otherwise it is weak and anchored at the node's opaque root
//!   ([`Dom::opaque_root`]): the group of a detached subtree lives while any wrapper in it is
//!   reachable.
//! - Objects an element owns (`classList`, `dataset`, `attributes`, `style`, `validity`, its live
//!   collections) are weak and anchored at their owner's wrapper.
//! - Objects nothing in Rust keeps (events, rects, ranges, records, static node lists...) are weak;
//!   while they exist they *pin* the nodes they refer to: those nodes' groups stay strong.
//! - Every other platform object stays strong, as before.
//!
//! Passes run from a V8 interrupt when enough wrappers are pending and at [`Engine::collect_garbage`],
//! which the event loop calls between tasks: there, dead wrappers are taken, and detached subtrees
//! with no live wrapper, no pin and no Rust reference are freed ([`Dom::free_subtree`]) with their
//! side tables, as are collectable objects with no wrapper.
//!
//! A pin that appears after a group was made weak takes effect at the next pass; a garbage
//! collection in between can lose that group's expando properties (the group's nodes stay alive).
//!
//! Event listeners' and handlers' callbacks are held strongly while their target's wrapper is, and
//! by the wrapper itself once it is weak ([`super::events::Callback`]): a closure that reaches its
//! target (or its group, or its realm) does not keep them alive from outside JS.
//!
//! Discarded browsing contexts (a removed iframe's, HTML §7.3.1.5 "destroy a child navigable") and
//! Windows a navigation replaced (with the document they had): the
//! engine drops its handle on the realm, and the Window, its singletons, its document's objects and
//! the document's nodes become one weak group anchored at the Window: script that still reaches any
//! of them keeps all of them (and with them the realm), as before. Once all their wrappers died and
//! nothing pins them, the document's tree, the Window and those objects are freed. The engine state
//! kept per active document (its document state, its loading state, the context's pins of its
//! document and container) does not pin a discarded document; the timers, animation frame and idle
//! callbacks of its Window, which never run again, go. A document a navigation replaced is freed
//! the same way (alone: its Window may stay with the new document).

use std::collections::HashSet;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use snowlark_bindings::rt::{self, Cx, Handle, Keep};
use snowlark_dom::NodeId;

use super::cssom::Decl;
use super::events::{EventData, EventExt};
use super::{handle_node, node_handle, Engine, Object};

/// How an object's wrapper lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Kept for good.
    Strong,
    /// Weak, anchored at the owner element's wrapper; freed with its wrapper.
    Owned(NodeId),
    /// Weak; freed when its wrapper is dead (or it never got one) and nothing pins it.
    Weak,
}

fn class(o: &Object) -> Class {
    match o {
        Object::TokenList { element, .. }
        | Object::StringMap { element }
        | Object::AttrMap { element }
        | Object::ValidityState { element } => Class::Owned(*element),
        Object::Svg(o) => match o.gc_owner() {
            Some(element) => Class::Owned(element),
            None => Class::Weak,
        },
        Object::StyleDeclaration(Decl::Inline(n)) => Class::Owned(*n),
        Object::DomImplementation { document } => Class::Owned(*document),
        Object::Collection(c) => match c.live_anchor() {
            Some(owner) => Class::Owned(owner),
            None => Class::Weak,
        },
        Object::Event(_)
        | Object::IdleDeadline { .. }
        | Object::EventTarget
        | Object::DomException { .. }
        | Object::MutationRecord(_)
        | Object::Range(_)
        | Object::StaticRange(_)
        | Object::TreeWalker(_)
        | Object::NodeIterator(_)
        | Object::DomRect(_)
        | Object::DomRectList(_)
        | Object::Touch(_)
        | Object::TouchList(_)
        | Object::DomPoint(_)
        | Object::DomQuad(_)
        | Object::DomMatrix(_)
        | Object::CryptoKey(_)
        | Object::TextMetrics { .. }
        | Object::ImageData(_)
        | Object::TrustedHtml(_)
        | Object::TrustedScript(_)
        | Object::TrustedScriptUrl(_)
        | Object::PerformanceEntry(_)
        | Object::PerformanceServerTiming(_)
        | Object::PermissionStatus(_)
        | Object::ClipboardItem(_)
        | Object::IntersectionObserverEntry(_)
        | Object::ResizeObserverEntry(_)
        | Object::ResizeObserverSize { .. }
        | Object::PerformanceObserverEntryList(..)
        | Object::StyleDeclaration(Decl::Computed(..)) => Class::Weak,
        Object::Media(m) if m.weak() => Class::Weak,
        // Animations live while script can reach them or while they are relevant
        // ([`Engine::animation_pins`]); effects while their animation does.
        Object::Animation(_) | Object::KeyframeEffect(_) => Class::Weak,
        // A media query list lives while script can reach it or it has a `change` listener
        // ([`Engine::media_list_pins`]): pages make one per `matchMedia(..).matches` read.
        Object::MediaQueryList(_) => Class::Weak,
        // IndexedDB's requests, key ranges and records live while script reaches them or a
        // transaction's request list (or a queued event) keeps them ([`IdbObject::weak`]).
        Object::Idb(o) if o.weak() => Class::Weak,
        Object::StringList(_, false) => Class::Weak,
        // A Request lives while script reaches it: `fetch()` reads the one it makes and starts the
        // fetch from a copy, and nothing else in the engine names it.
        Object::Request(_) => Class::Weak,
        // Objects that hold nothing the engine names by handle and have no pending activity: they
        // live while script reaches them, as Blink's (plain `ScriptWrappable`s, freed with their
        // wrappers). Pages make them per call (`new DOMParser().parseFromString(..)` per render).
        Object::DomParser { .. }
        | Object::XmlSerializer
        | Object::Sanitizer(_)
        | Object::TrustedHtmlParserOptions(_)
        | Object::TextEncoder
        | Object::TextDecoder(_) => Class::Weak,
        _ => Class::Strong,
    }
}

/// What an object refers to that must stay alive while it does.
#[derive(Default)]
struct Refs {
    nodes: Vec<NodeId>,
    objects: Vec<Handle>,
    /// Live collections' (root, anchor) when they differ.
    live_roots: Vec<(NodeId, NodeId)>,
}

impl Refs {
    fn handle(&mut self, h: Handle) {
        match handle_node(h) {
            Some(n) => self.nodes.push(n),
            None => self.objects.push(h),
        }
    }
    fn opt(&mut self, h: Option<Handle>) {
        if let Some(h) = h {
            self.handle(h);
        }
    }
}

fn event_refs(e: &EventData, r: &mut Refs) {
    r.opt(e.target);
    r.opt(e.related_target);
    r.opt(e.current_target);
    for p in &e.path {
        r.handle(p.item);
        r.opt(p.shadow_adjusted_target);
        r.opt(p.related_target);
    }
    match &e.ext {
        EventExt::Ui(u) => {
            r.opt(u.view);
            r.opt(u.related_target);
            r.opt(u.data_transfer);
            for &h in u
                .target_ranges
                .iter()
                .chain(&u.coalesced)
                .chain(&u.predicted)
            {
                r.handle(h);
            }
            r.objects.extend(u.touch_lists.iter().flatten());
        }
        EventExt::Submit(h) | EventExt::Clipboard(h) => r.opt(*h),
        EventExt::FormData(h) => r.opt(Some(*h)),
        EventExt::Command { source, .. } => r.opt(*source),
        EventExt::Toggle { source, .. } => r.opt(*source),
        EventExt::Message { source, ports, .. } => {
            r.opt(*source);
            r.objects.extend(ports);
        }
        EventExt::FontFaces(faces) => faces.iter().for_each(|&h| r.handle(h)),
        EventExt::Storage(s) => r.opt(s.storage_area),
        EventExt::Track(t) => r.opt(*t),
        EventExt::CssAnimation(c) => r.opt(c.animation),
        _ => {}
    }
}

/// The references of an object that pin (an owned object's owner is an anchor, not a pin).
fn refs(o: &Object, r: &mut Refs) {
    match o {
        Object::Event(e) => event_refs(e, r),
        Object::MutationObserver(m) => m.records.iter().for_each(|&h| r.handle(h)),
        Object::FontFaceSet { script, css, .. } => {
            script.iter().for_each(|&h| r.handle(h));
            css.iter().for_each(|&(_, h)| r.handle(h));
        }
        Object::MutationRecord(rec) => {
            r.nodes.push(rec.target);
            r.nodes.extend(rec.previous_sibling);
            r.nodes.extend(rec.next_sibling);
            r.objects.push(rec.added);
            r.objects.push(rec.removed);
        }
        Object::Range(b) | Object::StaticRange(b) => {
            r.nodes.push(b.start.0);
            r.nodes.push(b.end.0);
        }
        Object::TreeWalker(w) => {
            r.nodes.push(w.root);
            r.nodes.push(w.current);
        }
        Object::NodeIterator(i) => {
            r.nodes.push(i.root);
            r.nodes.push(i.reference);
        }
        Object::DomRectList(rects) | Object::FileList(rects) | Object::TouchList(rects) => {
            r.objects.extend(rects)
        }
        Object::Touch(t) => r.handle(t.target),
        Object::DataTransfer(d) => d.gc_refs(&mut r.objects),
        Object::DataTransferItemList(dt) | Object::DataTransferItem(dt) => r.objects.push(*dt),
        Object::Svg(o) => o.gc_refs(&mut r.objects),
        // (the SVG object a viewBox rect reflects; a list's points and a transform's matrix are
        // kept by their owner, which detaches them when it is freed)
        Object::DomRect(rect) => r.opt(rect.svg),
        Object::DomQuad(points) => r.objects.extend(points),
        Object::Collection(c) => match c.live_anchor() {
            Some(anchor) => {
                let from = r.nodes.len();
                c.gc_refs(&mut r.nodes);
                let roots = r.nodes.drain(from..).map(|root| (root, anchor));
                r.live_roots.extend(roots);
            }
            None => c.gc_refs(&mut r.nodes),
        },
        Object::StyleDeclaration(Decl::Computed(n, _)) => r.nodes.push(*n),
        Object::StyleDeclaration(Decl::Rule(h) | Decl::FontFace(h)) => r.objects.push(*h),
        Object::Canvas2D(c) => {
            r.nodes.push(c.element);
            r.opt(c.fill_object);
            r.opt(c.stroke_object);
        }
        Object::ElementInternals(i) => {
            r.nodes.extend(i.element);
            i.refs.values().for_each(|v| r.nodes.extend(v));
            r.opt(i.states_object);
        }
        Object::StyleSheet(s) => {
            r.nodes.extend(s.owner);
            r.opt(s.owner_rule);
        }
        Object::StyleSheetList { scope } => r.nodes.push(*scope),
        Object::Selection(s) => {
            r.nodes.push(s.document);
            r.opt(s.range);
        }
        Object::FileReader(f) => r.opt(f.error),
        Object::Xhr(x) => {
            r.opt(x.window);
            r.handle(x.upload);
            if let Some(Some(d)) = x.response_document {
                r.nodes.push(d);
            }
        }
        Object::Request(q) => {
            r.handle(q.headers);
            r.handle(q.signal);
            r.nodes.extend(q.client);
        }
        Object::Response(p) => r.handle(p.headers),
        Object::Performance(p) => {
            for t in &p.tuples {
                r.objects.extend(&t.buffer);
            }
            r.objects.extend(&p.resource_secondary);
        }
        Object::PerformanceObserver(o) => r.objects.extend(&o.buffer),
        Object::PerformanceObserverEntryList(list, _) => r.objects.extend(list),
        Object::IntersectionObserver(o) => {
            r.objects.extend(&o.queued);
            r.nodes.extend(o.root);
        }
        Object::ResizeObserverEntry(e) => {
            r.nodes.push(e.target);
            r.objects.push(e.content_rect);
            r.objects.push(e.border_box_size);
            r.objects.push(e.content_box_size);
            r.objects.push(e.device_pixel_content_box_size);
        }
        Object::Animation(a) => {
            r.opt(a.effect);
            r.opt(a.timeline);
            if let Some(
                super::animation::CssOwner::Animation { element, .. }
                | super::animation::CssOwner::Transition { element, .. },
            ) = &a.css
            {
                r.nodes.push(*element);
            }
        }
        // (an effect's animation is a back reference: it does not keep the animation)
        Object::KeyframeEffect(e) => r.nodes.extend(e.target),
        Object::Idb(o) => o.refs(&mut r.objects),
        Object::IntersectionObserverEntry(e) => {
            r.nodes.push(e.target);
            r.opt(e.root_bounds);
            r.objects.push(e.bounding_client_rect);
            r.objects.push(e.intersection_rect);
        }
        _ => {}
    }
}

/// What a pass knows besides the DOM: the documents that are alive and the opaque roots and objects
/// something outside JS refers to.
pub(crate) struct Snapshot {
    /// Node ids as bits ([`NodeId::to_bits`]).
    live_docs: FastSet,
    pinned_roots: FastSet,
    pinned_objects: FastSet,
    discarded: Discarded,
    /// With [`Engine::snapshot_traced`]: why each inactive document and each pinned member of a
    /// discarded context's group is pinned, by source.
    explain: Vec<String>,
    /// How many objects and references it went through: what it cost.
    walked: usize,
}

/// New wrappers per item a snapshot walks before the next pass is asked for: a page's passes then
/// cost a bounded amount per wrapper it makes, whatever it holds.
const WALKED_PER_WRAPPER: usize = 4;

/// Items a snapshot walks per candidate (removed subtree or new collectable object) a collection
/// waits for: as V8 sizes its next collection by the heap the last one left, a collection's
/// snapshot then costs a bounded amount per candidate, however much the page holds.
const WALKED_PER_CANDIDATE: usize = 8;

/// The least backlog of removed subtrees and new collectable objects an idle loop collects at
/// ([`Engine::collect_garbage_when_idle`]), whatever the last snapshot walked.
const IDLE_BACKLOG: usize = 1024;

/// How many times the idle backlog a busy loop collects at ([`Engine::collect_garbage_after_task`]):
/// in between, the garbage waits for the loop's idle time.
const BUSY_BACKLOG_FACTOR: usize = 4;

/// How old the last full collection is before V8's collections make an idle loop collect again.
const IDLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How old the last full collection is before V8's collections or dead wrappers make a busy loop
/// collect (one that has not been idle that long).
const BUSY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a collection a busy loop has due waits for a turn after which no task is ready, before
/// it runs ahead of them: what a page that always has a task ready allocates in that time is what
/// it holds more for it.
const READY_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The least idle time a collection runs in.
const IDLE_MIN: std::time::Duration = std::time::Duration::from_millis(4);

/// What a traced snapshot records: each pinning node and object with where it came from.
#[derive(Default)]
struct PinTrace {
    nodes: Vec<(NodeId, String)>,
    objects: Vec<(Handle, String)>,
}

/// The discarded browsing contexts whose Window was not freed yet, and what joins their Window's
/// group.
#[derive(Default)]
struct Discarded {
    /// Document → Window.
    docs: FxHashMap<NodeId, Handle>,
    /// Window → document.
    windows: FxHashMap<Handle, NodeId>,
    /// The Windows, their singletons and their documents' objects → the Window. (Shared with
    /// [`GcState::grouping`], which the next snapshot starts from.)
    owned: Rc<FxHashMap<Handle, Handle>>,
    /// Documents a navigation replaced as their browsing context's active document (their Window,
    /// when it stays with the new document, is not theirs any more).
    unloaded: rustc_hash::FxHashSet<NodeId>,
    /// The members that are in a group only because their realm made their wrapper (not a
    /// Window's singleton, a document's object or an animation).
    made: Rc<rustc_hash::FxHashSet<Handle>>,
    /// Which group each node asked about is in.
    groups: NodeGroups,
    /// How many objects finding the groups went through (part of a snapshot's cost).
    asked: usize,
}

/// A node whose document and opaque root are in different groups: [`NodeGroups`] asks again.
const MIXED: u32 = u32::MAX;

/// The discarded group of each node, found once per snapshot. Objects that hold many nodes (the
/// static lists `querySelectorAll` returns) share most of them, and a walk over them would otherwise
/// read each node's slot, far apart in the arena, once per list.
#[derive(Default)]
struct NodeGroups {
    /// Document → group (1 and up), which is an index into `windows` plus one.
    docs: FxHashMap<NodeId, u32>,
    windows: Vec<Handle>,
    /// By slot: the generation of the node the entry is for (0: none, as generations are not), then
    /// its group (0: none) or [`MIXED`]. A stale id's generation differs, so it finds no entry.
    memo: Vec<u64>,
}

impl NodeGroups {
    fn new(dom: &snowlark_dom::Dom, docs: &FxHashMap<NodeId, Handle>) -> Self {
        let mut g = NodeGroups {
            memo: vec![0; dom.node_slots()],
            ..Default::default()
        };
        // (a Window can have several documents here, replaced ones: they are one group)
        let mut ids: FxHashMap<Handle, u32> = FxHashMap::default();
        for (&doc, &w) in docs {
            let id = *ids.entry(w).or_insert_with(|| {
                g.windows.push(w);
                g.windows.len() as u32
            });
            g.docs.insert(doc, id);
        }
        g
    }

    fn group(&mut self, dom: &snowlark_dom::Dom, n: NodeId) -> u32 {
        let generation = u64::from(n.generation());
        let slot = n.index() as usize;
        if let Some(&e) = self.memo.get(slot) {
            if e >> 32 == generation {
                return e as u32;
            }
        }
        if !dom.is_alive(n) {
            return 0;
        }
        let by_document = self.docs.get(&dom.node_document(n)).copied();
        let by_root = self.docs.get(&dom.opaque_root(n)).copied();
        let group = match (by_document, by_root) {
            (Some(a), Some(b)) if a != b => MIXED,
            (a, b) => a.or(b).unwrap_or(0),
        };
        if let Some(e) = self.memo.get_mut(slot) {
            *e = generation << 32 | u64::from(group);
        }
        group
    }

    fn window_of(&self, group: u32) -> Option<Handle> {
        self.windows.get(group.checked_sub(1)? as usize).copied()
    }

    /// The Window of `n`'s group: its document's, then its opaque root's.
    fn window(&mut self, dom: &snowlark_dom::Dom, n: NodeId) -> Option<Handle> {
        match self.group(dom, n) {
            0 => None,
            MIXED => self.window_of(*self.docs.get(&dom.node_document(n))?),
            group => self.window_of(group),
        }
    }

    /// Whether `n`'s document or its opaque root is in `window`'s group.
    fn is_in(&mut self, dom: &snowlark_dom::Dom, n: NodeId, window: Handle) -> bool {
        match self.group(dom, n) {
            0 => false,
            MIXED => [dom.node_document(n), dom.opaque_root(n)].iter().any(|d| {
                self.docs
                    .get(d)
                    .and_then(|&g| self.window_of(g))
                    .is_some_and(|w| w == window)
            }),
            group => self.window_of(group) == Some(window),
        }
    }
}

/// Moves `joined` into `owned`, then every `waiting` object that refers (`targets`) to a member, and
/// so on until none joins. `waiting` is what is left.
fn join_referrers(
    owned: &mut FxHashMap<Handle, Handle>,
    mut joined: Vec<(Handle, Handle)>,
    waiting: &mut Vec<(Handle, std::ops::Range<usize>)>,
    targets: &[Handle],
) {
    while !joined.is_empty() {
        owned.extend(joined.drain(..));
        waiting.retain(|(h, refers_to)| {
            let group = targets[refers_to.clone()]
                .iter()
                .find_map(|t| owned.get(t).copied());
            if let Some(w) = group {
                joined.push((*h, w));
            }
            group.is_none()
        });
    }
}

impl Discarded {
    /// Whether `doc` is no browsing context's active document any more (it was discarded with its
    /// context, or replaced): engine state kept per document does not pin it, and it is freed with
    /// its tree once nothing else reaches it.
    fn inactive(&self, doc: &NodeId) -> bool {
        self.docs.contains_key(doc) || self.unloaded.contains(doc)
    }

    fn any(&self) -> bool {
        !self.docs.is_empty() || !self.unloaded.is_empty()
    }

    /// The objects of `window`'s group (the Window included).
    fn of_window(&self, window: Handle) -> Vec<Handle> {
        self.owned
            .iter()
            .filter(|&(_, &w)| w == window)
            .map(|(&h, _)| h)
            .collect()
    }
}

/// Handles and node ids (as bits), hashed with the bindings' handle hasher.
type FastSet = HashSet<u64, rt::HandleHash>;

/// Collection bookkeeping besides the wrappers.
#[derive(Default)]
pub struct GcState {
    /// Roots of subtrees removed since the last collection (candidates to free).
    pub detached: Vec<NodeId>,
    /// Collectable objects allocated since the last collection that had no wrapper yet.
    pub fresh: Vec<Handle>,
    /// Browsing contexts discarded since the last collection.
    pub discarded: Vec<super::frames::ContextId>,
    /// Windows a navigation replaced (with their documents) since the last collection: they become
    /// groups as discarded browsing contexts' Windows do.
    pub retired_pending: Vec<(Handle, NodeId)>,
    /// Those Windows and documents, until they are freed.
    pub retired: Vec<(Handle, NodeId)>,
    /// Documents a navigation replaced, until they are freed.
    pub unloaded: rustc_hash::FxHashSet<NodeId>,
    /// Documents a navigation replaced since the last collection, whose fetches are to be aborted.
    pub unloaded_pending: Vec<NodeId>,
    /// Objects whose wrappers a discarded browsing context's realm made, with that context's Window
    /// (they join its group: [`Engine::gc_contexts_discarded`]).
    pub realm_objects: FxHashMap<Handle, Handle>,
    /// Free object slots (indexes), reused by [`Engine::alloc`].
    pub free_slots: Vec<u32>,
    pub stats: GcStats,
    /// V8's collection count when the weak wrappers were last scanned.
    pub gcs_seen: u64,
    /// `SNOWLARK_GC_STATS=1` ([`Engine::gc_report`]): when the last report was printed.
    pub report: Option<Option<std::time::Instant>>,
    /// `SNOWLARK_HEAP_SNAPSHOT`: the heap snapshot was written.
    pub heap_snapshot_written: bool,
    /// When the last full collection ran ([`Engine::collect_garbage_after_task`]).
    pub last_full: Option<std::time::Instant>,
    /// Since when a collection after a task waits for a turn with no task ready
    /// ([`Engine::collect_garbage_after_task`], [`READY_WAIT`]).
    pub due_since: Option<std::time::Instant>,
    /// How long the last collection the event loop started took
    /// ([`Engine::collect_garbage_when_idle`] waits for that much idle time).
    pub last_cost: std::time::Duration,
    /// When every inactive document was last asked again ([`Engine::collect_garbage`]).
    pub inactive_asked: Option<std::time::Instant>,
    /// The sizes of the last snapshot's pin sets, to make the next ones at once.
    pub pin_sizes: (usize, usize),
    /// What the last snapshot went through ([`Snapshot::walked`]): the next collection waits for
    /// a backlog in proportion ([`WALKED_PER_CANDIDATE`]).
    pub last_walked: usize,
    /// The discarded browsing contexts' groups as the last snapshot found them, which the next
    /// ones bring up to date instead of walking every object again ([`Engine::discarded_realms`]).
    pub grouping: Option<Grouping>,
}

/// How long a [`Grouping`] is brought up to date before it is made again from every object.
///
/// Between two full walks, an object that comes to refer into a group after it was asked (an old
/// event dispatched at a removed frame's node) joins at the next one: until then its reference pins
/// the group, which lives that much longer. Nothing is freed sooner for it: a member keeps its whole
/// group while its wrapper lives, and what refers to a member from outside pins it.
const GROUPING_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// The groups of the discarded browsing contexts, kept between snapshots while the same documents
/// are discarded.
pub struct Grouping {
    /// The discarded documents it is for, with their Windows.
    docs: FxHashMap<NodeId, Handle>,
    owned: Rc<FxHashMap<Handle, Handle>>,
    made: Rc<rustc_hash::FxHashSet<Handle>>,
    /// When it was last made from every object.
    made_at: std::time::Instant,
    /// Collectable objects allocated since, which the next snapshot asks.
    pub allocated: Vec<Handle>,
    /// Objects freed since: they leave their group (their slot may hold another object next).
    pub freed: Vec<Handle>,
}

/// What collections freed, in total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub collections: u64,
    /// Passes over the pending wrappers (in a collection or asked for by V8's interrupt).
    pub passes: u64,
    pub nodes_freed: u64,
    pub objects_freed: u64,
    /// Turns after which a due collection waited because a task was ready.
    pub deferred: u64,
}

impl Engine {
    /// Whether an allocation of `o` must be tracked until it gets a wrapper.
    pub(crate) fn gc_collectable(o: &Object) -> bool {
        class(o) != Class::Strong
    }

    /// The documents whose nodes' wrappers are strong: active documents of browsing contexts.
    fn live_documents(&self) -> Option<FastSet> {
        let f = self.frames.try_borrow().ok()?;
        Some(
            f.list
                .iter()
                .filter(|c| !c.discarded)
                .map(|c| c.document.to_bits())
                .collect(),
        )
    }

    /// The discarded browsing contexts whose Window and document are not freed yet, and the objects
    /// of their realms that the engine keeps by document or Window.
    ///
    /// Finding which objects outside a group refer into it walks every object, so the groups are
    /// kept ([`GcState::grouping`]) and brought up to date with the objects allocated and freed
    /// since, while the same documents are discarded; they are made again from every object when
    /// another context is discarded or freed, when a realm's objects change and every
    /// [`GROUPING_FRESH_FOR`].
    fn discarded_realms(&self, dom: &snowlark_dom::Dom) -> Option<Discarded> {
        let mut d = Discarded {
            unloaded: self.gc.try_borrow().ok()?.unloaded.clone(),
            ..Default::default()
        };
        let f = self.frames.try_borrow().ok()?;
        for (id, c) in f.list.iter().enumerate() {
            if c.discarded && f.by_window.get(&c.window) == Some(&id) && dom.is_alive(c.document) {
                d.docs.insert(c.document, c.window);
                d.windows.insert(c.window, c.document);
            }
        }
        for &(w, doc) in &self.gc.try_borrow().ok()?.retired {
            if dom.is_alive(doc) {
                d.docs.insert(doc, w);
                d.windows.insert(w, doc);
            }
        }
        let kept = {
            let mut gc = self.gc.try_borrow_mut().ok()?;
            if d.docs.is_empty() {
                gc.grouping = None;
                return Some(d);
            }
            gc.grouping
                .take()
                .filter(|g| g.docs == d.docs && g.made_at.elapsed() < GROUPING_FRESH_FOR)
        };
        let objects = self.objects.try_borrow().ok()?;
        // The members found now, whose referents are asked below.
        let mut added: Vec<(Handle, Handle)> = Vec::new();
        let (made_at, mut allocated) = match kept {
            Some(mut g) => {
                d.owned = g.owned;
                d.made = g.made;
                if !g.freed.is_empty() {
                    let owned = Rc::make_mut(&mut d.owned);
                    let made = Rc::make_mut(&mut d.made);
                    for h in &g.freed {
                        owned.remove(h);
                        made.remove(h);
                    }
                }
                (Some(g.made_at), std::mem::take(&mut g.allocated))
            }
            None => (None, Vec::new()),
        };
        {
            let owned = Rc::make_mut(&mut d.owned);
            let mut add = |h: Handle, w: Handle| {
                if owned.insert(h, w).is_none() {
                    added.push((h, w));
                }
            };
            for &w in d.windows.keys() {
                add(w, w);
            }
            for (&(w, _), &h) in &f.singletons {
                if d.windows.contains_key(&w) {
                    add(h, w);
                }
            }
            let mut per_doc = |map: &mut dyn Iterator<Item = (&NodeId, &Handle)>| {
                for (doc, &h) in map {
                    if let Some(&w) = d.docs.get(doc) {
                        add(h, w);
                    }
                }
            };
            {
                let c = self.cssom.try_borrow().ok()?;
                per_doc(&mut c.sheet_lists.iter());
                per_doc(&mut c.font_sets.iter());
            }
            per_doc(&mut self.animations.try_borrow().ok()?.timelines.iter());
            per_doc(&mut self.selections.try_borrow().ok()?.iter());
            per_doc(&mut self.timing.try_borrow().ok()?.navigations.iter());
            per_doc(&mut self.view_transitions.try_borrow().ok()?.iter());
        }
        // The documents' animations (script's and CSS's), on their timelines.
        for &a in &self.animations.try_borrow().ok()?.list {
            let Some(Object::Animation(data)) = objects.get(a.get() as usize - 1) else {
                continue;
            };
            if let Some(&w) = data.timeline.and_then(|t| d.owned.get(&t)) {
                if Rc::make_mut(&mut d.owned).insert(a, w).is_none() {
                    added.push((a, w));
                }
            }
        }
        if made_at.is_none() {
            let owned = Rc::make_mut(&mut d.owned);
            let made = Rc::make_mut(&mut d.made);
            for (&h, &w) in &self.gc.try_borrow().ok()?.realm_objects {
                if d.windows.contains_key(&w) && !owned.contains_key(&h) {
                    owned.insert(h, w);
                    made.insert(h);
                    added.push((h, w));
                }
            }
        }
        // Objects of kinds that are freed, which the group's members refer to: theirs too (what
        // else refers to them still pins them, and through them the group).
        let mut stack = added;
        while let Some((h, w)) = stack.pop() {
            d.asked += 1;
            let Some(o) = objects.get(h.get() as usize - 1) else {
                continue;
            };
            let mut r = Refs::default();
            refs(o, &mut r);
            for t in r.objects {
                if d.owned.contains_key(&t) {
                    continue;
                }
                let collectable = objects
                    .get(t.get() as usize - 1)
                    .is_some_and(|o| !matches!(o, Object::Free) && class(o) != Class::Strong);
                if collectable {
                    Rc::make_mut(&mut d.owned).insert(t, w);
                    stack.push((t, w));
                }
            }
        }
        // Weak objects outside the group that refer into it (an event fired at its nodes, a
        // transition on its timeline, an observer entry for one of its elements): theirs too, so
        // that their references are internal and the group lives exactly while one of their
        // wrappers does. Otherwise such an object, kept by a wrapper in the group's own realm,
        // pins the group, and the group's strong wrappers keep that realm: a cycle V8 cannot see.
        // (Owned objects stay with their owner element.) A kept grouping asks only the objects
        // allocated since.
        let mut groups = NodeGroups::new(dom, &d.docs);
        let mut r = Refs::default();
        let mut joined = Vec::new();
        // (one walk: what could join later is what refers to objects, as nodes' groups stay)
        let mut waiting = Vec::new();
        let mut targets = Vec::new();
        let mut ask = |h: Handle, o: &Object| {
            if matches!(o, Object::Free) || class(o) != Class::Weak || d.owned.contains_key(&h) {
                return;
            }
            r.nodes.clear();
            r.objects.clear();
            r.live_roots.clear();
            refs(o, &mut r);
            let group = r
                .nodes
                .iter()
                .find_map(|&n| groups.window(dom, n))
                .or_else(|| r.objects.iter().find_map(|t| d.owned.get(t).copied()))
                .or_else(|| {
                    r.live_roots
                        .iter()
                        .find_map(|&(root, _)| groups.window(dom, root))
                });
            match group {
                Some(w) => joined.push((h, w)),
                None if !r.objects.is_empty() => {
                    let from = targets.len();
                    targets.extend_from_slice(&r.objects);
                    waiting.push((h, from..targets.len()));
                }
                None => {}
            }
        };
        let asked = if made_at.is_some() {
            allocated.len()
        } else {
            objects.len()
        };
        if made_at.is_some() {
            allocated.sort_unstable();
            allocated.dedup();
            for &h in &allocated {
                if let Some(o) = objects.get(h.get() as usize - 1) {
                    ask(h, o);
                }
            }
        } else {
            for (i, o) in objects.iter().enumerate() {
                ask(Handle::new(i as u64 + 1), o);
            }
        }
        if !joined.is_empty() {
            join_referrers(Rc::make_mut(&mut d.owned), joined, &mut waiting, &targets);
        }
        d.groups = groups;
        d.asked += asked;
        allocated.clear();
        self.gc.try_borrow_mut().ok()?.grouping = Some(Grouping {
            docs: d.docs.clone(),
            owned: d.owned.clone(),
            made: d.made.clone(),
            made_at: made_at.unwrap_or_else(std::time::Instant::now),
            allocated,
            freed: Vec::new(),
        });
        Some(d)
    }

    /// Everything outside JS that refers to nodes or collectable objects, as opaque roots and
    /// object handles. `None` when some state is borrowed (a pass then waits).
    fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot_traced(false)
    }

    /// [`Engine::snapshot`], recording with `trace` where each pin comes from (the GC report).
    fn snapshot_traced(&self, trace: bool) -> Option<Snapshot> {
        let dom = self.dom.try_borrow().ok()?;
        let mut discarded = self.discarded_realms(&dom)?;
        let mut groups = std::mem::take(&mut discarded.groups);
        let mut r = Refs::default();
        let mut t = trace.then(PinTrace::default);
        // Records what the block that started at `(nodes, objects)` added, as `label`'s.
        let mark = |t: &mut Option<PinTrace>,
                    r: &Refs,
                    from: (usize, usize),
                    label: &dyn Fn() -> String| {
            if let Some(t) = t {
                if r.nodes.len() > from.0 || r.objects.len() > from.1 {
                    let l = label();
                    t.nodes
                        .extend(r.nodes[from.0..].iter().map(|&n| (n, l.clone())));
                    t.objects
                        .extend(r.objects[from.1..].iter().map(|&h| (h, l.clone())));
                }
            }
        };
        let at = |r: &Refs| (r.nodes.len(), r.objects.len());
        let kind = |h: Handle| {
            format!(
                "{:?}#{}",
                <Engine as rt::Host>::interface_of(self, h),
                h.get()
            )
        };
        {
            let objects = self.objects.try_borrow().ok()?;
            if discarded.owned.is_empty() && t.is_none() {
                for o in objects.iter() {
                    refs(o, &mut r);
                }
            } else {
                for (i, o) in objects.iter().enumerate() {
                    let from = at(&r);
                    let h = Handle::new(i as u64 + 1);
                    let Some(&w) = discarded.owned.get(&h) else {
                        refs(o, &mut r);
                        if t.is_some() && !matches!(o, Object::Free) {
                            mark(&mut t, &r, from, &|| kind(h));
                        }
                        continue;
                    };
                    // A discarded browsing context's group lives and dies as one: references
                    // between its members do not pin it.
                    let mut own = Refs::default();
                    refs(o, &mut own);
                    // (a node of the context's document, in its tree or detached, is the group's)
                    let mut ours = |n: NodeId| groups.is_in(&dom, n, w);
                    r.nodes.extend(own.nodes.into_iter().filter(|&n| !ours(n)));
                    r.objects.extend(
                        own.objects
                            .into_iter()
                            .filter(|t| discarded.owned.get(t) != Some(&w)),
                    );
                    r.live_roots
                        .extend(own.live_roots.into_iter().filter(|&(root, _)| !ours(root)));
                    mark(&mut t, &r, from, &|| {
                        format!("{} (of a discarded group)", kind(h))
                    });
                }
            }
        }
        {
            let from = at(&r);
            let l = self.loader.try_borrow().ok()?;
            for j in &l.jobs {
                match j.purpose {
                    crate::loader::Purpose::Script(n)
                    | crate::loader::Purpose::Stylesheet(n)
                    | crate::loader::Purpose::SvgImage(n) => r.nodes.push(n),
                    _ => {}
                }
                r.nodes.extend(j.document);
            }
            mark(&mut t, &r, from, &|| "loader jobs".into());
        }
        // The state the engine keeps for active documents (their browsing contexts, focus, loading,
        // pointer and scrolling state...): it does not pin a discarded browsing context's document.
        let per_document = r.nodes.len();
        {
            let from = at(&r);
            let f = self.frames.try_borrow().ok()?;
            for c in f.list.iter().filter(|c| !c.discarded) {
                r.nodes.push(c.document);
                r.nodes.extend(c.container);
            }
            mark(&mut t, &r, from, &|| "browsing contexts".into());
        }
        {
            let from = at(&r);
            let h = self.html.try_borrow().ok()?;
            // (a document without a browsing context, one `createHTMLDocument` or `DOMParser` made,
            // lives while script reaches it: its state here does not keep it, nor the realm its
            // wrappers were made in; jQuery makes one per `parseHTML`)
            for (&d, s) in &h.docs {
                if s.has_browsing_context {
                    r.nodes.push(d);
                }
                r.nodes.extend(s.focused);
            }
            r.nodes.extend(&h.clicking);
            r.nodes.extend(&h.popovers);
            r.nodes.extend(&h.activating_labels);
            r.nodes.extend(&h.constructing_entry_list);
            r.nodes.extend(&h.firing_submission);
            for v in h.element_refs.values() {
                r.nodes.extend(v);
            }
            for &k in h.internals.keys() {
                r.nodes.push(k);
            }
            mark(&mut t, &r, from, &|| {
                "HTML state (documents, focus, popovers, refs)".into()
            });
        }
        {
            let from = at(&r);
            let c = self.custom.try_borrow().ok()?;
            for q in &c.stack {
                r.nodes.extend(q);
            }
            r.nodes.extend(&c.backup);
            for (&n, q) in &c.queues {
                if !q.is_empty() {
                    r.nodes.push(n);
                }
            }
            for d in &c.definitions {
                if let Ok(s) = d.construction_stack.try_borrow() {
                    r.nodes.extend(s.iter().flatten());
                }
            }
            mark(&mut t, &r, from, &|| "custom element reactions".into());
        }
        {
            let from = at(&r);
            let l = self.loader.try_borrow().ok()?;
            for (&n, s) in &l.scripts {
                r.nodes.push(n);
                r.nodes.extend(s.parser_document);
                r.nodes.extend(s.preparation_document);
            }
            for (&d, load) in &l.docs {
                r.nodes.push(d);
                r.nodes.extend(load.pending_parsing_blocking);
                r.nodes.extend(&load.defer_scripts);
                r.nodes.extend(&load.in_order_scripts);
                r.nodes.extend(&load.asap_scripts);
            }
            mark(&mut t, &r, from, &|| {
                "loading state (scripts, documents)".into()
            });
        }
        {
            let from = at(&r);
            let p = self.pointer.try_borrow().ok()?;
            r.nodes.extend(p.over);
            r.nodes.extend(&p.hovered);
            r.nodes.extend(p.pressed.map(|(n, _)| n));
            r.nodes.extend(&p.active);
            r.nodes.extend(p.may_drag.map(|d| d.source));
            if let Some(d) = &p.drag {
                r.nodes.extend(d.source);
                r.nodes.extend(d.immediate);
                r.nodes.extend(d.target);
                for i in &d.items {
                    if let crate::engine::datatransfer::ItemData::File(f) = i.data {
                        r.objects.push(f);
                    }
                }
            }
            mark(&mut t, &r, from, &|| "pointer state".into());
        }
        {
            let from = at(&r);
            let st = self.render.try_borrow().ok()?;
            r.nodes.extend(st.document);
            r.nodes.extend(&st.pending_scroll);
            r.nodes.extend(&st.pending_scrollend);
            r.nodes.extend(st.smooth.iter().map(|s| s.scroller.key()));
            mark(&mut t, &r, from, &|| "render state".into());
        }
        if discarded.any() {
            let of_active: Vec<NodeId> = r
                .nodes
                .drain(per_document..)
                .filter(|&n| {
                    // (a node of an inactive document, or in one's tree: template contents and
                    // shadow trees have their own node document)
                    !(dom.is_alive(n)
                        && (discarded.inactive(&dom.node_document(n))
                            || discarded.inactive(&dom.opaque_root(n))))
                })
                .collect();
            r.nodes.extend(of_active);
        }
        let from = at(&r);
        r.nodes
            .extend(&self.observers.try_borrow().ok()?.signal_slots);
        mark(&mut t, &r, from, &|| "slot change signals".into());
        let from = at(&r);
        r.nodes
            .extend(self.inserted_batch.try_borrow().ok()?.iter());
        mark(&mut t, &r, from, &|| "insertion batch".into());
        let from = at(&r);
        self.undo.try_borrow().ok()?.gc_nodes(&dom, &mut r.nodes);
        mark(&mut t, &r, from, &|| "undo steps".into());
        let from = at(&r);
        r.nodes.extend(self.current_script.get());
        r.opt(self.current_event.get());
        mark(&mut t, &r, from, &|| "current script / event".into());
        let references = r.nodes.len() + r.objects.len() + r.live_roots.len();
        let sizes = self
            .gc
            .try_borrow()
            .map(|g| g.pin_sizes)
            .unwrap_or_default();
        let mut pinned_roots = FastSet::with_capacity_and_hasher(sizes.0, Default::default());
        // (traced: the pinning nodes that survived the filters above)
        let pinning: rustc_hash::FxHashSet<NodeId> = if t.is_some() {
            r.nodes.iter().copied().collect()
        } else {
            Default::default()
        };
        // (by slot, the generation of a node already counted: the same nodes come many times)
        let mut counted = vec![0u32; dom.node_slots()];
        for n in r.nodes {
            let seen = counted.get_mut(n.index() as usize);
            if seen.as_deref() != Some(&n.generation()) && dom.is_alive(n) {
                pinned_roots.insert(dom.opaque_root(n).to_bits());
                if let Some(seen) = seen {
                    *seen = n.generation();
                }
            }
        }
        let mut live_pins: Vec<(NodeId, NodeId)> = Vec::new();
        // A live collection's root pins only when it is outside its anchor's group.
        // (and not when both are nodes of the same inactive document: one group)
        for root in r.live_roots {
            let (root, anchor) = root;
            if dom.is_alive(root) && dom.is_alive(anchor) {
                let r = dom.opaque_root(root);
                let same_inactive = discarded.inactive(&dom.node_document(root))
                    && dom.node_document(root) == dom.node_document(anchor);
                if r != dom.opaque_root(anchor) && !same_inactive {
                    pinned_roots.insert(r.to_bits());
                    if t.is_some() {
                        live_pins.push((root, anchor));
                    }
                }
            }
        }
        let mut pinned_objects =
            FastSet::with_capacity_and_hasher(sizes.1.max(r.objects.len()), Default::default());
        pinned_objects.extend(r.objects.iter().map(|h| h.get()));
        // (a discarded browsing context's animations never run again: they do not pin its group)
        let animation_pins: Vec<Handle> = self
            .animation_pins()?
            .into_iter()
            .filter(|h| !discarded.owned.contains_key(h))
            .collect();
        pinned_objects.extend(animation_pins.iter().map(|h| h.get()));
        // (as are a discarded browsing context's media query lists: their document is not fully
        // active again, so no `change` event is fired at them)
        pinned_objects.extend(
            self.media_list_pins()?
                .into_iter()
                .filter(|h| !discarded.owned.contains_key(h))
                .map(|h| h.get()),
        );
        // An event being dispatched stays (the dispatch reads it after each listener).
        let objects = self.objects.try_borrow().ok()?;
        let mut dispatching = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            if let Object::Event(e) = o {
                if e.dispatching {
                    pinned_objects.insert(i as u64 + 1);
                    dispatching.push(Handle::new(i as u64 + 1));
                }
            }
        }
        let walked = references + objects.len() + discarded.asked;
        drop(objects);
        // Traced: for each inactive document that is pinned and each pinned member of a discarded
        // group, every source that pins it.
        let mut explain = Vec::new();
        if let Some(t) = t {
            let inactive: Vec<NodeId> = discarded
                .docs
                .keys()
                .chain(&discarded.unloaded)
                .copied()
                .collect();
            for doc in inactive {
                if !pinned_roots.contains(&doc.to_bits()) {
                    continue;
                }
                let mut by: std::collections::BTreeMap<String, usize> = Default::default();
                for (n, label) in &t.nodes {
                    if pinning.contains(n) && dom.is_alive(*n) && dom.opaque_root(*n) == doc {
                        *by.entry(label.clone()).or_default() += 1;
                    }
                }
                for &(root, anchor) in &live_pins {
                    if dom.opaque_root(root) == doc {
                        *by.entry(format!(
                            "live collection anchored at {}",
                            kind(node_handle(anchor))
                        ))
                        .or_default() += 1;
                    }
                }
                let what = if discarded.docs.contains_key(&doc) {
                    "removed frame's"
                } else {
                    "replaced"
                };
                explain.push(format!(
                    "{what} document {} ({}) pinned by {by:?}",
                    doc.to_bits(),
                    dom.document_url(doc)
                ));
            }
            for &h in discarded.owned.keys() {
                if !pinned_objects.contains(&h.get()) {
                    continue;
                }
                let mut by: Vec<String> = t
                    .objects
                    .iter()
                    .filter(|(o, _)| *o == h)
                    .map(|(_, l)| l.clone())
                    .collect();
                if animation_pins.contains(&h) {
                    by.push("animations".into());
                }
                if dispatching.contains(&h) {
                    by.push("an event dispatch".into());
                }
                explain.push(format!("group member {} pinned by {by:?}", kind(h)));
            }
        }
        if let Ok(mut g) = self.gc.try_borrow_mut() {
            g.pin_sizes = (pinned_roots.len(), pinned_objects.len());
            g.last_walked = walked;
        }
        Some(Snapshot {
            live_docs: self.live_documents()?,
            pinned_roots,
            pinned_objects,
            discarded,
            explain,
            walked,
        })
    }

    /// The pass policy for one pending wrapper.
    fn keep(&self, snap: &Snapshot, h: Handle) -> Keep {
        let Ok(dom) = self.dom.try_borrow() else {
            return Keep::Pinned;
        };
        if let Some(n) = handle_node(h) {
            if !dom.is_alive(n) {
                return Keep::Weak;
            }
            let root = dom.opaque_root(n);
            if snap.live_docs.contains(&root.to_bits()) {
                Keep::Strong
            } else if snap.pinned_roots.contains(&root.to_bits()) {
                Keep::Pinned
            } else if let Some(&window) = snap.discarded.docs.get(&root) {
                Keep::Anchored(window)
            } else {
                Keep::Anchored(node_handle(root))
            }
        } else {
            if let Some(&window) = snap.discarded.owned.get(&h) {
                return if snap.pinned_objects.contains(&h.get()) {
                    Keep::Pinned
                } else {
                    Keep::Anchored(window)
                };
            }
            let Ok(objects) = self.objects.try_borrow() else {
                return Keep::Pinned;
            };
            let Some(o) = objects.get(h.get() as usize - 1) else {
                return Keep::Weak;
            };
            match class(o) {
                Class::Strong if !matches!(o, Object::Free) => Keep::Strong,
                Class::Strong => Keep::Weak,
                Class::Owned(owner) if dom.is_alive(owner) => Keep::Anchored(node_handle(owner)),
                Class::Owned(_) => Keep::Weak,
                Class::Weak if snap.pinned_objects.contains(&h.get()) => Keep::Pinned,
                Class::Weak => Keep::Weak,
            }
        }
    }

    /// A pass over the pending wrappers. Returns false when some state was borrowed (the pass waits).
    pub(crate) fn gc_pass(&self, scope: &mut v8::PinScope<'_, '_>) -> bool {
        self.gc_pass_snapshot(scope).is_some()
    }

    /// [`Engine::gc_pass`], with the snapshot it took if any wrapper was pending. It is still true
    /// after the pass, which runs no script and only changes how wrappers and their values are held.
    /// `None` when some state was borrowed.
    fn gc_pass_snapshot(&self, scope: &mut v8::PinScope<'_, '_>) -> Option<Option<Snapshot>> {
        if !rt::has_pending(scope).0 {
            return Some(None);
        }
        let Some(snap) = self.snapshot() else {
            rt::defer_pass(scope);
            return None;
        };
        rt::pace_passes(scope, snap.walked / WALKED_PER_WRAPPER);
        self.gc.borrow_mut().stats.passes += 1;
        let weakened = rt::pass(scope, &mut |h| self.keep(&snap, h));
        for h in weakened {
            self.gc_trace_callbacks(scope, h);
            self.gc_trace_event_values(scope, h);
        }
        Some(Some(snap))
    }

    /// `SNOWLARK_GC_STATS=1`: after a V8 collection (at most every two seconds), a report on stderr
    /// of the discarded browsing contexts and replaced documents not freed yet, and of what outside
    /// JS holds into their realms (strong wrappers made in them, listener, handler and observer
    /// callbacks, custom element definitions), by kind.
    fn gc_report(&self, scope: &mut v8::PinScope<'_, '_>, snap: &Snapshot) {
        let now = std::time::Instant::now();
        {
            let mut gc = self.gc.borrow_mut();
            let on = *gc.report.get_or_insert_with(|| {
                std::env::var("SNOWLARK_GC_STATS")
                    .is_ok_and(|v| v == "1")
                    .then_some(now - std::time::Duration::from_secs(60))
            });
            match on {
                Some(last) if now.duration_since(last).as_secs() >= 2 => {
                    gc.report = Some(Some(now));
                }
                _ => return,
            }
        }
        let _ = snap;
        if let Some(snap) = self.snapshot_traced(true) {
            let mut out = Vec::new();
            self.gc_report_lines(scope, &snap, &mut out);
            for line in out {
                eprintln!("{line}");
            }
        }
    }

    /// The [`Engine::gc_report`] lines now (tests: why a removed frame stays). False when some
    /// state is borrowed.
    pub fn gc_report_now(&self, cx: &mut Cx) -> bool {
        let Some(lines) = self.gc_report_text(cx) else {
            return false;
        };
        for line in lines {
            eprintln!("{line}");
        }
        true
    }

    fn gc_report_lines(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        snap: &Snapshot,
        out: &mut Vec<String>,
    ) {
        use std::collections::BTreeMap;
        let d = &snap.discarded;
        let heap = scope.get_heap_statistics().used_heap_size() >> 20;
        let mut urls: BTreeMap<String, usize> = BTreeMap::new();
        {
            let dom = self.dom();
            for doc in d.docs.keys() {
                *urls.entry(dom.document_url(*doc).to_owned()).or_default() += 1;
            }
        }
        let (nodes, objects) = self.live_counts();
        out.push(format!(
            "[gc] heap {heap} MiB, {nodes} nodes, {objects} objects; discarded frames alive {}: {:?}; replaced documents alive {}",
            d.docs.len(),
            urls,
            d.unloaded.len()
        ));
        if d.windows.is_empty() {
            self.gc_report_why(scope, snap, out);
            return;
        }
        let of_discarded = |scope: &v8::PinScope<'_, '_>, o: v8::Local<v8::Object>| {
            rt::creation_realm(scope, o).is_some_and(|w| d.windows.contains_key(&w))
        };
        let name = |h: Handle| format!("{:?}", <Engine as rt::Host>::interface_of(self, h));
        let mut wrappers: BTreeMap<String, usize> = BTreeMap::new();
        let mut strong = Vec::new();
        rt::for_each_strong(scope, |h, o| strong.push((h, v8::Global::new(scope, o))));
        for (h, o) in strong {
            let o = v8::Local::new(scope, &o);
            if of_discarded(scope, o) {
                *wrappers.entry(name(h)).or_default() += 1;
            }
        }
        let mut listeners: BTreeMap<String, usize> = BTreeMap::new();
        let mut handlers = 0;
        {
            use super::events::{Callback, Handler};
            for (&t, list) in self.listeners.borrow().iter() {
                for l in list {
                    if let Some(Callback::Strong(c)) = &*l.callback.borrow() {
                        if of_discarded(scope, v8::Local::new(scope, c)) {
                            *listeners.entry(name(t)).or_default() += 1;
                        }
                    }
                }
            }
            for h in self.handlers.borrow().values() {
                if let Handler::Compiled(Callback::Strong(c), _) = h {
                    if of_discarded(scope, v8::Local::new(scope, c)) {
                        handlers += 1;
                    }
                }
            }
        }
        let mut observers: BTreeMap<&str, usize> = BTreeMap::new();
        {
            let objects = self.objects.borrow();
            for o in objects.iter() {
                let (kind, cb) = match o {
                    Object::MutationObserver(m) => ("MutationObserver", &m.callback),
                    Object::ResizeObserver(r) => ("ResizeObserver", &r.callback),
                    Object::IntersectionObserver(i) => ("IntersectionObserver", &i.callback),
                    Object::PerformanceObserver(p) => ("PerformanceObserver", &p.callback),
                    _ => continue,
                };
                let super::held::Held::Strong(cb) = cb else {
                    continue;
                };
                if of_discarded(scope, v8::Local::new(scope, cb)) {
                    *observers.entry(kind).or_default() += 1;
                }
            }
        }
        let definitions = self
            .custom
            .borrow()
            .definitions
            .iter()
            .filter(|def| match &def.js.borrow().constructor {
                super::held::Held::Strong(c) => of_discarded(scope, v8::Local::new(scope, c)),
                super::held::Held::Traced(_) => false,
            })
            .count();
        out.push(format!(
            "[gc]   held from outside JS: strong wrappers {wrappers:?}; listeners on {listeners:?}; handlers {handlers}; observers {observers:?}; custom element definitions {definitions}"
        ));
        let values = self.gc_strong_values_census(scope, &of_discarded);
        out.push(format!(
            "[gc]   strong JS values the engine keeps of those realms, by field: {values:?}"
        ));
        self.gc_report_why(scope, snap, out);
    }

    /// The JS values the engine keeps strongly, in its objects and its own state, that `of_realm`
    /// says are of the realms in question (objects made there), counted by where they are kept: a
    /// strong handle to an object of a removed frame's realm keeps that realm, and through it the
    /// frame, alive (the leaks it found: event details, policy callbacks, module records). Streams'
    /// underlying sources and sinks are not covered.
    fn gc_strong_values_census(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        of_realm: &dyn Fn(&v8::PinScope<'_, '_>, v8::Local<v8::Object>) -> bool,
    ) -> std::collections::BTreeMap<&'static str, usize> {
        use super::held::Held;
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut count =
            |scope: &mut v8::PinScope<'_, '_>, label: &'static str, v: v8::Local<v8::Value>| {
                if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
                    if of_realm(scope, o) {
                        *counts.entry(label).or_default() += 1;
                    }
                }
            };
        fn held<'s, T>(scope: &v8::PinScope<'s, '_>, h: &Held<T>) -> Option<v8::Local<'s, T>> {
            match h {
                Held::Strong(g) => Some(v8::Local::new(scope, g)),
                Held::Traced(_) => None,
            }
        }
        fn value<'s>(
            scope: &v8::PinScope<'s, '_>,
            g: &v8::Global<v8::Value>,
        ) -> v8::Local<'s, v8::Value> {
            v8::Local::new(scope, g)
        }
        {
            use super::events::EventExt;
            let objects = self.objects.borrow();
            for o in objects.iter() {
                match o {
                    Object::Event(e) => {
                        let (label, v): (&'static str, Option<&Held<v8::Value>>) = match &e.ext {
                            EventExt::Custom(v) => ("CustomEvent.detail", v.as_ref()),
                            EventExt::PopState(v) => ("PopStateEvent.state", v.as_ref()),
                            EventExt::Error { error, .. } => ("ErrorEvent.error", error.as_ref()),
                            EventExt::Message { data, .. } => ("MessageEvent.data", data.as_ref()),
                            EventExt::Rejection { promise, reason } => {
                                if let Some(p) = held(scope, promise) {
                                    count(scope, "PromiseRejectionEvent.promise", p.into());
                                }
                                ("PromiseRejectionEvent.reason", reason.as_ref())
                            }
                            _ => continue,
                        };
                        if let Some(v) = v.and_then(|v| held(scope, v)) {
                            count(scope, label, v);
                        }
                    }
                    Object::AbortSignal(a) => {
                        if let Some(r) = a.reason.as_ref().and_then(|r| held(scope, r)) {
                            count(scope, "AbortSignal.reason", r);
                        }
                    }
                    Object::PerformanceEntry(e) => {
                        if let Some(d) = &e.detail {
                            let v = value(scope, d.global());
                            count(scope, "PerformanceEntry.detail", v);
                        }
                    }
                    Object::ImageData(i) => {
                        let v = v8::Local::new(scope, i.data.global());
                        count(scope, "ImageData.data", v.into());
                    }
                    Object::ClipboardItem(c) => {
                        for (_, item) in &c.items {
                            let v = v8::Local::new(scope, item.global());
                            count(scope, "ClipboardItem items", v.into());
                        }
                    }
                    Object::ViewTransition(t) => {
                        for p in [&t.update_callback_done, &t.ready, &t.finished] {
                            let r = v8::Local::new(scope, p.resolver());
                            let promise = r.get_promise(scope);
                            count(scope, "ViewTransition promises", promise.into());
                        }
                    }
                    Object::TrustedTypePolicy(p) => {
                        for cb in [&p.html, &p.script, &p.script_url, &p.parser_options]
                            .into_iter()
                            .flatten()
                        {
                            if let Some(c) = held(scope, cb) {
                                count(scope, "TrustedTypePolicy callbacks", c.into());
                            }
                        }
                    }
                    Object::Xhr(x) => {
                        if let Some(v) = x.response_value.as_ref().and_then(|v| held(scope, v)) {
                            count(scope, "XMLHttpRequest.response", v);
                        }
                    }
                    Object::ReadableStream(r) => {
                        if let Some(e) = &r.stored_error {
                            let v = value(scope, e.global());
                            count(scope, "ReadableStream stored error", v);
                        }
                    }
                    Object::ReadableController(c) => {
                        for (chunk, _) in &c.queue {
                            let v = value(scope, chunk.global());
                            count(scope, "ReadableStream queue", v);
                        }
                        if let Some(f) = &c.size {
                            let v = v8::Local::new(scope, f);
                            count(scope, "ReadableStream size algorithm", v.into());
                        }
                    }
                    _ => {}
                }
            }
        }
        {
            use crate::event_loop::TimerHandler;
            let el = self.event_loop.borrow();
            for t in el.timers.values() {
                if let TimerHandler::Function(f, args, _) = &t.handler {
                    let v = v8::Local::new(scope, f.global());
                    count(scope, "timers", v.into());
                    for a in args {
                        let v = value(scope, a.global());
                        count(scope, "timer arguments", v);
                    }
                }
            }
            for (_, f, _) in &el.raf {
                let v = v8::Local::new(scope, f.global());
                count(scope, "requestAnimationFrame callbacks", v.into());
            }
            for r in el.idle.iter().chain(&el.idle_runnable) {
                let v = v8::Local::new(scope, r.callback.global());
                count(scope, "requestIdleCallback callbacks", v.into());
            }
        }
        {
            let c = self.custom.borrow();
            for p in c.when_defined.values() {
                if let Some(r) = held(scope, p) {
                    let promise = r.get_promise(scope);
                    count(scope, "customElements.whenDefined promises", promise.into());
                }
            }
        }
        {
            let h = self.history.borrow();
            for s in h.states.values() {
                if let Some(v) = held(scope, s) {
                    count(scope, "history states", v);
                }
            }
        }
        {
            let fonts: Vec<v8::Global<v8::PromiseResolver>> = self
                .cssom
                .borrow()
                .fonts_ready
                .values()
                .map(|p| p.resolver().clone())
                .collect();
            for r in fonts {
                let r = v8::Local::new(scope, &r);
                let promise = r.get_promise(scope);
                count(scope, "document.fonts.ready promises", promise.into());
            }
        }
        {
            let functions: Vec<v8::Global<v8::Function>> = self
                .streams
                .borrow()
                .size_functions
                .values()
                .cloned()
                .collect();
            for f in functions {
                let v = v8::Local::new(scope, &f);
                count(scope, "queuing strategy size functions", v.into());
            }
        }
        {
            let pending: Vec<v8::Global<v8::Promise>> = self
                .about_to_be_notified
                .borrow()
                .iter()
                .map(|r| r.promise.clone())
                .collect();
            for p in pending {
                let v = v8::Local::new(scope, &p);
                count(scope, "rejections about to be notified", v.into());
            }
        }
        counts
    }

    /// Why the inactive documents stay, exactly: the traced snapshot's pins (the engine side), then
    /// the live wrappers of each inactive document that nothing pins (what keeps those is on V8's
    /// side: `SNOWLARK_HEAP_SNAPSHOT=<file>` writes a heap snapshot for tools/heap-retainers).
    fn gc_report_why(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        snap: &Snapshot,
        out: &mut Vec<String>,
    ) {
        use std::collections::BTreeMap;
        let d = &snap.discarded;
        for line in &snap.explain {
            out.push(format!("[gc]   why: {line}"));
        }
        let name = |h: Handle| format!("{:?}", <Engine as rt::Host>::interface_of(self, h));
        let dom = self.dom();
        let mut unpinned: BTreeMap<String, usize> = BTreeMap::new();
        let mut docs = 0;
        for &doc in d.docs.keys().chain(&d.unloaded) {
            if snap.pinned_roots.contains(&doc.to_bits()) {
                continue;
            }
            docs += 1;
            for n in dom.owned_nodes(doc) {
                let state = rt::wrapper_state(scope, node_handle(n));
                if state != rt::WrapperState::None {
                    let kind = if state == rt::WrapperState::Strong {
                        "strong"
                    } else {
                        "weak"
                    };
                    *unpinned
                        .entry(format!("{} {kind}", name(node_handle(n))))
                        .or_default() += 1;
                }
            }
        }
        if docs > 0 {
            out.push(format!(
                "[gc]   {docs} inactive documents nothing in the engine pins, with live wrappers {unpinned:?} (held on V8's side: see SNOWLARK_HEAP_SNAPSHOT)"
            ));
        }
        drop(dom);
        // A heap snapshot, once, when eight removed or replaced frames are alive (not the one or two
        // a page's script still refers to for a while: frames that stayed).
        let path = std::env::var("SNOWLARK_HEAP_SNAPSHOT").ok();
        if let Some(path) = path.filter(|p| !p.is_empty()) {
            // (`SNOWLARK_HEAP_SNAPSHOT_FRAMES=<n>` waits for n instead)
            let frames = std::env::var("SNOWLARK_HEAP_SNAPSHOT_FRAMES")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(8);
            if d.windows.len() >= frames && !self.gc.borrow().heap_snapshot_written {
                self.gc.borrow_mut().heap_snapshot_written = true;
                out.push(
                    match self.write_heap_snapshot(scope, std::path::Path::new(&path)) {
                        Ok(()) => format!("[gc] heap snapshot written to {path}"),
                        Err(e) => format!("[gc] heap snapshot: cannot write {path}: {e}"),
                    },
                );
            }
        }
    }

    /// Writes a V8 heap snapshot (the .heapsnapshot format tools/heap-retainers reads) to `path`.
    pub fn write_heap_snapshot(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        path: &std::path::Path,
    ) -> std::io::Result<()> {
        use std::io::Write;
        let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut result = Ok(());
        scope.take_heap_snapshot(|chunk| match w.write_all(chunk) {
            Ok(()) => true,
            Err(e) => {
                result = Err(e);
                false
            }
        });
        result?;
        w.flush()
    }

    /// The [`Engine::gc_report_now`] lines, returned (a harness's report). None when some state is
    /// borrowed.
    pub fn gc_report_text(&self, cx: &mut Cx) -> Option<Vec<String>> {
        let snap = self.snapshot_traced(true)?;
        let mut out = Vec::new();
        self.gc_report_lines(cx.scope(), &snap, &mut out);
        Some(out)
    }

    /// The discarded browsing contexts and replaced Windows whose document is not freed yet, by
    /// document URL. None when some state is borrowed.
    pub fn frames_alive(&self) -> Option<std::collections::BTreeMap<String, usize>> {
        let dom = self.dom.try_borrow().ok()?;
        let d = self.discarded_realms(&dom)?;
        let mut urls = std::collections::BTreeMap::new();
        for doc in d.windows.values() {
            *urls.entry(dom.document_url(*doc).to_owned()).or_default() += 1;
        }
        Some(urls)
    }

    /// The JS values an event carries move to its wrapper, which is weak now
    /// ([`super::events::EventExt`]): a `detail` or `data` of the event's realm no longer keeps the
    /// realm, and with it the event, alive.
    fn gc_trace_event_values(&self, scope: &mut v8::PinScope<'_, '_>, h: Handle) {
        if handle_node(h).is_some() {
            return;
        }
        let Ok(mut objects) = self.objects.try_borrow_mut() else {
            return;
        };
        match objects.get_mut(h.get() as usize - 1) {
            Some(Object::Event(e)) => e.ext.trace(scope, h),
            Some(Object::AbortSignal(a)) => {
                if let Some(r) = a.reason.as_mut() {
                    r.trace(scope, h);
                }
            }
            _ => {}
        }
    }

    /// The callbacks of `target`'s event listeners and handlers move to its wrapper, which is weak
    /// now ([`super::events::Callback`]): a callback that reaches its target no longer keeps it.
    fn gc_trace_callbacks(&self, scope: &mut v8::PinScope<'_, '_>, target: Handle) {
        use super::events::{Callback, Handler};
        let list = match self.listeners.try_borrow() {
            Ok(l) => match l.get(&target) {
                Some(list) => list.clone(),
                None => return,
            },
            Err(_) => return,
        };
        let trace = |scope: &mut v8::PinScope<'_, '_>, cb: &mut Callback| cb.trace(scope, target);
        for l in &list {
            if let Ok(mut c) = l.callback.try_borrow_mut() {
                if let Some(cb) = c.as_mut() {
                    trace(scope, cb);
                }
            }
            if let Some(event) = l.handler {
                if let Ok(mut handlers) = self.handlers.try_borrow_mut() {
                    if let Some(Handler::Compiled(cb, _)) = handlers.get_mut(&(target, event)) {
                        trace(scope, cb);
                    }
                }
            }
        }
    }

    /// [`Engine::gc_trace_callbacks`] for a target given a callback now, when its wrapper is weak
    /// already.
    pub(crate) fn gc_trace_if_weak(&self, cx: &mut Cx, target: Handle) {
        let scope = cx.scope();
        if rt::wrapper_state(scope, target) == rt::WrapperState::Weak {
            if handle_node(target).is_none() && self.gc_keep_media_list(scope, target) {
                return;
            }
            self.gc_trace_callbacks(scope, target);
        }
    }

    /// A media query list with a listener is kept ([`Engine::media_list_pins`]): when `target` is
    /// one, its weak wrapper turns strong again, and the next pass finds it pinned. Returns whether
    /// it is one. Out of line: every listener added to a weak target comes by its caller.
    #[cold]
    #[inline(never)]
    fn gc_keep_media_list(&self, scope: &mut v8::PinScope<'_, '_>, target: Handle) -> bool {
        let list = matches!(
            self.objects.borrow().get(target.get() as usize - 1),
            Some(Object::MediaQueryList(_))
        );
        if list {
            rt::touch(scope, target);
        }
        list
    }

    /// A new wrapper for `h`: the wrapper that must stay strong until the next pass groups the new
    /// one with it (a detached node's opaque root, an owned object's owner).
    fn gc_on_wrap(&self, h: Handle) -> Option<Handle> {
        let dom = self.dom.try_borrow().ok()?;
        match handle_node(h) {
            Some(n) => {
                if !dom.is_alive(n) {
                    return None;
                }
                if dom.is_connected(n) {
                    let doc = dom.node_document(n);
                    if self.context_of_document(doc).is_some() {
                        return None;
                    }
                }
                let root = dom.opaque_root(n);
                (root != n).then(|| node_handle(root))
            }
            None => {
                let objects = self.objects.try_borrow().ok()?;
                match class(objects.get(h.get() as usize - 1)?) {
                    Class::Owned(owner) => Some(node_handle(owner)),
                    _ => None,
                }
            }
        }
    }

    /// A node with a wrapper moved into or out of a tree: its wrapper (and its new group's anchor)
    /// turn strong until the next pass. A removed subtree's root is a candidate to free.
    pub(crate) fn gc_node_moved(&self, cx: &mut Cx, node: NodeId, removed: bool) {
        if removed {
            self.gc.borrow_mut().detached.push(node);
        }
        let scope = cx.scope();
        if !rt::touch(scope, node_handle(node)) {
            return;
        }
        if !removed {
            let root = self.dom().opaque_root(node);
            if root != node {
                rt::hold(scope, node_handle(root));
            }
        }
    }

    /// Panics where `root` or a shadow-including descendant has a wrapper (nodes inserted by
    /// [`Engine::parsed_insertion`]).
    #[cfg(debug_assertions)]
    pub(crate) fn assert_unwrapped(&self, cx: &mut Cx, root: NodeId) {
        let dom = self.dom();
        for n in std::iter::once(root).chain(dom.shadow_including_descendants(root)) {
            assert!(
                rt::wrapper_state(cx.scope(), node_handle(n)) == rt::WrapperState::None,
                "a parsed node inserted by fragment parsing has a wrapper"
            );
        }
    }

    /// The wrappers of `root`'s shadow-including descendants moved with it (see
    /// [`Engine::gc_node_moved`]).
    pub(crate) fn gc_touch_descendants(&self, cx: &mut Cx, root: NodeId) {
        let dom = self.dom();
        rt::touch_all(
            cx.scope(),
            dom.shadow_including_descendants(root).map(node_handle),
        );
    }

    /// The wrappers of `nodes` (a removed node's descendants, as its removal collected them) moved
    /// with it, as [`Engine::gc_touch_descendants`].
    pub(crate) fn gc_touch_nodes(&self, cx: &mut Cx, nodes: &[NodeId]) {
        rt::touch_all(cx.scope(), nodes.iter().map(|&n| node_handle(n)));
    }

    /// A navigation replaced `doc` as its browsing context's active document: its nodes' wrappers
    /// are no longer kept by it, and it is freed with its tree once nothing reaches it (nothing makes
    /// a replaced document active again).
    pub(crate) fn gc_document_unloaded(&self, cx: &mut Cx, doc: NodeId) {
        {
            let dom = self.dom();
            rt::repend_where(cx.scope(), |h| {
                handle_node(h).is_some_and(|n| dom.is_alive(n) && dom.node_document(n) == doc)
            });
        }
        let mut gc = self.gc.borrow_mut();
        gc.unloaded.insert(doc);
        gc.unloaded_pending.push(doc);
        gc.detached.push(doc);
    }

    /// HTML "abort a document" (when it is unloaded or destroyed): the fetches in the context of
    /// `docs` (their subresources, their browsing contexts' navigations when `contexts`) and the API
    /// fetches of `windows`' realms (`made`: their objects, for their XMLHttpRequests) are cancelled.
    /// A fetch that never ends would otherwise keep its document, and its realm, alive.
    fn abort_document_fetches(
        &self,
        cx: &mut Cx,
        docs: &[NodeId],
        contexts: &[super::frames::ContextId],
        windows: &[Handle],
        made: &[(Handle, Handle)],
    ) {
        use crate::loader::Purpose;
        {
            let dom = self.dom();
            let of_docs = |n: NodeId| dom.is_alive(n) && docs.contains(&dom.node_document(n));
            self.loader.borrow_mut().jobs.retain(|j| {
                let node = match j.purpose {
                    Purpose::Script(n)
                    | Purpose::Stylesheet(n)
                    | Purpose::Preload(n)
                    | Purpose::SvgImage(n)
                    | Purpose::ModulePreload(n, _) => Some(n),
                    Purpose::Document(id) if contexts.contains(&id) => return false,
                    _ => None,
                };
                !(node.is_some_and(of_docs) || j.document.is_some_and(|d| docs.contains(&d)))
            });
        }
        if windows.is_empty() {
            return;
        }
        let mut ended = Vec::new();
        {
            let scope = cx.scope();
            self.api_jobs.borrow_mut().retain(|j| {
                let keep = match &j.owner {
                    super::fetch::JobOwner::Fetch {
                        realm: Some(realm), ..
                    } => {
                        let ctx = v8::Local::new(scope, realm);
                        !rt::realm_global(ctx).is_some_and(|w| windows.contains(&w))
                    }
                    super::fetch::JobOwner::Xhr(x) => !made.iter().any(|(h, _)| h == x),
                    _ => true,
                };
                if !keep {
                    if let (
                        Some(c),
                        super::fetch::JobOwner::Fetch {
                            realm: Some(realm), ..
                        },
                    ) = (j.body_controller, &j.owner)
                    {
                        ended.push((c, realm.clone()));
                    }
                }
                keep
            });
        }
        // Fetch "terminate" (the fetch group's fetches end): a body already given to script errors,
        // which settles its pending reads; they would otherwise keep the realm alive.
        for (c, realm) in ended {
            self.in_realm(cx, Some(realm), |cx| {
                let e = super::streams::type_error(cx, "The fetch was terminated");
                self.readable_controller_error(cx, c, e);
            });
        }
    }

    /// Frees an object slot.
    fn free_object(&self, h: Handle) {
        let old = {
            let mut objects = self.objects.borrow_mut();
            let Some(slot) = objects.get_mut(h.get() as usize - 1) else {
                return;
            };
            if matches!(slot, Object::Free) {
                return;
            }
            std::mem::replace(slot, Object::Free)
        };
        match old {
            Object::Range(_) => self.ranges.borrow_mut().retain(|&r| r != h),
            Object::NodeIterator(_) => self.iterators.borrow_mut().retain(|&r| r != h),
            Object::Animation(_) | Object::DocumentTimeline(_) => self.animation_freed(h),
            Object::Svg(ref o) => self.svg_freed(o),
            Object::EventTarget | Object::Idb(_) => {
                self.listeners.borrow_mut().remove(&h);
                self.handlers.borrow_mut().retain(|(t, _), _| *t != h);
            }
            Object::MediaQueryList(ref list) => {
                self.media_list_freed(list);
                // (most lists never had a listener: no walk of the handlers for those)
                if self.listeners.borrow_mut().remove(&h).is_some() {
                    self.handlers.borrow_mut().retain(|(t, _), _| *t != h);
                }
            }
            // (its definitions and pending `whenDefined()` promises go with it: they are keyed by
            // its handle, which the next registry allocated may take)
            Object::CustomElementRegistry => {
                let mut c = self.custom.borrow_mut();
                c.definitions.retain(|d| d.registry != h);
                c.when_defined.retain(|(r, _), _| *r != h);
            }
            _ => {}
        }
        drop(old);
        let mut gc = self.gc.borrow_mut();
        // (the next object in this slot is not of that realm)
        gc.realm_objects.remove(&h);
        if let Some(g) = &mut gc.grouping {
            g.freed.push(h);
        }
        gc.free_slots.push(h.get() as u32 - 1);
        gc.stats.objects_freed += 1;
    }

    /// Drops the side-table entries of nodes about to be freed.
    fn purge_nodes(&self, freed: &rustc_hash::FxHashSet<NodeId>) {
        let is_freed = |h: &Handle| handle_node(*h).is_some_and(|n| freed.contains(&n));
        self.listeners.borrow_mut().retain(|h, _| !is_freed(h));
        self.handlers.borrow_mut().retain(|(h, _), _| !is_freed(h));
        self.hidden_nonces
            .borrow_mut()
            .retain(|n, _| !freed.contains(n));
        self.cssom
            .borrow_mut()
            .unrendered
            .retain(|n, _| !freed.contains(n));
        {
            let mut html = self.html.borrow_mut();
            html.docs.retain(|n, _| !freed.contains(n));
            html.controls.retain(|n, _| !freed.contains(n));
            html.dialog_return.retain(|n, _| !freed.contains(n));
            html.element_refs.retain(|(n, _), _| !freed.contains(n));
            html.files.retain(|n, _| !freed.contains(n));
            html.src_objects.retain(|n, _| !freed.contains(n));
            html.value_at_focus.retain(|n, _| !freed.contains(n));
            html.edited_since_focus.retain(|n| !freed.contains(n));
        }
        self.cssom
            .borrow_mut()
            .inline
            .retain(|n, _| !freed.contains(n));
        self.svg.borrow_mut().nodes_freed(freed);
        self.dom_mut().forget_animated_attrs(|n| freed.contains(&n));
        {
            let mut c = self.custom.borrow_mut();
            c.element_definition.retain(|n, _| !freed.contains(n));
            c.queues.retain(|n, _| !freed.contains(n));
            c.interface_override.retain(|n, _| !freed.contains(n));
        }
        let observers: Vec<Handle> = {
            let mut o = self.observers.borrow_mut();
            o.registered.retain(|n, _| !freed.contains(n));
            o.all.clone()
        };
        {
            let mut objects = self.objects.borrow_mut();
            for h in observers {
                if let Some(Object::MutationObserver(m)) = objects.get_mut(h.get() as usize - 1) {
                    m.nodes.retain(|n| !freed.contains(n));
                }
            }
        }
        {
            let mut s = self.slots.borrow_mut();
            let s = &mut *s;
            s.manual.retain(|n, _| !freed.contains(n));
            s.assigned.retain(|n, _| !freed.contains(n));
            for v in s.manual.values_mut().chain(s.assigned.values_mut()) {
                v.retain(|n| !freed.contains(n));
            }
        }
        self.intersection_nodes_freed(freed);
        self.media_nodes_freed(freed);
        self.resize_nodes_freed(freed);
        self.for_each_renderer(|r, st| {
            st.element_scroll.retain(|n, _| !freed.contains(n));
            r.nodes_freed(freed);
        });
        self.frame_renders
            .borrow_mut()
            .retain(|d, _| !freed.contains(d));
        self.error_pages
            .borrow_mut()
            .retain(|d, _| !freed.contains(d));
        self.selections
            .borrow_mut()
            .retain(|d, _| !freed.contains(d));
        self.controls_dirty
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.controls_focused
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.form_states_dirty
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.form_states_dirty_script
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.form_states_dirty_detached
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.form_states
            .borrow_mut()
            .retain(|n, _| !freed.contains(n));
        self.canvas_dirty
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.canvases.borrow_mut().retain(|n, _| !freed.contains(n));
        self.xml_documents
            .borrow_mut()
            .retain(|n| !freed.contains(n));
        self.history
            .borrow_mut()
            .states
            .retain(|n, _| !freed.contains(n));
        {
            let mut l = self.loader.borrow_mut();
            l.scripts.retain(|n, _| !freed.contains(n));
            l.docs.retain(|n, _| !freed.contains(n));
        }
        {
            let mut html = self.html.borrow_mut();
            html.internals.retain(|n, _| !freed.contains(n));
            html.popovers.retain(|n| !freed.contains(n));
        }
        {
            let p = &mut *self.popover.borrow_mut();
            p.data.retain(|n, _| !freed.contains(n));
            p.toggle_tracker.retain(|n, _| !freed.contains(n));
            for map in [&mut p.auto, &mut p.hint] {
                map.retain(|n, _| !freed.contains(n));
                for l in map.values_mut() {
                    l.retain(|n| !freed.contains(n));
                }
            }
            p.pointerdown_target.retain(|n, _| !freed.contains(n));
        }
        {
            let mut p = self.pointer.borrow_mut();
            if p.over.is_some_and(|n| freed.contains(&n)) {
                p.over = None;
            }
            p.hovered.retain(|n| !freed.contains(n));
            if p.pressed.is_some_and(|(n, _)| freed.contains(&n)) {
                p.pressed = None;
            }
            p.active.retain(|n| !freed.contains(n));
        }
        {
            let mut st = self.render.borrow_mut();
            st.pending_scroll.retain(|n| !freed.contains(n));
            st.pending_scrollend.retain(|n| !freed.contains(n));
            st.smooth.retain(|s| !freed.contains(&s.scroller.key()));
        }
        self.gc.borrow_mut().unloaded.retain(|n| !freed.contains(n));
        // Documents' state (a discarded browsing context's document is freed with its tree).
        self.html
            .borrow_mut()
            .docs
            .retain(|n, _| !freed.contains(n));
        {
            let mut f = self.frames.borrow_mut();
            f.by_document.retain(|n, _| !freed.contains(n));
            f.inherited_origins.retain(|n, _| !freed.contains(n));
            f.ancestor_policies.retain(|n, _| !freed.contains(n));
        }
        {
            let mut c = self.cssom.borrow_mut();
            c.sheet_lists.retain(|n, _| !freed.contains(n));
            c.font_sets.retain(|n, _| !freed.contains(n));
            c.fonts_ready.retain(|n, _| !freed.contains(n));
            c.adopted.retain(|n, _| !freed.contains(n));
        }
        {
            let mut a = self.animations.borrow_mut();
            a.timelines.retain(|n, _| !freed.contains(n));
            a.doc_times.retain(|n, _| !freed.contains(n));
            a.doc_time_tasks.retain(|n, _| !freed.contains(n));
        }
        self.timing
            .borrow_mut()
            .navigations
            .retain(|n, _| !freed.contains(n));
        self.view_transitions
            .borrow_mut()
            .retain(|n, _| !freed.contains(n));
        self.list_anchor
            .borrow_mut()
            .retain(|n, _| !freed.contains(n));
    }

    /// A collection at a safe point (between tasks, no script on the stack): a pass, then freeing of
    /// what died. Returns false when it could not run.
    pub fn collect_garbage(&self, cx: &mut Cx) -> bool {
        if self.js_depth.get() > 0 {
            return false;
        }
        self.gc.borrow_mut().last_full = Some(std::time::Instant::now());
        self.gc_contexts_discarded(cx);
        let unloaded = std::mem::take(&mut self.gc.borrow_mut().unloaded_pending);
        if !unloaded.is_empty() {
            self.abort_document_fetches(cx, &unloaded, &[], &[], &[]);
        }
        let Some(passed) = self.gc_pass_snapshot(cx.scope()) else {
            return false;
        };
        // Weak wrappers die only in V8's collections: the weak list is scanned after one (always
        // when the isolate does not count them).
        let gcs = snowlark_js::hooks::gc_count(cx.scope());
        let gc_ran = gcs.is_none_or(|n| n != self.gc.borrow().gcs_seen);
        let idle = {
            let gc = self.gc.borrow();
            gc.detached.is_empty() && gc.fresh.is_empty()
        };
        if idle && !gc_ran && !rt::has_pending(cx.scope()).1 {
            self.gc.borrow_mut().stats.collections += 1;
            return true;
        }
        let Some(snap) = passed.or_else(|| self.snapshot()) else {
            return false;
        };
        let dead = rt::take_dead(cx.scope(), gc_ran);
        let (mut candidates, fresh) = {
            let mut gc = self.gc.borrow_mut();
            gc.gcs_seen = gcs.unwrap_or(0);
            gc.stats.collections += 1;
            (
                std::mem::take(&mut gc.detached),
                std::mem::take(&mut gc.fresh),
            )
        };
        // Objects: dead wrappers, and allocations that never got one.
        let mut keep_fresh = Vec::new();
        for h in dead.iter().copied().chain(fresh) {
            if let Some(n) = handle_node(h) {
                candidates.push(n);
                continue;
            }
            // A discarded realm's object: its document's tree may go now.
            if let Some(w) = snap.discarded.owned.get(&h) {
                candidates.extend(snap.discarded.windows.get(w));
                // (an event that never got a wrapper is out of script's reach, and the values it
                // holds strongly, having no wrapper to trace them, would keep the realm for good)
                let event = matches!(
                    self.objects.borrow().get(h.get() as usize - 1),
                    Some(Object::Event(_))
                );
                if !event {
                    continue;
                }
            }
            let collectable = matches!(
                self.objects.borrow().get(h.get() as usize - 1),
                Some(o) if !matches!(o, Object::Free) && class(o) != Class::Strong
            );
            if !collectable {
                continue;
            }
            if rt::wrapper_state(cx.scope(), h) != rt::WrapperState::None {
                continue;
            }
            if snap.pinned_objects.contains(&h.get()) {
                keep_fresh.push(h);
                continue;
            }
            self.free_object(h);
        }
        self.gc.borrow_mut().fresh.extend(keep_fresh);
        if gc_ran {
            self.gc_report(cx.scope(), &snap);
        }
        // The inactive documents are asked again now and then (every 2 s, after a V8 collection):
        // a document whose wrappers die comes back as a candidate by itself, this is for one whose
        // last pin went without a wrapper dying. (Asking all of them after every collection walked
        // every inactive document's tree, many times a second under load.)
        if gc_ran {
            let mut gc = self.gc.borrow_mut();
            let due = gc
                .inactive_asked
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2));
            if due {
                gc.inactive_asked = Some(std::time::Instant::now());
                candidates.extend(snap.discarded.docs.keys());
                candidates.extend(&snap.discarded.unloaded);
            }
        }
        // Detached subtrees with no live wrapper and nothing outside JS referring to them; and
        // documents that are no browsing context's (any more, or ever: one `createHTMLDocument` or
        // `DOMParser` made), which go with their trees once nothing reaches them.
        let roots: rustc_hash::FxHashSet<NodeId> = {
            let dom = self.dom();
            let html = self.html.borrow();
            let never_active =
                |d: &NodeId| !html.docs.get(d).is_some_and(|s| s.has_browsing_context);
            candidates
                .into_iter()
                .filter(|&n| dom.is_alive(n))
                .map(|n| dom.opaque_root(n))
                .filter(|&r| {
                    (!dom.is_document(r) || snap.discarded.inactive(&r) || never_active(&r))
                        && !snap.pinned_roots.contains(&r.to_bits())
                })
                .collect()
        };
        let mut freed = rustc_hash::FxHashSet::default();
        let mut free_roots = Vec::new();
        let mut free_realms = Vec::new();
        let mut asked = roots.clone();
        let mut roots: Vec<NodeId> = roots.into_iter().collect();
        while let Some(root) = roots.pop() {
            // A discarded browsing context's document goes with its Window's group, once nothing
            // of it has a wrapper or a pin and no node of the document is outside its tree.
            let realm = match snap.discarded.docs.get(&root) {
                Some(&window) => {
                    let group = snap.discarded.of_window(window);
                    if rt::any_wrapped(cx.scope(), group.iter().copied())
                        || group.iter().any(|h| snap.pinned_objects.contains(&h.get()))
                    {
                        continue;
                    }
                    Some((window, group))
                }
                None => None,
            };
            let nodes = self.dom().owned_nodes(root);
            if rt::any_wrapped(cx.scope(), nodes.iter().map(|&n| node_handle(n))) {
                continue;
            }
            if self.dom().is_document(root) && !self.dom().documents_owned_within(&nodes) {
                // An inactive document waits on its nodes outside its tree (subtrees its script
                // removed): they are asked now too, rather than only when a wrapper of theirs dies,
                // which happened, if ever, while the document's realm still referred to them. The
                // document goes at the next collection once they went.
                if snap.discarded.inactive(&root) {
                    let dom = self.dom();
                    for r in dom.detached_roots_of(root) {
                        if !snap.pinned_roots.contains(&r.to_bits()) && asked.insert(r) {
                            roots.push(r);
                        }
                    }
                }
                continue;
            }
            free_realms.extend(realm);
            freed.extend(nodes.iter().copied());
            free_roots.push((root, nodes));
        }
        if free_roots.is_empty() {
            return true;
        }
        // A discarded browsing context's document that a freed subtree's nodes belonged to may go
        // at the next collection (they kept it: [`snowlark_dom::Dom::documents_owned_within`]).
        let retry: Vec<NodeId> = {
            let dom = self.dom();
            free_roots
                .iter()
                .map(|(root, _)| dom.node_document(*root))
                .filter(|d| snap.discarded.inactive(d) && !freed.contains(d))
                .collect()
        };
        self.purge_nodes(&freed);
        {
            let mut dom = self.dom_mut();
            for (root, nodes) in free_roots {
                dom.free_owned(root, &nodes);
            }
        }
        self.gc.borrow_mut().detached.extend(retry);
        for (window, group) in free_realms {
            self.free_discarded_window(cx, window, &group, &snap.discarded.made);
        }
        self.gc.borrow_mut().stats.nodes_freed += freed.len() as u64;
        true
    }

    /// The browsing contexts discarded since the last collection: their Windows' timers, animation
    /// frame and idle callbacks go (they never run again), and the wrappers of their Windows,
    /// singletons, documents' objects and documents' nodes, strong while the documents were active,
    /// are asked again ([`Engine::keep`] groups them at the Window). Their documents become
    /// candidates to free.
    fn gc_contexts_discarded(&self, cx: &mut Cx) {
        let (ids, retired) = {
            let mut gc = self.gc.borrow_mut();
            let retired = std::mem::take(&mut gc.retired_pending);
            gc.retired.extend(retired.iter().copied());
            (std::mem::take(&mut gc.discarded), retired)
        };
        if ids.is_empty() && retired.is_empty() {
            return;
        }
        let (windows, docs): (Vec<Handle>, Vec<NodeId>) = {
            let f = self.frames.borrow();
            ids.iter()
                .map(|&id| (f.list[id].window, f.list[id].document))
                .chain(retired.iter().copied())
                .unzip()
        };
        {
            use crate::event_loop::TimerHandler;
            let mut el = self.event_loop.borrow_mut();
            el.timers.retain(|_, t| match &t.handler {
                TimerHandler::Function(_, _, w) | TimerHandler::Code(_, w, _) => {
                    !windows.contains(w)
                }
                TimerHandler::Internal(_) => true,
            });
            el.raf.retain(|(_, _, w)| !windows.contains(w));
            el.idle.retain(|r| !windows.contains(&r.window));
            el.idle_runnable.retain(|r| !windows.contains(&r.window));
        }
        self.forget_module_maps(&docs);
        {
            let mut e = self.embedder.borrow_mut();
            e.device_enumerations.retain(|(d, _)| !docs.contains(d));
            e.activations.retain(|d, _| !docs.contains(d));
        }
        self.fullscreen_forget(&docs);
        self.close_watchers_forget(&docs);
        self.policy_forget(&docs);
        // (JS values kept per realm or per document outside any object: the realms' queuing
        // strategy size functions, the documents' `fonts.ready` promises)
        self.streams
            .borrow_mut()
            .size_functions
            .retain(|&(realm, _), _| !windows.iter().any(|w| w.get() == realm));
        self.cssom
            .borrow_mut()
            .fonts_ready
            .retain(|d, _| !docs.contains(d));
        // The objects whose wrappers the contexts' realms made join their Window's group, and the JS
        // values the engine holds for them (and for the Windows' registries and documents) are kept
        // by the Window's wrapper from now on (engine/held.rs).
        let made = {
            let scope = cx.scope();
            let mut made = Vec::new();
            // (weak wrappers too: an event or a range the realm made refers to its nodes)
            rt::for_each_live(scope, |h, o| {
                if handle_node(h).is_none() {
                    if let Some(w) = rt::creation_realm(scope, o).filter(|w| windows.contains(w)) {
                        made.push((h, w));
                    }
                }
            });
            {
                let mut gc = self.gc.borrow_mut();
                gc.realm_objects.extend(made.iter().copied());
                gc.grouping = None;
            }
            self.gc_trace_realm_values(scope, &made, &windows, &docs);
            made
        };
        self.abort_document_fetches(cx, &docs, &ids, &windows, &made);
        let owned: rustc_hash::FxHashSet<Handle> = {
            let dom = self.dom();
            match self.discarded_realms(&dom) {
                Some(d) => d.owned.keys().copied().collect(),
                None => windows.iter().copied().collect(),
            }
        };
        {
            let dom = self.dom();
            rt::repend_where(cx.scope(), |h| match handle_node(h) {
                Some(n) => dom.is_alive(n) && docs.contains(&dom.node_document(n)),
                None => owned.contains(&h),
            });
        }
        self.gc.borrow_mut().detached.extend(docs);
    }

    /// The JS values the engine holds for a discarded context's objects (`made`: object, Window),
    /// its Window's custom element registry and its document move to the Window's wrapper.
    fn gc_trace_realm_values(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        made: &[(Handle, Handle)],
        windows: &[Handle],
        docs: &[NodeId],
    ) {
        let window_of: FxHashMap<Handle, Handle> = made.iter().copied().collect();
        {
            let mut objects = self.objects.borrow_mut();
            for (&h, &w) in &window_of {
                let Some(o) = objects.get_mut(h.get() as usize - 1) else {
                    continue;
                };
                match o {
                    Object::MutationObserver(m) => m.callback.trace(scope, w),
                    Object::ResizeObserver(r) => r.callback.trace(scope, w),
                    Object::IntersectionObserver(i) => i.callback.trace(scope, w),
                    Object::PerformanceObserver(p) => p.callback.trace(scope, w),
                    Object::TrustedTypePolicy(p) => p.trace(scope, w),
                    // (an event keeps its values on its own wrapper)
                    Object::Event(e) => e.ext.trace(scope, h),
                    Object::AbortSignal(a) => {
                        if let Some(r) = a.reason.as_mut() {
                            r.trace(scope, h);
                        }
                    }
                    Object::Xhr(x) => {
                        if let Some(v) = x.response_value.as_mut() {
                            v.trace(scope, w);
                        }
                    }
                    _ => {}
                }
            }
        }
        let registries: Vec<(Handle, Handle)> = {
            let f = self.frames.borrow();
            f.singletons
                .iter()
                .filter(|((w, _), _)| windows.contains(w))
                .map(|(&(w, _), &h)| (h, w))
                .collect()
        };
        {
            let mut c = self.custom.borrow_mut();
            let c = &mut *c;
            for d in &c.definitions {
                if let Some(&(_, w)) = registries.iter().find(|(r, _)| *r == d.registry) {
                    d.trace(scope, w);
                }
            }
            for ((registry, _), p) in c.when_defined.iter_mut() {
                if let Some(&(_, w)) = registries.iter().find(|(r, _)| r == registry) {
                    p.trace(scope, w);
                }
            }
        }
        {
            let mut history = self.history.borrow_mut();
            for (doc, w) in docs.iter().zip(windows) {
                if let Some(s) = history.states.get_mut(doc) {
                    s.trace(scope, *w);
                }
            }
        }
    }

    /// Frees the objects of a discarded browsing context's Window's group (`group`, the Window
    /// included) once its document was freed: nothing reaches them any more. The Window's own slot
    /// stays, never reused: tasks queued for it carry its handle, and find it discarded (it no
    /// longer names a browsing context's Window) rather than another object. So do the members of
    /// kinds that are never freed which are in the group only because its realm made them (other
    /// engine state may name them: an observer in its registry), with what they refer to: none of
    /// them is left naming a freed slot that another object may take.
    fn free_discarded_window(
        &self,
        cx: &mut Cx,
        window: Handle,
        group: &[Handle],
        made: &rustc_hash::FxHashSet<Handle>,
    ) {
        {
            let mut f = self.frames.borrow_mut();
            f.by_window.remove(&window);
            let location = f.singletons.get(&(window, "location")).copied();
            f.singletons.retain(|&(w, _), _| w != window);
            f.origins.remove(&window);
            f.replaced.remove(&window);
            f.tokens.forget(window);
            if let Some(l) = location {
                f.tokens.forget(l);
            }
        }
        {
            let mut gc = self.gc.borrow_mut();
            gc.realm_objects.retain(|_, w| *w != window);
            gc.grouping = None;
            gc.retired.retain(|&(w, _)| w != window);
        }
        // What stays: the Window, the made members of kinds never freed, and what they refer to.
        let mut stays: rustc_hash::FxHashSet<Handle> = rustc_hash::FxHashSet::default();
        stays.insert(window);
        {
            let objects = self.objects.borrow();
            let mut stack: Vec<Handle> = group
                .iter()
                .copied()
                .filter(|h| {
                    made.contains(h)
                        && matches!(
                            objects.get(h.get() as usize - 1),
                            Some(o) if class(o) == Class::Strong
                        )
                })
                .collect();
            while let Some(h) = stack.pop() {
                if !stays.insert(h) {
                    continue;
                }
                if let Some(o) = objects.get(h.get() as usize - 1) {
                    let mut r = Refs::default();
                    refs(o, &mut r);
                    stack.extend(r.objects);
                }
            }
        }
        let mut freed = Vec::new();
        for &h in group {
            if stays.contains(&h) {
                continue;
            }
            self.listeners.borrow_mut().remove(&h);
            self.handlers.borrow_mut().retain(|(t, _), _| *t != h);
            self.free_object(h);
            freed.push(h);
        }
        // (no handle of a freed slot is left to be taken for another object's)
        self.gc.borrow_mut().fresh.retain(|h| !freed.contains(h));
        self.listeners.borrow_mut().remove(&window);
        self.handlers.borrow_mut().retain(|(t, _), _| *t != window);
        rt::forget_realm(cx.scope(), window);
    }

    /// The collection the event loop runs after a task: [`Engine::collect_garbage`] when it has
    /// work that cannot wait for the loop's idle time ([`Engine::collect_garbage_when_idle`]), else
    /// nothing. A pass and a collection take a snapshot of everything outside JS that refers to
    /// nodes and objects, which costs as much as the page has of them; between the tasks of a busy
    /// page most turns would take it to find nothing new. So a busy page collects when wrappers
    /// wait for a pass (as many as make V8 ask for one), when [`BUSY_BACKLOG_FACTOR`] times the
    /// idle backlog of removed subtrees and new objects waits, when a browsing context was
    /// discarded or a document replaced, or when V8 collected or wrappers died and the last full
    /// collection is [`BUSY_INTERVAL`] old. Nothing is freed earlier for it, only some things later.
    /// A due collection also waits while a task is ready to run, for at most [`READY_WAIT`].
    pub fn collect_garbage_after_task(&self, cx: &mut Cx) -> bool {
        if self.js_depth.get() > 0 {
            return false;
        }
        if !self.collection_due(cx, false) {
            self.gc.borrow_mut().due_since = None;
            return true;
        }
        // A collection is the engine's own work, not a task: it does not go ahead of a task that
        // can run now (Chromium runs its collectors' work that no allocation forces in idle tasks,
        // which wait for every other task). It runs at the first turn after which no task is
        // ready, or once it has waited [`READY_WAIT`]. One that a discarded browsing context or a
        // replaced document asks for runs at once: it ends their timers and callbacks and aborts
        // their fetches, which must not wait for their next task.
        let teardown = {
            let gc = self.gc.borrow();
            !gc.discarded.is_empty()
                || !gc.retired_pending.is_empty()
                || !gc.unloaded_pending.is_empty()
        };
        if !teardown && self.has_ready_task() {
            let since = *self
                .gc
                .borrow_mut()
                .due_since
                .get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < READY_WAIT {
                self.gc.borrow_mut().stats.deferred += 1;
                return true;
            }
        }
        self.gc.borrow_mut().due_since = None;
        self.collect_garbage_timed(cx)
    }

    /// The collection the event loop runs when it is about to wait with nothing to do and `free`
    /// until its next timer or frame: Chromium's collectors do their work in such idle time (V8's
    /// and Oilpan's idle tasks, between frames), out of the way of the page's tasks, and
    /// [`Engine::collect_garbage_after_task`] leaves for it what can wait. It collects when any of
    /// that is due, or when a backlog of removed subtrees and new objects in proportion to the last
    /// snapshot ([`WALKED_PER_CANDIDATE`]) waits, when wrappers died, or when V8 collected and the
    /// last full collection is [`IDLE_INTERVAL`] old (as the loop did after every task before);
    /// and only when `free` holds as long as the last
    /// collection took (and [`IDLE_MIN`]), as a collection cannot stop at a deadline. Returns
    /// whether it collected.
    pub fn collect_garbage_when_idle(&self, cx: &mut Cx, free: std::time::Duration) -> bool {
        if self.js_depth.get() > 0 || free < IDLE_MIN.max(self.gc.borrow().last_cost) {
            return false;
        }
        if !self.collection_due(cx, true) {
            return false;
        }
        self.collect_garbage_timed(cx)
    }

    /// Whether a due collection waits for the ready tasks ([`Engine::collect_garbage_after_task`]).
    pub fn collection_deferred(&self) -> bool {
        self.gc.borrow().due_since.is_some()
    }

    /// Whether the loop's next idle time has a collection to run
    /// ([`Engine::collect_garbage_when_idle`]).
    pub fn idle_collection_due(&self, cx: &mut Cx) -> bool {
        self.js_depth.get() == 0 && self.collection_due(cx, true)
    }

    /// Whether a collection is due now, the loop being `idle` (about to wait) or busy.
    fn collection_due(&self, cx: &mut Cx, idle: bool) -> bool {
        let scope = cx.scope();
        let gc_ran =
            snowlark_js::hooks::gc_count(scope).is_none_or(|n| n != self.gc.borrow().gcs_seen);
        if rt::pass_due(scope, idle) {
            return true;
        }
        let dead = rt::has_pending(scope).1;
        let gc = self.gc.borrow();
        let backlog = gc.detached.len() + gc.fresh.len();
        let at = IDLE_BACKLOG.max(gc.last_walked / WALKED_PER_CANDIDATE);
        let old = |interval| gc.last_full.is_none_or(|t| t.elapsed() >= interval);
        let rest = if idle {
            backlog >= at || dead || (gc_ran && old(IDLE_INTERVAL))
        } else {
            backlog >= at * BUSY_BACKLOG_FACTOR || ((gc_ran || dead) && old(BUSY_INTERVAL))
        };
        rest || !gc.discarded.is_empty()
            || !gc.retired_pending.is_empty()
            || !gc.unloaded_pending.is_empty()
    }

    /// [`Engine::collect_garbage`], keeping how long it took for the idle time the next one needs.
    fn collect_garbage_timed(&self, cx: &mut Cx) -> bool {
        let start = std::time::Instant::now();
        let collected = self.collect_garbage(cx);
        self.gc.borrow_mut().last_cost = start.elapsed();
        collected
    }

    /// Collection totals.
    pub fn gc_stats(&self) -> GcStats {
        self.gc.borrow().stats
    }

    /// Live nodes and live platform objects that are not nodes.
    /// What is alive, for a harness's report: platform objects by interface, and each document's
    /// nodes (in its tree, the rest), the largest first. Walks the arena.
    pub fn live_census(&self) -> Vec<String> {
        use std::collections::BTreeMap;
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        {
            let objects = self.objects.borrow();
            for (i, o) in objects.iter().enumerate() {
                if matches!(o, Object::Free) {
                    continue;
                }
                let h = Handle::new(i as u64 + 1);
                let name = format!("{:?}", <Engine as rt::Host>::interface_of(self, h));
                *kinds.entry(name).or_default() += 1;
            }
        }
        let mut kinds: Vec<(String, usize)> = kinds.into_iter().collect();
        kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
        let mut out = vec!["live objects by interface:".to_owned()];
        for (k, n) in kinds.iter().take(25) {
            out.push(format!("  {n:7} {k}"));
        }
        // What the animation effects still alive point at (an effect outlives its target and its
        // animation only by a leak).
        let mut effects: BTreeMap<String, usize> = BTreeMap::new();
        {
            let objects = self.objects.borrow();
            let dom = self.dom();
            for o in objects.iter() {
                let Object::KeyframeEffect(e) = o else {
                    continue;
                };
                let target = match e.target {
                    None => "no target",
                    Some(n) if !dom.is_alive(n) => "target freed",
                    Some(n) if dom.is_connected(n) => "target connected",
                    Some(_) => "target detached",
                };
                let animation = match e.animation {
                    None => "no animation",
                    Some(a) => match objects.get(a.get() as usize - 1) {
                        Some(Object::Animation(_)) => "animation alive",
                        Some(Object::Free) | None => "animation freed",
                        Some(_) => "animation slot reused",
                    },
                };
                *effects.entry(format!("{target}, {animation}")).or_default() += 1;
            }
        }
        for (k, n) in &effects {
            out.push(format!("  {n:7} KeyframeEffect: {k}"));
        }
        let dom = self.dom();
        let mut docs = dom.documents_with_node_counts();
        docs.sort_by_key(|d| std::cmp::Reverse(d.1));
        out.push("live documents (nodes: all, in its tree):".to_owned());
        for (d, n) in docs.iter().take(15) {
            let in_tree = dom.owned_nodes(*d).len();
            out.push(format!("  {n:7} {in_tree:7}  {}", dom.document_url(*d)));
        }
        out.push(format!(
            "  nodes without a document: {}",
            dom.nodes_without_document()
        ));
        out
    }

    pub fn live_counts(&self) -> (usize, usize) {
        let objects = self.objects.borrow().len() - self.gc.borrow().free_slots.len();
        (self.dom().node_count(), objects)
    }
}

/// [`rt::GcHooks::on_wrap`] for the engine.
fn on_wrap_thunk(engine: &dyn std::any::Any, h: Handle) -> Option<Handle> {
    engine.downcast_ref::<Engine>()?.gc_on_wrap(h)
}

/// [`rt::GcHooks::request_pass`]: a V8 interrupt runs the pass.
fn request_pass(isolate: &v8::Isolate) {
    snowlark_js::hooks::request_interrupt(isolate);
}

/// The interrupt: a pass in the top-level realm's context.
fn interrupt_pass(scope: &mut v8::PinScope<'_, '_, ()>) {
    let Some(st) = rt::state_of(scope) else {
        return;
    };
    let engine = st.engine::<Engine>();
    let Some(realm) = engine.realm_of_context(super::frames::TOP) else {
        return;
    };
    let context = v8::Local::new(scope, &realm);
    let scope = &mut v8::ContextScope::new(scope, context);
    engine.gc_pass(scope);
}

/// Installs the wrapper lifetime hooks in an isolate whose bindings are installed.
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>) {
    rt::set_gc_hooks(
        scope,
        rt::GcHooks {
            on_wrap: on_wrap_thunk,
            request_pass,
        },
    );
    snowlark_js::hooks::set_interrupt_hook(scope, interrupt_pass);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referrers_join_through_a_chain_of_objects() {
        let h = Handle::new;
        let window = h(1);
        let mut owned: FxHashMap<Handle, Handle> = FxHashMap::default();
        owned.insert(window, window);
        // 4 refers to 3, which refers to 2, which joined by a node; 5 refers to nothing in the
        // group. The chain is listed against the order it joins in.
        let targets = [h(3), h(2), h(9)];
        let mut waiting = vec![(h(4), 0..1), (h(3), 1..2), (h(5), 2..3)];
        join_referrers(&mut owned, vec![(h(2), window)], &mut waiting, &targets);
        assert_eq!(owned.get(&h(4)), Some(&window));
        assert_eq!(owned.get(&h(3)), Some(&window));
        assert_eq!(waiting, vec![(h(5), 2..3)]);
    }

    #[test]
    fn node_groups_answer_as_the_document_and_the_opaque_root_do() {
        let mut dom = snowlark_dom::Dom::new();
        let a = dom.create_document(snowlark_dom::DocumentKind::Html);
        let other = dom.create_document(snowlark_dom::DocumentKind::Html);
        let (wa, wb) = (Handle::new(1), Handle::new(2));
        let root = dom.create_element(a, "div", None).unwrap();
        dom.append_child(a, root).unwrap();
        let detached = dom.create_element(a, "div", None).unwrap();
        let outside = dom.create_element(other, "div", None).unwrap();
        // A connected template's contents: their document is not their opaque root.
        let template = dom.create_element(a, "template", None).unwrap();
        dom.append_child(root, template).unwrap();
        let contents = dom.template_contents(template).unwrap();
        let inert = dom.node_document(contents);
        let mixed = dom.create_element(inert, "p", None).unwrap();
        dom.append_child(contents, mixed).unwrap();
        assert_ne!(inert, a);

        let mut docs = FxHashMap::default();
        docs.insert(a, wa);
        docs.insert(inert, wb);
        let mut groups = NodeGroups::new(&dom, &docs);
        // (each asked twice: the second answer comes from the memo)
        for _ in 0..2 {
            assert_eq!(groups.window(&dom, root), Some(wa));
            assert_eq!(groups.window(&dom, detached), Some(wa));
            assert_eq!(groups.window(&dom, outside), None);
            assert_eq!(groups.window(&dom, mixed), Some(wb));
            assert!(groups.is_in(&dom, mixed, wa) && groups.is_in(&dom, mixed, wb));
            assert!(groups.is_in(&dom, root, wa) && !groups.is_in(&dom, root, wb));
            assert!(!groups.is_in(&dom, outside, wa));
        }

        // A slot reused for another node does not answer for the old id, or with the old answer.
        dom.free_subtree(detached);
        let reused = dom.create_element(other, "div", None).unwrap();
        assert_eq!(reused.index(), detached.index());
        assert_eq!(groups.window(&dom, reused), None);
        assert_eq!(groups.window(&dom, detached), None);
    }
}
