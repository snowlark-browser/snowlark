// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wrapper lifetime (ARCHITECTURE.md "Wrappers and GC"): which wrappers V8 may collect.
//!
//! Every wrapper is made strong and *pending*. A pass (run by the engine at a safe point, or from a V8
//! interrupt once enough wrappers are pending) asks the engine about each pending wrapper:
//!
//! - [`Keep::Strong`]: it stays strong and leaves the pending list (a node of a document that is alive,
//!   a platform object the engine never releases);
//! - [`Keep::Pinned`]: it stays strong and pending (something outside JS refers to it for now);
//! - [`Keep::Weak`]: it becomes weak;
//! - [`Keep::Anchored`]: it becomes weak and joins the group of an *anchor* (a detached node's opaque
//!   root, an owned object's owner): the member points at the anchor's wrapper and the anchor's wrapper
//!   heads a list of its members, so the group stays reachable as long as any of its wrappers is, and
//!   expando properties on a detached node survive while anything in its subtree is referenced.
//!
//! A weak wrapper becomes strong again ([`touch`]) when its node moves; dead weak wrappers are handed to
//! the engine ([`take_dead`]), which frees what nothing else refers to.

use super::*;

/// A wrapper as the cache holds it: one global handle, strong or weak in place (it must not move while
/// weak, which the wrapper table sees to).
pub(crate) type Wrapper = snowlark_js::wrapper_ref::WrapperRef;

/// The engine's answer for a pending wrapper in a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Strong for good (until the engine touches it again).
    Strong,
    /// Strong for now; asked again at the next pass.
    Pinned,
    /// Weak, alone.
    Weak,
    /// Weak, in the group of the anchor (the handle itself for a group's root).
    Anchored(Handle),
}

/// What the engine provides for wrapper lifetime.
#[derive(Clone, Copy)]
pub struct GcHooks {
    /// Called when a wrapper is made: a handle whose weak wrapper must turn strong until the next
    /// pass, because the new wrapper is not in its group yet (a detached node's opaque root).
    pub on_wrap: fn(&dyn Any, Handle) -> Option<Handle>,
    /// Asks for a pass soon: the engine requests a V8 interrupt and runs [`pass`] from it.
    pub request_pass: fn(&v8::Isolate),
}

/// The state of a handle's wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapperState {
    None,
    Strong,
    Weak,
}

/// Counts for tests and statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WrapperCounts {
    pub strong: usize,
    pub weak: usize,
    pub pending: usize,
}

/// Wrapper bookkeeping in [`State`].
pub(crate) struct Gc {
    pub hooks: Cell<Option<GcHooks>>,
    /// Handles whose wrapper is strong and waits for a pass.
    pub pending: RefCell<Vec<u64>>,
    /// Handles made weak (some since strong again or dead: checked when pruned).
    pub weak: RefCell<Vec<u64>>,
    /// Handles whose weak wrapper died, for [`take_dead`].
    pub dead: RefCell<Vec<u64>>,
    /// Pending length that asks for a pass.
    pub pass_at: Cell<usize>,
    /// New wrappers the next pass waits for at least ([`pace_passes`]).
    pub pass_floor: Cell<usize>,
    /// Weak list length that prunes it.
    pub prune_at: Cell<usize>,
    pub requested: Cell<bool>,
    pub in_pass: Cell<bool>,
    /// A bit per handle's low 32 bits that ever got a wrapper (set in [`wrapper_made`], never
    /// cleared): a clear bit means no wrapper, so [`touch`] skips the map for the many nodes script
    /// never saw. Handles sharing low bits only cost a lookup.
    pub ever_wrapped: RefCell<Vec<u64>>,
}

impl Gc {
    fn mark_wrapped(&self, handle: u64) {
        let i = handle as u32 as usize;
        let mut bits = self.ever_wrapped.borrow_mut();
        if bits.len() <= i / 64 {
            bits.resize(i / 64 + 1, 0);
        }
        bits[i / 64] |= 1 << (i % 64);
    }

    fn maybe_wrapped(&self, handle: u64) -> bool {
        let i = handle as u32 as usize;
        self.ever_wrapped
            .borrow()
            .get(i / 64)
            .is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

const MIN_PASS: usize = 4096;

/// How many times the pending wrappers a pass waits for ([`Gc::pass_at`]) make script stop for one
/// (V8's interrupt): until then they wait for the event loop's idle time, where the engine runs its
/// passes ([`pass_due`]), as Chromium's collectors work in idle tasks between frames.
const BUSY_PASS_FACTOR: usize = 4;
const MIN_PRUNE: usize = 8192;

impl Default for Gc {
    fn default() -> Self {
        Gc {
            hooks: Cell::new(None),
            pending: RefCell::new(Vec::new()),
            weak: RefCell::new(Vec::new()),
            dead: RefCell::new(Vec::new()),
            pass_at: Cell::new(MIN_PASS),
            pass_floor: Cell::new(0),
            prune_at: Cell::new(MIN_PRUNE),
            requested: Cell::new(false),
            in_pass: Cell::new(false),
            ever_wrapped: RefCell::new(Vec::new()),
        }
    }
}

/// Installs the engine's hooks.
pub fn set_gc_hooks(scope: &v8::PinScope<'_, '_>, hooks: GcHooks) {
    state(scope).gc.hooks.set(Some(hooks));
}

/// A new wrapper was cached for `handle`: it is pending, and the anchor the engine names turns strong.
pub(crate) fn wrapper_made(scope: &v8::PinScope<'_, '_>, st: &State, handle: Handle) {
    st.gc.mark_wrapped(handle.get());
    st.gc.pending.borrow_mut().push(handle.get());
    let Some(hooks) = st.gc.hooks.get() else {
        return;
    };
    if let Some(anchor) = (hooks.on_wrap)(&*st.engine, handle) {
        strengthen(st, anchor);
    }
    if !st.gc.requested.get()
        && !st.gc.in_pass.get()
        && st.gc.pending.borrow().len() >= st.gc.pass_at.get().saturating_mul(BUSY_PASS_FACTOR)
    {
        st.gc.requested.set(true);
        (hooks.request_pass)(scope);
    }
}

/// Makes a live weak wrapper strong and pending (returning [`WrapperState::Weak`], what it was); a
/// dead one is dropped (and reported dead).
fn strengthen(st: &State, handle: Handle) -> WrapperState {
    let mut map = st.wrappers.borrow_mut();
    let Some(w) = map.get(&handle.get()) else {
        return WrapperState::None;
    };
    if !w.is_weak() {
        return WrapperState::Strong;
    }
    if w.make_strong() {
        st.gc.pending.borrow_mut().push(handle.get());
        return WrapperState::Weak;
    }
    map.remove(&handle.get());
    st.gc.dead.borrow_mut().push(handle.get());
    WrapperState::None
}

/// A handle's wrapper moved (its node was inserted or removed): a weak one turns strong, and either
/// waits for the next pass to be grouped again. Returns whether the handle has a wrapper.
pub fn touch(scope: &v8::PinScope<'_, '_>, handle: Handle) -> bool {
    touch_in(&state(scope), handle)
}

/// [`touch`] for each of `handles` (a moved subtree's nodes).
pub fn touch_all(scope: &v8::PinScope<'_, '_>, handles: impl IntoIterator<Item = Handle>) {
    let st = state(scope);
    for h in handles {
        touch_in(&st, h);
    }
}

fn touch_in(st: &State, handle: Handle) -> bool {
    if !st.gc.maybe_wrapped(handle.get()) {
        return false;
    }
    match strengthen(st, handle) {
        WrapperState::Strong => {
            let mut p = st.gc.pending.borrow_mut();
            if p.last() != Some(&handle.get()) {
                p.push(handle.get());
            }
            true
        }
        WrapperState::Weak => true,
        WrapperState::None => false,
    }
}

/// A weak wrapper turns strong until the next pass (a group's anchor that gained a member not
/// linked yet); a strong one stays as it is.
pub fn hold(scope: &v8::PinScope<'_, '_>, handle: Handle) {
    let st = state(scope);
    strengthen(&st, handle);
}

/// Keeps `value` alive as long as `handle`'s wrapper, in a private property of the wrapper: for
/// values the engine reaches through the handle (an event listener's callback) that must not keep
/// the wrapper alive by themselves, as a strong handle would (the callback's closure often refers to
/// the wrapper). Returns false when the handle has no live wrapper.
pub fn keep_with_wrapper(
    scope: &v8::PinScope<'_, '_>,
    handle: Handle,
    value: v8::Local<v8::Value>,
) -> bool {
    let st = state(scope);
    let Some(obj) = live(scope, &st, handle.get()) else {
        return false;
    };
    let key = v8::Private::for_api(scope, Some(name(scope, "snowlark#kept")));
    let list = obj
        .get_private(scope, key)
        .and_then(|v| v8::Local::<v8::Array>::try_from(v).ok());
    let list = match list {
        Some(l) => l,
        None => {
            let l = v8::Array::new(scope, 0);
            if obj.set_private(scope, key, l.into()) != Some(true) {
                return false;
            }
            l
        }
    };
    list.set_index(scope, list.length(), value).is_some()
}

/// Calls `f` with every live wrapper, strong or weak.
pub fn for_each_live<'s>(
    scope: &v8::PinScope<'s, '_>,
    mut f: impl FnMut(Handle, v8::Local<'s, v8::Object>),
) {
    let st = state(scope);
    let map = st.wrappers.borrow();
    for (&h, w) in map.iter() {
        if let Some(obj) = w.get(scope) {
            f(Handle(h), obj);
        }
    }
}

/// Calls `f` with every strong wrapper (diagnostics: what keeps a realm alive).
pub fn for_each_strong<'s>(
    scope: &v8::PinScope<'s, '_>,
    mut f: impl FnMut(Handle, v8::Local<'s, v8::Object>),
) {
    let st = state(scope);
    let map = st.wrappers.borrow();
    for (&h, w) in map.iter() {
        if !w.is_weak() {
            if let Some(obj) = w.get(scope) {
                f(Handle(h), obj);
            }
        }
    }
}

/// The handle of the global object of the realm `obj` was made in (none for a realm whose global
/// is not a wrapper).
pub fn creation_realm(scope: &v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> Option<Handle> {
    realm_global(obj.get_creation_context(scope)?)
}

/// The handle of the global object of the realm `handle`'s wrapper was made in (none without a live
/// wrapper).
pub fn wrapper_realm(scope: &v8::PinScope<'_, '_>, handle: Handle) -> Option<Handle> {
    let st = state(scope);
    if !st.gc.maybe_wrapped(handle.get()) {
        return None;
    }
    let map = st.wrappers.borrow();
    let obj = map.get(&handle.get())?.get(scope)?;
    creation_realm(scope, obj)
}

/// Whether any of `handles` has a wrapper ([`wrapper_state`] is not `None`).
pub fn any_wrapped(
    scope: &v8::PinScope<'_, '_>,
    handles: impl IntoIterator<Item = Handle>,
) -> bool {
    let st = state(scope);
    let map = st.wrappers.borrow();
    handles
        .into_iter()
        .any(|h| st.gc.maybe_wrapped(h.get()) && map.get(&h.get()).is_some_and(|w| !w.is_dead()))
}

/// The state of `handle`'s wrapper.
pub fn wrapper_state(scope: &v8::PinScope<'_, '_>, handle: Handle) -> WrapperState {
    let st = state(scope);
    if !st.gc.maybe_wrapped(handle.get()) {
        return WrapperState::None;
    }
    let map = st.wrappers.borrow();
    match map.get(&handle.get()) {
        None => WrapperState::None,
        Some(w) if w.is_dead() => WrapperState::None,
        Some(w) if w.is_weak() => WrapperState::Weak,
        Some(_) => WrapperState::Strong,
    }
}

/// Whether wrappers wait for a pass, or dead ones for [`take_dead`].
pub fn has_pending(scope: &v8::PinScope<'_, '_>) -> (bool, bool) {
    let st = state(scope);
    let pending = !st.gc.pending.borrow().is_empty();
    let dead = !st.gc.dead.borrow().is_empty();
    (pending, dead)
}

/// Whether a pass was asked for, or enough wrappers are pending: the pass threshold when the event
/// loop is `idle` (about to wait), [`BUSY_PASS_FACTOR`] times it when it is busy.
pub fn pass_due(scope: &v8::PinScope<'_, '_>, idle: bool) -> bool {
    let st = state(scope);
    let at = st.gc.pass_at.get();
    let at = if idle {
        at
    } else {
        at.saturating_mul(BUSY_PASS_FACTOR)
    };
    st.gc.requested.get() || st.gc.pending.borrow().len() >= at
}

/// The engine could not run a pass now (its state is borrowed): ask again once more wrappers are
/// pending.
pub fn defer_pass(scope: &v8::PinScope<'_, '_>) {
    let st = state(scope);
    st.gc.requested.set(false);
    let n = st.gc.pending.borrow().len();
    st.gc.pass_at.set(n + MIN_PASS);
}

/// Every strong wrapper `f` selects becomes pending again (a document was unloaded: its nodes are no
/// longer kept by it).
pub fn repend_where(scope: &v8::PinScope<'_, '_>, mut f: impl FnMut(Handle) -> bool) {
    let st = state(scope);
    let map = st.wrappers.borrow();
    let mut p = st.gc.pending.borrow_mut();
    for (&h, w) in map.iter() {
        if !w.is_weak() && f(Handle(h)) {
            p.push(h);
        }
    }
}

/// Drops every cached wrapper handle while the isolate is still alive (before it is disposed: V8's
/// teardown collection would otherwise call back into weak handles being dropped with it).
pub fn release_all(isolate: &mut v8::Isolate) {
    let Some(st) = isolate.get_slot::<Rc<State>>().cloned() else {
        return;
    };
    let wrappers = std::mem::take(&mut *st.wrappers.borrow_mut());
    drop(wrappers);
    st.realm_contexts.borrow_mut().clear();
    st.gc.pending.borrow_mut().clear();
    st.gc.weak.borrow_mut().clear();
    st.gc.dead.borrow_mut().clear();
}

/// Wrapper counts.
pub fn wrapper_counts(scope: &v8::PinScope<'_, '_>) -> WrapperCounts {
    let st = state(scope);
    let map = st.wrappers.borrow();
    let mut c = WrapperCounts {
        pending: st.gc.pending.borrow().len(),
        ..Default::default()
    };
    for w in map.values() {
        if !w.is_weak() {
            c.strong += 1;
        } else if !w.is_dead() {
            c.weak += 1;
        }
    }
    c
}

/// Moves dead weak wrappers out of the cache into the dead list.
fn prune(st: &State) {
    let mut map = st.wrappers.borrow_mut();
    let mut dead = st.gc.dead.borrow_mut();
    let mut weak = st.gc.weak.borrow_mut();
    weak.retain(|h| match map.get(h) {
        Some(w) if w.is_weak() && w.is_dead() => {
            map.remove(h);
            dead.push(*h);
            false
        }
        Some(w) => w.is_weak(),
        None => false,
    });
    st.gc.prune_at.set((weak.len() * 2).max(MIN_PRUNE));
}

/// The handles whose wrappers V8 collected since the last call (each once). The engine frees what
/// nothing else refers to. `gc_ran`: whether V8 collected garbage since the last call; weak
/// wrappers only die in a collection, so without one the weak list is not scanned.
pub fn take_dead(scope: &v8::PinScope<'_, '_>, gc_ran: bool) -> Vec<Handle> {
    let st = state(scope);
    if gc_ran {
        prune(&st);
    }
    let dead = std::mem::take(&mut *st.gc.dead.borrow_mut());
    dead.into_iter().map(Handle).collect()
}

/// The live wrapper of a handle, if it has one.
fn live<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    handle: u64,
) -> Option<v8::Local<'s, v8::Object>> {
    st.wrappers.borrow().get(&handle)?.get(scope)
}

// Group links live in the wrapper's own internal fields ([`WRAPPER_FIELDS`]): a member points at its
// anchor; an anchor heads a doubly linked list of its members. Setting a field is a store with a write
// barrier: no allocation, no shape change.
const ANCHOR: usize = 3;
const HEAD: usize = 4;
const NEXT: usize = 5;
const PREV: usize = 6;

fn field<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    index: usize,
) -> Option<v8::Local<'s, v8::Object>> {
    let data = obj.get_internal_field(scope, index)?;
    let value = v8::Local::<v8::Value>::try_from(data).ok()?;
    v8::Local::<v8::Object>::try_from(value).ok()
}

fn set_field(obj: v8::Local<v8::Object>, index: usize, value: v8::Local<v8::Value>) {
    obj.set_internal_field(index, value.into());
}

/// Takes `member` out of its group.
fn unlink(scope: &v8::PinScope<'_, '_>, member: v8::Local<v8::Object>) {
    let Some(anchor) = field(scope, member, ANCHOR) else {
        return;
    };
    let undefined: v8::Local<v8::Value> = v8::undefined(scope).into();
    let prev = field(scope, member, PREV).unwrap_or(anchor);
    let next = field(scope, member, NEXT);
    let next_value = next.map_or(undefined, Into::into);
    if prev == anchor {
        set_field(anchor, HEAD, next_value);
    } else {
        set_field(prev, NEXT, next_value);
    }
    if let Some(n) = next {
        set_field(n, PREV, prev.into());
    }
    set_field(member, ANCHOR, undefined);
    set_field(member, NEXT, undefined);
    set_field(member, PREV, undefined);
}

/// Puts `member` in `anchor`'s group, out of the group it was in.
fn link(
    scope: &v8::PinScope<'_, '_>,
    member: v8::Local<v8::Object>,
    anchor: v8::Local<v8::Object>,
) {
    if field(scope, member, ANCHOR) == Some(anchor) {
        return;
    }
    unlink(scope, member);
    let head = field(scope, anchor, HEAD);
    set_field(member, ANCHOR, anchor.into());
    set_field(
        member,
        NEXT,
        head.map_or_else(|| v8::undefined(scope).into(), Into::into),
    );
    set_field(member, PREV, anchor.into());
    if let Some(h) = head {
        set_field(h, PREV, member.into());
    }
    set_field(anchor, HEAD, member.into());
}

/// Pending length that asks for the next pass, with `carried` wrappers still pending after this one.
fn next_pass_at(carried: usize, floor: usize) -> usize {
    (carried * 2).max(MIN_PASS).max(carried + floor)
}

/// The next pass waits for at least `new_wrappers` more wrappers: what a pass costs the engine goes
/// with what the page holds, so a page that holds much is asked less often.
pub fn pace_passes(scope: &v8::PinScope<'_, '_>, new_wrappers: usize) {
    state(scope).gc.pass_floor.set(new_wrappers);
}

/// A pass over the pending wrappers with the engine's `policy` (see the module documentation).
/// Returns the handles whose wrappers became weak.
pub fn pass(
    scope: &mut v8::PinScope<'_, '_>,
    policy: &mut dyn FnMut(Handle) -> Keep,
) -> Vec<Handle> {
    let st = state(scope);
    if st.gc.in_pass.replace(true) {
        return Vec::new();
    }
    st.gc.requested.set(false);
    // Duplicates are harmless: a second visit finds the wrapper weak already.
    let pending = std::mem::take(&mut *st.gc.pending.borrow_mut());
    let mut carry = Vec::new();
    let mut weakened = Vec::new();
    for h in pending {
        let Some(obj) = (match st.wrappers.borrow().get(&h) {
            Some(w) if !w.is_weak() => w.get(scope),
            _ => None,
        }) else {
            continue;
        };
        let keep = policy(Handle(h));
        let anchor = match keep {
            Keep::Strong => {
                unlink(scope, obj);
                continue;
            }
            Keep::Pinned => {
                carry.push(h);
                continue;
            }
            Keep::Weak => None,
            Keep::Anchored(a) => Some(a),
        };
        if let Some(a) = anchor.filter(|a| a.get() != h) {
            // The anchor's wrapper, made now if it has none (it is pending then, and strong until
            // the next pass).
            let anchor_obj = match live(scope, &st, a.get()) {
                Some(o) => Some(o),
                None => wrap_own(scope, &st, a),
            };
            let Some(anchor_obj) = anchor_obj else {
                carry.push(h);
                continue;
            };
            link(scope, obj, anchor_obj);
        }
        if let Some(w) = st.wrappers.borrow().get(&h) {
            // SAFETY: an entry stays where it is in the table until it is removed (dropped in place).
            unsafe { w.make_weak() };
        }
        st.gc.weak.borrow_mut().push(h);
        weakened.push(Handle(h));
    }
    carry.sort_unstable();
    carry.dedup();
    let carried = carry.len();
    st.gc.pending.borrow_mut().extend(carry);
    st.gc
        .pass_at
        .set(next_pass_at(carried, st.gc.pass_floor.get()));
    if st.gc.weak.borrow().len() >= st.gc.prune_at.get() {
        prune(&st);
    }
    st.gc.in_pass.set(false);
    weakened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_pass_waits_for_what_the_last_one_cost() {
        assert_eq!(next_pass_at(0, 0), MIN_PASS);
        assert_eq!(next_pass_at(MIN_PASS, 0), MIN_PASS * 2);
        assert_eq!(next_pass_at(0, MIN_PASS * 8), MIN_PASS * 8);
        // (the floor counts new wrappers, on top of those still pending)
        assert_eq!(next_pass_at(100, MIN_PASS * 8), MIN_PASS * 8 + 100);
    }

    #[test]
    fn handles_never_wrapped_are_known_and_others_are_kept() {
        let gc = Gc::default();
        assert!(!gc.maybe_wrapped(5));
        gc.mark_wrapped(5);
        gc.mark_wrapped((7 << 32) | 1000);
        assert!(gc.maybe_wrapped(5));
        // Same low 32 bits: a possible wrapper (only a lookup is spent).
        assert!(gc.maybe_wrapped((3 << 32) | 5));
        assert!(gc.maybe_wrapped(1000));
        assert!(!gc.maybe_wrapped(999));
        assert!(!gc.maybe_wrapped(1 << 20));
    }
}
