// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `CSSAnimation` and `CSSTransition` objects (CSS Animations 2 §4, CSS Transitions 2 §4) for the CSS
//! animations and transitions stylo runs: made when script meets them (`getAnimations()`, an event's
//! `animation`), they show stylo's timing. Once script drives one through the `Animation` interface
//! (pause, seek, cancel...), its model leads and stylo's animation follows it.

use std::sync::Arc;

use snowlark_bindings::rt::{Cx, Handle};
use snowlark_dom::NodeId;
use snowlark_style::web_animations::{CssKey, CssTiming, Timing};

use super::model::{Model, PlayState};
use super::{AnimationData, CssOwner, EffectData, LazyPromise, ReplaceState};
use crate::engine::{Engine, Object};

impl Engine {
    /// The object of a CSS animation or transition, made when needed.
    fn css_object(&self, doc: NodeId, key: &CssKey) -> Handle {
        if let Some(h) = self.animations.borrow().css.get(key) {
            return *h;
        }
        let pseudo = (!key.pseudo_text().is_empty()).then(|| key.pseudo_text());
        let effect = self.alloc(Object::KeyframeEffect(Box::new(EffectData {
            timing: Timing::default(),
            target: Some(key.element),
            pseudo: pseudo.clone(),
            composite: snowlark_style::web_animations::CompositeOperation::Replace,
            keyframes: Arc::new(Vec::new()),
            animation: None,
        })));
        let css = if key.transition {
            CssOwner::Transition {
                element: key.element,
                pseudo,
                property: key.name.clone(),
            }
        } else {
            CssOwner::Animation {
                element: key.element,
                pseudo,
                name: key.name.clone(),
            }
        };
        let timeline = self.default_timeline(doc);
        let h = self.alloc(Object::Animation(Box::new(AnimationData {
            css: Some(css),
            css_key: Some(key.clone()),
            css_controlled: false,
            id: String::new(),
            timeline: Some(timeline),
            effect: Some(effect),
            model: Model::default(),
            ready: LazyPromise::resolved(),
            finished: LazyPromise::default(),
            replace_state: ReplaceState::Active,
            // (CSS animations sort by their own rules, before script animations)
            sequence: 0,
            finish_queued: false,
            compositor: None,
        })));
        self.effect_mut(effect, |e| e.animation = Some(h));
        self.animations.borrow_mut().css.insert(key.clone(), h);
        h
    }

    /// Brings an object stylo leads up to date with its CSS animation's timing.
    fn css_sync(&self, h: Handle, t: &CssTiming) {
        if self.anim(h, |a| a.css_controlled) {
            return;
        }
        let effect = self.anim_mut(h, |a| {
            a.model.start_time = t.start_time;
            a.model.hold_time = t.hold_time;
            a.model.playback_rate = 1.0;
            a.model.pending_playback_rate = None;
            a.model.pending_play = false;
            a.model.pending_pause = false;
            a.effect
        });
        if let Some(e) = effect {
            self.effect_mut(e, |e| e.timing = t.timing.clone());
        }
    }

    /// The `CSSTransition` and `CSSAnimation` objects of `doc`'s running CSS transitions and
    /// animations, synced, in composite order (CSS Transitions 2 §4.1, CSS Animations 2 §4.2):
    /// transitions before animations, each by their owning element's tree order (the element before
    /// its `::before` and `::after`), transitions then by property name, animations by name position.
    pub(crate) fn css_animation_objects(&self, cx: &mut Cx, doc: NodeId) -> Vec<Handle> {
        if !self.is_rendered_document(doc) {
            return Vec::new();
        }
        let Some(now) = self.document_time(doc) else {
            return Vec::new();
        };
        let mut timings = self
            .with_doc_renderer(doc, |r, _| r.css_timings(now / 1000.0))
            .unwrap_or_default();
        {
            // Each owning element's position in shadow-including tree order.
            let dom = self.dom();
            let mut position: rustc_hash::FxHashMap<NodeId, usize> = timings
                .iter()
                .map(|t| (t.key.element, usize::MAX))
                .collect();
            let mut n = Some(doc);
            let mut i = 0;
            let mut left = position.len();
            while let (Some(id), true) = (n, left > 0) {
                if let Some(p) = position.get_mut(&id) {
                    *p = i;
                    left -= 1;
                }
                i += 1;
                n = dom.next_in_shadow_including_tree_order(id, doc);
            }
            let pseudo_rank = |k: &CssKey| match k.pseudo_text().as_str() {
                "" => 0,
                "::marker" => 1,
                "::before" => 2,
                "::after" => 3,
                _ => 4,
            };
            // (a stable sort: an element's animations stay in their set's order)
            timings.sort_by_cached_key(|t| {
                let k = &t.key;
                (
                    !k.transition,
                    position[&k.element],
                    pseudo_rank(k),
                    if k.transition {
                        k.name.clone()
                    } else {
                        String::new()
                    },
                )
            });
        }
        let mut out = Vec::with_capacity(timings.len());
        for t in &timings {
            let h = self.css_object(doc, &t.key);
            self.css_sync(h, t);
            let _ = self.run_model(cx, h, |m, env, notes| {
                m.update_finished_state(env, false, false, notes);
                Ok(())
            });
            out.push(h);
        }
        // Objects whose animation is gone (finished, or removed by a style change) become idle.
        let gone: Vec<(CssKey, Handle)> = {
            let dom = self.dom();
            self.animations
                .borrow()
                .css
                .iter()
                .filter(|(k, _)| {
                    !timings.iter().any(|t| t.key == **k)
                        && (!dom.is_alive(k.element) || dom.node_document(k.element) == doc)
                })
                .map(|(k, h)| (k.clone(), *h))
                .collect()
        };
        for (k, h) in gone {
            self.animations.borrow_mut().css.remove(&k);
            if !matches!(&*self.obj(h), Object::Animation(_)) {
                continue;
            }
            self.anim_mut(h, |a| {
                a.css_controlled = true;
                a.css_key = None;
            });
            // A finished animation stays finished; one a style change removed is canceled.
            if Engine::play_state(self, h) != PlayState::Finished {
                let _ = self.run_model(cx, h, |m, env, notes| {
                    m.cancel(env, notes);
                    Ok(())
                });
            }
        }
        out
    }

    /// The object for a CSS event's `animation` attribute.
    pub(crate) fn css_event_object(&self, key: &CssKey) -> Option<Handle> {
        if let Some(h) = self.animations.borrow().css.get(key) {
            return Some(*h);
        }
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(key.element) {
                return None;
            }
            dom.node_document(key.element)
        };
        Some(self.css_object(doc, key))
    }

    /// After script drove a CSS animation's object, stylo's animation follows its model: it is put at
    /// the object's current time, paused when the object is, canceled when it is idle.
    pub(crate) fn css_follow_model(&self, h: Handle) {
        let Some(key) = self.anim(h, |a| a.css_key.clone()) else {
            return;
        };
        self.anim_mut(h, |a| a.css_controlled = true);
        let env = self.env(h);
        let (local, state, rate) = self.anim(h, |a| {
            (
                a.model.current_time(&env),
                a.model.play_state(&env),
                a.model.playback_rate,
            )
        });
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(key.element) {
                return;
            }
            dom.node_document(key.element)
        };
        let Some(now) = self.document_time(doc) else {
            return;
        };
        let paused = state == PlayState::Paused || rate == 0.0;
        let local = if state == PlayState::Idle {
            None
        } else {
            local
        };
        self.with_doc_renderer(doc, |r, st| {
            r.css_control(&key, now / 1000.0, local, paused);
            st.dirty = true;
        });
    }
}
