// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Callbacks that read their arguments in place and make a scope only when they need one, as Blink's
//! generated callbacks do (`bind_gen/interface.py`, `bind_callback_local_vars`: the receiver from
//! `info.This()`, no `HandleScope`, symbols bound only when used; `v8_set_return_value.h`: a cached
//! wrapper and the oddballs go straight into the return slot).
//!
//! The generated attribute getters whose receiver V8 checked and whose engine call takes no `Cx` are
//! [`Call`] bodies, installed through [`light_builder`], and so are the operations whose receiver V8
//! checked and that have one overload: they read their arguments in place ([`Call::arg`]) and make one
//! scope for the conversions and the engine call ([`Call::with_scope`]), as Blink's make none and an
//! `ExceptionState` only for members that throw. A result that must be allocated (a string, a
//! wrapper not made yet, a number outside a Smi) takes the full path of the other callbacks through
//! [`snowlark_js::raw_callback::Info::with_scope`].

use snowlark_js::guard::{self, TestPanic};
use snowlark_js::raw_callback::{self, Address, Info, Slot};

use super::{prof, return_handle, Handle, State, ToJs};

/// The embedder data slot of the isolate that holds the [`State`]'s address ([`super::set_state`]).
pub const STATE_DATA_SLOT: u32 = 0;

/// The addresses of the read-only oddballs results are set to.
#[derive(Clone, Copy)]
pub struct Roots {
    pub(crate) null: Address,
    pub(crate) true_: Address,
    pub(crate) false_: Address,
    /// The slot `undefined` is read from (the isolate's root table), for arguments past the last.
    pub(crate) undefined: Slot,
}

impl Default for Roots {
    fn default() -> Roots {
        Roots {
            null: 0,
            true_: 0,
            false_: 0,
            undefined: std::ptr::null(),
        }
    }
}

/// One call of a light callback: V8's arguments and the isolate's binding state.
#[derive(Clone, Copy)]
pub struct Call<'a> {
    info: Info<'a>,
    state: &'a State,
}

impl<'a> Call<'a> {
    #[inline(always)]
    fn new(info: Info<'a>) -> Call<'a> {
        let p = info.embedder_data(STATE_DATA_SLOT) as *const State;
        // SAFETY: light callbacks are installed only by the bindings, in isolates where `set_state`
        // stored the state's address in that slot; the isolate's slot map holds the `Rc` until the
        // isolate is disposed, after every call.
        let state = unsafe { &*p };
        Call { info, state }
    }

    /// The isolate's binding state.
    #[inline(always)]
    pub fn state(&self) -> &'a State {
        self.state
    }

    /// The receiver's handle.
    ///
    /// # Safety
    ///
    /// As [`super::receiver`]: only in a member whose template has a receiver signature and that is
    /// never called with a global proxy as its receiver.
    #[inline(always)]
    pub unsafe fn receiver(&self) -> Handle {
        // SAFETY: as the caller promised.
        unsafe { super::receiver(&self.info.this()) }
    }

    /// `Length()`.
    #[inline(always)]
    pub fn length(&self) -> i32 {
        self.info.length()
    }

    /// `info[i]`, read in place: the argument, or `undefined` past the last one.
    #[inline(always)]
    pub fn arg(&self, i: usize) -> v8::Local<'a, v8::Value> {
        self.info.arg(i, self.state.roots.get().undefined)
    }

    /// Runs `f` with a scope and the call's return value: for operations, whose engine call takes a
    /// `Cx` and may throw, and for results that must be allocated.
    #[inline(always)]
    pub fn with_scope<R>(
        &self,
        f: impl for<'s, 'i> FnOnce(&mut v8::PinScope<'s, 'i>, v8::ReturnValue<'s, v8::Value>) -> R,
    ) -> R {
        self.info.with_scope(|scope, _, rv| f(scope, rv))
    }

    /// Throws the TypeError of a call with fewer than `required` arguments.
    #[cold]
    #[inline(never)]
    pub fn not_enough_arguments(&self, what: &str, required: usize) {
        let given = self.length();
        self.info.with_scope(|scope, _, _| {
            super::not_enough_arguments::<()>(scope, what, required, given);
        });
    }

    /// [`prof::enter`] for this call.
    #[inline(always)]
    pub fn prof(&self, name: &'static str) -> Option<prof::Guard> {
        if !prof::enabled() {
            return None;
        }
        self.prof_slow(name)
    }

    #[cold]
    #[inline(never)]
    fn prof_slow(&self, name: &'static str) -> Option<prof::Guard> {
        self.info.with_scope(|scope, _, _| prof::enter(scope, name))
    }

    /// Sets the result to `handle`'s wrapper, or null ([`return_handle`]): a cached wrapper and null
    /// are stored in place.
    #[inline(always)]
    pub fn return_handle(&self, handle: Option<Handle>) {
        let st = self.state;
        let Some(h) = handle else {
            // SAFETY: `null` is a read-only root of this isolate.
            unsafe { self.info.set_return_address(st.roots.get().null) };
            return;
        };
        // (a replaced Window's value is its WindowProxy, not the wrapper cached for it)
        if h.get() > st.max_replaced.get() {
            if let Some(w) = st.wrappers.borrow().get(&h.get()) {
                if w.return_into(&self.info) {
                    return;
                }
            }
        }
        self.return_handle_slow(h);
    }

    #[inline(never)]
    fn return_handle_slow(&self, h: Handle) {
        self.info
            .with_scope(|scope, _, mut rv| return_handle(scope, &mut rv, Some(h)));
    }

    #[inline(always)]
    pub fn return_bool(&self, v: bool) {
        let roots = self.state.roots.get();
        // SAFETY: `true` and `false` are read-only roots of this isolate.
        unsafe {
            self.info
                .set_return_address(if v { roots.true_ } else { roots.false_ })
        };
    }

    #[inline(always)]
    pub fn return_i32(&self, v: i32) {
        self.info.set_return_i32(v);
    }

    #[inline(always)]
    pub fn return_u32(&self, v: u32) {
        self.info.set_return_u32(v);
    }

    #[inline]
    pub fn return_f64(&self, v: f64) {
        self.info.with_scope(|_, _, mut rv| rv.set_double(v));
    }

    /// Sets the result to `v` converted in a scope (strings, dictionaries, other values).
    #[inline]
    pub fn return_value(&self, v: &dyn ToJs) {
        self.info.with_scope(|scope, _, mut rv| {
            if let Some(js) = v.to_js(scope) {
                rv.set(js);
            }
        });
    }
}

/// `v8::FunctionTemplate::builder_raw` for a light callback, under the panic guard of the other
/// callbacks ([`super::guard`]): a panic is recorded and the running script terminated.
#[inline(always)]
pub fn light_builder<'s, F>(f: F) -> v8::FunctionBuilder<'s, v8::FunctionTemplate>
where
    F: for<'a> Fn(Call<'a>) + Copy,
{
    let _ = f;
    v8::FunctionTemplate::builder_raw(raw_callback::callback(light_call::<F>))
}

#[inline(always)]
fn light_call<F>(info: Info<'_>)
where
    F: for<'a> Fn(Call<'a>) + Copy,
{
    const {
        assert!(
            std::mem::size_of::<F>() == 0,
            "a light callback captures nothing"
        );
    }
    let r = guard::catch(|| {
        if guard::test_panic(TestPanic::Callback) {
            panic!("SNOWLARK_TEST_PANIC=callback: a panic in a V8 callback");
        }
        // A zero-sized `F` has one value; make it as the crate's `UnitType` does.
        let f: F = unit();
        f(Call::new(info))
    });
    if r.is_none() {
        info.with_scope(|scope, _, _| {
            scope.terminate_execution();
        });
    }
}

/// The one value of a zero-sized callback type.
#[inline(always)]
fn unit<F: Copy>() -> F {
    // SAFETY: only called for zero-sized `F` (asserted in `light_call`): a function item or a
    // closure that captures nothing, which has no bytes to initialize.
    unsafe { std::ptr::NonNull::<F>::dangling().as_ptr().read() }
}
