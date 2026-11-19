// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! JS values the engine holds for a platform object (a listener's or observer's callback, a custom
//! element definition's constructor, `history.state`, a pending promise): strong while the object's
//! realm is alive for good, and, once nothing outside JS must keep them (their target's wrapper is weak,
//! their browsing context was discarded), kept by a wrapper instead ([`rt::keep_with_wrapper`]) and
//! held weakly here. A strong handle to a value whose closure or prototype chain reaches the holder's
//! realm would otherwise keep that realm alive for good (engine/gc.rs).

use snowlark_bindings::rt::{self, Handle, JsObject, JsPromise, JsValue};

pub enum Held<T> {
    Strong(v8::Global<T>),
    Traced(v8::Weak<T>),
}

impl<T> Clone for Held<T> {
    fn clone(&self) -> Self {
        match self {
            Held::Strong(g) => Held::Strong(g.clone()),
            Held::Traced(w) => Held::Traced(w.clone()),
        }
    }
}

impl<T> Held<T> {
    /// The value (none once a traced value died with the wrapper that kept it).
    pub fn local<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, T>> {
        match self {
            Held::Strong(g) => Some(v8::Local::new(scope, g)),
            Held::Traced(w) => w.to_local(scope),
        }
    }

    /// Whether the value is held strongly.
    pub fn is_strong(&self) -> bool {
        matches!(self, Held::Strong(_))
    }

    /// From now on `owner`'s wrapper keeps the value and it is held weakly here. Does nothing when
    /// `owner` has no live wrapper (the value stays strong).
    pub fn trace(&mut self, scope: &mut v8::PinScope<'_, '_>, owner: Handle)
    where
        for<'s> v8::Local<'s, T>: Into<v8::Local<'s, v8::Value>>,
    {
        if let Held::Strong(g) = self {
            let local = v8::Local::new(scope, &*g);
            if rt::keep_with_wrapper(scope, owner, local.into()) {
                *self = Held::Traced(v8::Weak::new(scope, local));
            }
        }
    }
}

impl Held<v8::Object> {
    pub fn object(o: JsObject) -> Self {
        Held::Strong(o.global().clone())
    }

    /// The callback object (none once a traced one died with the wrapper that kept it).
    pub fn get(&self, scope: &v8::PinScope<'_, '_>) -> Option<JsObject> {
        self.local(scope).map(|o| JsObject::new(scope, o))
    }

    /// Whether the held object is `other` (the same object).
    pub fn is(&self, scope: &v8::PinScope<'_, '_>, other: &JsObject) -> bool {
        match self {
            Held::Strong(g) => g == other.global(),
            Held::Traced(w) => w
                .to_local(scope)
                .is_some_and(|o| o == v8::Local::new(scope, other.global())),
        }
    }
}

impl Held<v8::Value> {
    pub fn value(v: JsValue) -> Self {
        Held::Strong(v.global().clone())
    }

    pub fn get(&self, scope: &v8::PinScope<'_, '_>) -> Option<JsValue> {
        self.local(scope).map(|v| JsValue::new(scope, v))
    }
}

impl Held<v8::PromiseResolver> {
    pub fn promise(p: JsPromise) -> Self {
        Held::Strong(p.resolver().clone())
    }

    pub fn get(&self, scope: &v8::PinScope<'_, '_>) -> Option<JsPromise> {
        self.local(scope)
            .map(|r| JsPromise::from_resolver(v8::Global::new(scope, r)))
    }
}
