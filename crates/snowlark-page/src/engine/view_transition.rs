// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS View Transitions Level 1 and 2, the same-document API at its minimum: `startViewTransition()`
//! on a document or an element runs the update callback (in a task, as after the old state's
//! capture) and settles the transition's promises from it, with no capture, pseudo-elements or
//! animation: `ready` resolves when the callback's promise does, `finished` right after. What sites
//! do around a transition (the DOM update, awaiting the promises) proceeds; nothing animates.

use snowlark_bindings::generated::{
    ViewTransitionImpl, ViewTransitionTypeSetImpl, ViewTransitionUpdateCallback,
    ViewTransitionUpdateCallbackOrStartViewTransitionOptions,
};
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Handle, JsObject, JsPromise, JsValue};
use snowlark_dom::NodeId;

use super::{exception, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// A `ViewTransition`.
pub struct TransitionData {
    pub update_callback_done: JsPromise,
    pub ready: JsPromise,
    pub finished: JsPromise,
    pub types: Handle,
    /// The transition root (the document element, or the element it was started on), and the key
    /// of the active transition (the document or the element).
    pub root: NodeId,
    pub scope: NodeId,
    /// `ready` was settled (resolved, or rejected by skipping).
    pub ready_settled: bool,
}

impl Engine {
    fn transition<R>(&self, h: Handle, f: impl FnOnce(&mut TransitionData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ViewTransition(t) => f(t),
            _ => panic!("not a ViewTransition"),
        }
    }

    /// The active view transition of a document or an element.
    pub(crate) fn active_view_transition(&self, scope: NodeId) -> Option<Handle> {
        self.view_transitions.borrow().get(&scope).copied()
    }

    /// CSS View Transitions §7.1 `startViewTransition()` for `scope` (a document or an element).
    pub(crate) fn start_view_transition(
        &self,
        cx: &mut Cx,
        scope: NodeId,
        options: ViewTransitionUpdateCallbackOrStartViewTransitionOptions,
    ) -> rt::Result<Handle> {
        let (callback, types) = match options {
            ViewTransitionUpdateCallbackOrStartViewTransitionOptions::ViewTransitionUpdateCallback(
                f,
            ) => (Some(f), Vec::new()),
            ViewTransitionUpdateCallbackOrStartViewTransitionOptions::StartViewTransitionOptions(
                o,
            ) => (o.update, o.types.unwrap_or_default()),
        };
        let root = {
            let dom = self.dom();
            if dom.is_document(scope) {
                dom.document_element(scope).unwrap_or(scope)
            } else {
                scope
            }
        };
        let promise = |cx: &mut Cx| JsPromise::new(cx).ok_or(Error::Pending);
        let types = self.alloc(Object::ViewTransitionTypeSet(types));
        let transition = self.alloc(Object::ViewTransition(Box::new(TransitionData {
            update_callback_done: promise(cx)?,
            ready: promise(cx)?,
            finished: promise(cx)?,
            types,
            root,
            scope,
            ready_settled: false,
        })));
        // A transition already active in the scope is skipped.
        let previous = self.view_transitions.borrow_mut().insert(scope, transition);
        if let Some(previous) = previous {
            self.skip_transition_with(cx, previous, "Skipped by a new view transition");
        }
        self.queue_task(TaskSource::Rendering, move |e, cx| {
            e.run_update_callback(cx, transition, callback);
        });
        Ok(transition)
    }

    /// "Call the update callback", then settle the transition's promises from its outcome.
    fn run_update_callback(
        &self,
        cx: &mut Cx,
        transition: Handle,
        callback: Option<ViewTransitionUpdateCallback>,
    ) {
        // The callback's promise, or one rejected with what it threw.
        let outcome: JsValue = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let mut cx2 = Cx::new(tc);
            let r = match &callback {
                Some(f) => f.invoke(&mut cx2, None).map(Some),
                None => Ok(None),
            };
            match r {
                Ok(Some(p)) => {
                    let o = v8::Local::new(tc, p.global());
                    JsValue::new(tc, o.into())
                }
                Ok(None) => JsValue::new(tc, v8::undefined(tc).into()),
                Err(_) => {
                    let thrown = tc.exception();
                    tc.reset();
                    let Some(resolver) = v8::PromiseResolver::new(tc) else {
                        return;
                    };
                    let reason = thrown.unwrap_or_else(|| v8::undefined(tc).into());
                    resolver.reject(tc, reason);
                    let p = resolver.get_promise(tc);
                    JsValue::new(tc, p.into())
                }
            }
        };
        self.upon(
            cx,
            &outcome,
            Box::new(move |e, cx, r| {
                let (done, ready, finished, settled, scope) = e.transition(transition, |t| {
                    let settled = std::mem::replace(&mut t.ready_settled, true);
                    (
                        t.update_callback_done.clone(),
                        t.ready.clone(),
                        t.finished.clone(),
                        settled,
                        t.scope,
                    )
                });
                match r {
                    Ok(_) => {
                        done.resolve(cx, ());
                        if !settled {
                            ready.resolve(cx, ());
                        }
                        finished.resolve(cx, ());
                    }
                    Err(reason) => {
                        done.reject_value(cx, &reason);
                        if !settled {
                            ready.reject_value(cx, &reason);
                            ready.mark_handled(cx);
                        }
                        finished.reject_value(cx, &reason);
                    }
                }
                let mut active = e.view_transitions.borrow_mut();
                if active.get(&scope) == Some(&transition) {
                    active.remove(&scope);
                }
            }),
        );
    }

    /// "Skip the view transition": `ready` rejects with an AbortError (handled); the update
    /// callback still runs and settles the rest.
    fn skip_transition_with(&self, cx: &mut Cx, transition: Handle, message: &'static str) {
        let (ready, settled) = self.transition(transition, |t| {
            (
                t.ready.clone(),
                std::mem::replace(&mut t.ready_settled, true),
            )
        });
        if !settled {
            ready.reject(cx, exception(ExceptionName::AbortError, message));
            ready.mark_handled(cx);
        }
    }
}

impl ViewTransitionImpl for Engine {
    fn update_callback_done(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.transition(this, |t| t.update_callback_done.clone()))
    }
    fn ready(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.transition(this, |t| t.ready.clone()))
    }
    fn finished(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.transition(this, |t| t.finished.clone()))
    }
    fn types(&self, this: Handle) -> Handle {
        self.transition(this, |t| t.types)
    }
    fn transition_root(&self, this: Handle) -> Handle {
        node_handle(self.transition(this, |t| t.root))
    }
    fn skip_transition(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.skip_transition_with(cx, this, "Transition was skipped");
        Ok(())
    }
    /// (With no animation there is nothing for the promise to keep alive.)
    fn wait_until(&self, _cx: &mut Cx, _this: Handle, _promise: JsObject) -> rt::Result<()> {
        Ok(())
    }
}

impl Engine {
    fn type_set<R>(&self, h: Handle, f: impl FnOnce(&mut Vec<String>) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ViewTransitionTypeSet(v) => f(v),
            _ => f(&mut Vec::new()),
        }
    }
}

impl ViewTransitionTypeSetImpl for Engine {
    fn setlike_at(&self, this: Handle, index: u32) -> Option<String> {
        self.type_set(this, |v| v.get(index as usize).cloned())
    }
    fn setlike_size(&self, this: Handle) -> u32 {
        self.type_set(this, |v| v.len() as u32)
    }
    fn setlike_has(&self, this: Handle, value: &str) -> bool {
        self.type_set(this, |v| v.iter().any(|t| t == value))
    }
    fn setlike_add(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.type_set(this, |v| {
            if !v.iter().any(|t| t == value) {
                v.push(value.to_owned());
            }
        });
        Ok(())
    }
    fn setlike_delete(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<bool> {
        Ok(self.type_set(this, |v| {
            let before = v.len();
            v.retain(|t| t != value);
            v.len() != before
        }))
    }
    fn setlike_clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.type_set(this, |v| v.clear());
        Ok(())
    }
}
