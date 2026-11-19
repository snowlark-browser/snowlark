// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 §4.5: an animation's playback state machine (start time, hold time, playback rate,
//! pending tasks, the finished state) without the objects around it. The procedures follow the spec's
//! steps; what they do to promises and events comes out as [`Note`]s for the engine to carry out.

/// What an animation's surroundings say at the moment of a procedure: its timeline's time and its
/// associated effect's end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Env {
    /// The animation has a timeline.
    pub has_timeline: bool,
    /// The timeline's current time (`None`: no timeline, or inactive).
    pub timeline_time: Option<f64>,
    /// The associated effect end (§4.5.12): the effect's end time, 0 without an effect.
    pub effect_end: f64,
}

/// The promise and event work a procedure asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Note {
    /// The current ready promise is replaced with a new pending one.
    NewReady,
    /// Resolve the current ready promise with the animation.
    ResolveReady,
    /// "Reset an animation's pending tasks": reject the current ready promise with an AbortError
    /// (handled), then a new resolved one replaces it.
    RejectReady,
    /// Queue a microtask to run the finish notification steps (unless one is queued).
    QueueFinishNotification,
    /// Run the finish notification steps now (a queued microtask for them no longer runs them).
    FinishNotificationNow,
    /// The current finished promise is replaced with a new pending one (it was resolved).
    NewFinished,
    /// Cancel: reject the current finished promise with an AbortError (handled) and replace it.
    RejectFinished,
    /// Queue a `cancel` event, with the timeline time given.
    CancelEvent(Option<f64>),
}

/// The play states (§4.5.17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Idle,
    Running,
    Paused,
    Finished,
}

/// Why a procedure threw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Throw {
    /// A TypeError (seeking to an unresolved time while the current time is resolved).
    Type,
    /// An "InvalidStateError" DOMException.
    InvalidState,
}

/// An animation's timing state.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub start_time: Option<f64>,
    pub hold_time: Option<f64>,
    pub playback_rate: f64,
    pub pending_playback_rate: Option<f64>,
    pub pending_play: bool,
    pub pending_pause: bool,
    pub previous_current_time: Option<f64>,
    /// The current finished promise is resolved.
    pub finished_resolved: bool,
}

impl Default for Model {
    fn default() -> Model {
        Model {
            start_time: None,
            hold_time: None,
            playback_rate: 1.0,
            pending_playback_rate: None,
            pending_play: false,
            pending_pause: false,
            previous_current_time: None,
            finished_resolved: false,
        }
    }
}

impl Model {
    pub fn pending(&self) -> bool {
        self.pending_play || self.pending_pause
    }

    /// The effective playback rate (§4.5.15.2).
    pub fn effective_rate(&self) -> f64 {
        self.pending_playback_rate.unwrap_or(self.playback_rate)
    }

    /// §4.5.1 the current time.
    pub fn current_time(&self, env: &Env) -> Option<f64> {
        if let Some(h) = self.hold_time {
            return Some(h);
        }
        let (Some(t), Some(s)) = (env.timeline_time, self.start_time) else {
            return None;
        };
        Some((t - s) * self.playback_rate)
    }

    /// §4.5.17 the play state.
    pub fn play_state(&self, env: &Env) -> PlayState {
        let current = self.current_time(env);
        if current.is_none() && self.start_time.is_none() && !self.pending() {
            return PlayState::Idle;
        }
        if self.pending_pause || (self.start_time.is_none() && !self.pending_play) {
            return PlayState::Paused;
        }
        if let Some(c) = current {
            let rate = self.effective_rate();
            if (rate > 0.0 && c >= env.effect_end) || (rate < 0.0 && c <= 0.0) {
                return PlayState::Finished;
            }
        }
        PlayState::Running
    }

    /// "Apply any pending playback rate".
    pub fn apply_pending_rate(&mut self) {
        if let Some(r) = self.pending_playback_rate.take() {
            self.playback_rate = r;
        }
    }

    /// §4.5.4 "silently set the current time".
    pub fn silently_set_current_time(&mut self, env: &Env, seek: Option<f64>) -> Result<(), Throw> {
        let Some(seek) = seek else {
            if self.current_time(env).is_some() {
                return Err(Throw::Type);
            }
            return Ok(());
        };
        match env.timeline_time {
            Some(t)
                if self.hold_time.is_none()
                    && self.start_time.is_some()
                    && self.playback_rate != 0.0 =>
            {
                self.start_time = Some(t - seek / self.playback_rate);
            }
            _ => self.hold_time = Some(seek),
        }
        if env.timeline_time.is_none() {
            self.start_time = None;
        }
        self.previous_current_time = None;
        Ok(())
    }

    /// §4.5.4 "set the current time".
    pub fn set_current_time(
        &mut self,
        env: &Env,
        seek: Option<f64>,
        notes: &mut Vec<Note>,
    ) -> Result<(), Throw> {
        self.silently_set_current_time(env, seek)?;
        if self.pending_pause {
            self.hold_time = seek;
            self.apply_pending_rate();
            self.start_time = None;
            self.pending_pause = false;
            notes.push(Note::ResolveReady);
        }
        self.update_finished_state(env, true, false, notes);
        Ok(())
    }

    /// §4.5.5 "set the start time".
    pub fn set_start_time(&mut self, env: &Env, new: Option<f64>, notes: &mut Vec<Note>) {
        if env.timeline_time.is_none() && new.is_some() {
            self.hold_time = None;
        }
        let previous = self.current_time(env);
        self.apply_pending_rate();
        self.start_time = new;
        if new.is_some() {
            if self.playback_rate != 0.0 {
                self.hold_time = None;
            }
        } else {
            self.hold_time = previous;
        }
        if self.pending() {
            self.pending_play = false;
            self.pending_pause = false;
            notes.push(Note::ResolveReady);
        }
        self.update_finished_state(env, true, false, notes);
    }

    /// §4.5.8 "play an animation".
    pub fn play(
        &mut self,
        env: &Env,
        auto_rewind: bool,
        notes: &mut Vec<Note>,
    ) -> Result<(), Throw> {
        let aborted_pause = self.pending_pause;
        let mut has_pending_ready = false;
        let mut seek = None;
        let current = self.current_time(env);
        let rate = self.effective_rate();
        let end = env.effect_end;
        if auto_rewind {
            if rate >= 0.0 && current.is_none_or(|c| c < 0.0 || c >= end) {
                seek = Some(0.0);
            } else if rate < 0.0 && current.is_none_or(|c| c <= 0.0 || c > end) {
                if end == f64::INFINITY {
                    return Err(Throw::InvalidState);
                }
                seek = Some(end);
            }
        }
        if seek.is_none() && self.start_time.is_none() && current.is_none() {
            seek = Some(0.0);
        }
        // (document timelines are monotonically increasing: no finite timeline here)
        if let Some(s) = seek {
            self.hold_time = Some(s);
        }
        if self.hold_time.is_some() {
            self.start_time = None;
        }
        if self.pending() {
            self.pending_play = false;
            self.pending_pause = false;
            has_pending_ready = true;
        }
        if self.hold_time.is_none()
            && seek.is_none()
            && !aborted_pause
            && self.pending_playback_rate.is_none()
        {
            return Ok(());
        }
        if !has_pending_ready {
            notes.push(Note::NewReady);
        }
        self.pending_play = true;
        self.update_finished_state(env, false, false, notes);
        Ok(())
    }

    /// §4.5.9 "pause an animation".
    pub fn pause(&mut self, env: &Env, notes: &mut Vec<Note>) -> Result<(), Throw> {
        if self.pending_pause || self.play_state(env) == PlayState::Paused {
            return Ok(());
        }
        let mut seek = None;
        if self.current_time(env).is_none() {
            if self.playback_rate >= 0.0 {
                seek = Some(0.0);
            } else {
                if env.effect_end == f64::INFINITY {
                    return Err(Throw::InvalidState);
                }
                seek = Some(env.effect_end);
            }
        }
        if seek.is_some() {
            self.hold_time = seek;
        }
        let mut has_pending_ready = false;
        if self.pending_play {
            self.pending_play = false;
            has_pending_ready = true;
        }
        if !has_pending_ready {
            notes.push(Note::NewReady);
        }
        self.pending_pause = true;
        self.update_finished_state(env, false, false, notes);
        Ok(())
    }

    /// The pending play task and pending pause task, run once the animation is ready (§4.5.8, §4.5.9)
    /// at `ready_time` (the timeline's time).
    pub fn run_pending_tasks(&mut self, env: &Env, ready_time: f64, notes: &mut Vec<Note>) {
        if self.pending_play {
            self.pending_play = false;
            if let Some(hold) = self.hold_time {
                self.apply_pending_rate();
                let start = if self.playback_rate == 0.0 {
                    ready_time
                } else {
                    ready_time - hold / self.playback_rate
                };
                self.start_time = Some(start);
                if self.playback_rate != 0.0 {
                    self.hold_time = None;
                }
            } else if let (Some(start), Some(_)) = (self.start_time, self.pending_playback_rate) {
                let to_match = (ready_time - start) * self.playback_rate;
                self.apply_pending_rate();
                if self.playback_rate == 0.0 {
                    self.hold_time = Some(to_match);
                }
                let new_start = if self.playback_rate == 0.0 {
                    ready_time
                } else {
                    ready_time - to_match / self.playback_rate
                };
                self.start_time = Some(new_start);
            }
            notes.push(Note::ResolveReady);
            self.update_finished_state(env, false, false, notes);
        } else if self.pending_pause {
            self.pending_pause = false;
            if let (Some(start), None) = (self.start_time, self.hold_time) {
                self.hold_time = Some((ready_time - start) * self.playback_rate);
            }
            self.apply_pending_rate();
            self.start_time = None;
            notes.push(Note::ResolveReady);
            self.update_finished_state(env, false, false, notes);
        }
    }

    /// §4.5.12 "update an animation's finished state".
    pub fn update_finished_state(
        &mut self,
        env: &Env,
        did_seek: bool,
        sync: bool,
        notes: &mut Vec<Note>,
    ) {
        let unconstrained = if did_seek {
            self.current_time(env)
        } else {
            match (env.timeline_time, self.start_time) {
                (Some(t), Some(s)) => Some((t - s) * self.playback_rate),
                _ => None,
            }
        };
        if let (Some(u), Some(_), false) = (unconstrained, self.start_time, self.pending()) {
            let end = env.effect_end;
            if self.playback_rate > 0.0 && u >= end {
                self.hold_time = Some(if did_seek {
                    u
                } else {
                    match self.previous_current_time {
                        Some(p) => p.max(end),
                        None => end,
                    }
                });
            } else if self.playback_rate < 0.0 && u <= 0.0 {
                self.hold_time = Some(if did_seek {
                    u
                } else {
                    match self.previous_current_time {
                        Some(p) => p.min(0.0),
                        None => 0.0,
                    }
                });
            } else if let (true, Some(t)) = (self.playback_rate != 0.0, env.timeline_time) {
                if let (true, Some(h)) = (did_seek, self.hold_time) {
                    self.start_time = Some(t - h / self.playback_rate);
                }
                self.hold_time = None;
            }
        }
        self.previous_current_time = self.current_time(env);
        let finished = self.play_state(env) == PlayState::Finished;
        if finished && !self.finished_resolved {
            notes.push(if sync {
                Note::FinishNotificationNow
            } else {
                Note::QueueFinishNotification
            });
        }
        if !finished && self.finished_resolved {
            self.finished_resolved = false;
            notes.push(Note::NewFinished);
        }
    }

    /// The finish notification steps' state part (§4.5.12): whether they go on (the animation is
    /// still finished), in which case the finished promise counts as resolved.
    pub fn finish_notification(&mut self, env: &Env) -> bool {
        if self.play_state(env) != PlayState::Finished {
            return false;
        }
        self.finished_resolved = true;
        true
    }

    /// §4.5.13 "finish an animation".
    pub fn finish(&mut self, env: &Env, notes: &mut Vec<Note>) -> Result<(), Throw> {
        let rate = self.effective_rate();
        if rate == 0.0 || (rate > 0.0 && env.effect_end == f64::INFINITY) {
            return Err(Throw::InvalidState);
        }
        self.apply_pending_rate();
        let limit = if self.playback_rate > 0.0 {
            env.effect_end
        } else {
            0.0
        };
        let _ = self.silently_set_current_time(env, Some(limit));
        if self.start_time.is_none() {
            if let Some(t) = env.timeline_time {
                self.start_time = Some(t - limit / self.playback_rate);
            }
        }
        if self.pending_pause && self.start_time.is_some() {
            self.hold_time = None;
            self.pending_pause = false;
            notes.push(Note::ResolveReady);
        }
        if self.pending_play && self.start_time.is_some() {
            self.pending_play = false;
            notes.push(Note::ResolveReady);
        }
        self.update_finished_state(env, true, true, notes);
        Ok(())
    }

    /// "Reset an animation's pending tasks" (§4.5.14).
    pub fn reset_pending_tasks(&mut self, notes: &mut Vec<Note>) {
        if !self.pending() {
            return;
        }
        self.pending_play = false;
        self.pending_pause = false;
        self.apply_pending_rate();
        notes.push(Note::RejectReady);
    }

    /// §4.5.14 "cancel an animation".
    pub fn cancel(&mut self, env: &Env, notes: &mut Vec<Note>) {
        if self.play_state(env) != PlayState::Idle {
            self.reset_pending_tasks(notes);
            self.finished_resolved = false;
            notes.push(Note::RejectFinished);
            notes.push(Note::CancelEvent(env.timeline_time));
        }
        self.hold_time = None;
        self.start_time = None;
    }

    /// §4.5.15.1 "set the playback rate".
    pub fn set_playback_rate(&mut self, env: &Env, rate: f64, notes: &mut Vec<Note>) {
        self.pending_playback_rate = None;
        let previous = self.current_time(env);
        self.playback_rate = rate;
        if env.has_timeline {
            if let Some(p) = previous {
                let _ = self.set_current_time(env, Some(p), notes);
            }
        }
        self.update_finished_state(env, true, false, notes);
    }

    /// §4.5.15.2 "seamlessly update the playback rate".
    pub fn update_playback_rate(
        &mut self,
        env: &Env,
        rate: f64,
        notes: &mut Vec<Note>,
    ) -> Result<(), Throw> {
        let previous_state = self.play_state(env);
        self.pending_playback_rate = Some(rate);
        if self.pending() {
            return Ok(());
        }
        if matches!(previous_state, PlayState::Idle | PlayState::Paused)
            || self.current_time(env).is_none()
        {
            self.apply_pending_rate();
        } else if previous_state == PlayState::Finished {
            let unconstrained = match (env.timeline_time, self.start_time) {
                (Some(t), Some(s)) => Some((t - s) * self.playback_rate),
                _ => None,
            };
            if let (Some(t), Some(u)) = (env.timeline_time, unconstrained) {
                self.start_time = Some(if rate == 0.0 { t } else { t - u / rate });
            }
            self.apply_pending_rate();
            self.update_finished_state(env, false, false, notes);
        } else {
            self.play(env, false, notes)?;
        }
        Ok(())
    }

    /// §4.5.16 "reverse an animation".
    pub fn reverse(&mut self, env: &Env, notes: &mut Vec<Note>) -> Result<(), Throw> {
        if env.timeline_time.is_none() {
            return Err(Throw::InvalidState);
        }
        let original = self.pending_playback_rate;
        self.pending_playback_rate = Some(-self.effective_rate());
        let mut own = Vec::new();
        if let Err(e) = self.play(env, true, &mut own) {
            self.pending_playback_rate = original;
            return Err(e);
        }
        notes.extend(own);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(t: f64, end: f64) -> Env {
        Env {
            has_timeline: true,
            timeline_time: Some(t),
            effect_end: end,
        }
    }

    #[test]
    fn play_waits_for_ready_then_runs() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        let e = env(100.0, 1000.0);
        assert_eq!(m.play_state(&e), PlayState::Idle);
        m.play(&e, true, &mut notes).unwrap();
        assert_eq!(notes, vec![Note::NewReady]);
        assert!(m.pending_play);
        assert_eq!(m.play_state(&e), PlayState::Running);
        assert_eq!(m.current_time(&e), Some(0.0));
        notes.clear();
        let e = env(116.0, 1000.0);
        m.run_pending_tasks(&e, 116.0, &mut notes);
        assert_eq!(notes, vec![Note::ResolveReady]);
        assert_eq!(m.start_time, Some(116.0));
        assert_eq!(m.hold_time, None);
        assert_eq!(m.current_time(&env(616.0, 1000.0)), Some(500.0));
    }

    #[test]
    fn finishes_and_notifies() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        m.play(&env(0.0, 1000.0), true, &mut notes).unwrap();
        m.run_pending_tasks(&env(0.0, 1000.0), 0.0, &mut notes);
        notes.clear();
        let e = env(1500.0, 1000.0);
        m.update_finished_state(&e, false, false, &mut notes);
        assert_eq!(m.play_state(&e), PlayState::Finished);
        // The hold time sticks at the end.
        assert_eq!(m.current_time(&e), Some(1000.0));
        assert_eq!(notes, vec![Note::QueueFinishNotification]);
        assert!(m.finish_notification(&e));
        // Seeking back leaves the finished state: a new finished promise.
        notes.clear();
        m.set_current_time(&e, Some(200.0), &mut notes).unwrap();
        assert_eq!(notes, vec![Note::NewFinished]);
        assert_eq!(m.play_state(&e), PlayState::Running);
        assert_eq!(m.start_time, Some(1300.0));
    }

    #[test]
    fn pause_holds_the_time() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        m.play(&env(0.0, 1000.0), true, &mut notes).unwrap();
        m.run_pending_tasks(&env(0.0, 1000.0), 0.0, &mut notes);
        notes.clear();
        m.pause(&env(300.0, 1000.0), &mut notes).unwrap();
        assert_eq!(notes, vec![Note::NewReady]);
        assert_eq!(m.play_state(&env(300.0, 1000.0)), PlayState::Paused);
        m.run_pending_tasks(&env(320.0, 1000.0), 320.0, &mut notes);
        assert_eq!(m.hold_time, Some(320.0));
        assert_eq!(m.start_time, None);
        assert_eq!(m.current_time(&env(900.0, 1000.0)), Some(320.0));
    }

    #[test]
    fn cancel_and_finish_and_reverse() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        let e = env(0.0, 1000.0);
        m.play(&e, true, &mut notes).unwrap();
        notes.clear();
        m.cancel(&e, &mut notes);
        assert_eq!(
            notes,
            vec![
                Note::RejectReady,
                Note::RejectFinished,
                Note::CancelEvent(Some(0.0))
            ]
        );
        assert_eq!(m.play_state(&e), PlayState::Idle);
        notes.clear();
        m.finish(&e, &mut notes).unwrap();
        assert_eq!(m.current_time(&e), Some(1000.0));
        assert!(notes.contains(&Note::FinishNotificationNow));
        // An infinite effect cannot finish forwards.
        let inf = env(0.0, f64::INFINITY);
        assert_eq!(
            Model::default().finish(&inf, &mut notes),
            Err(Throw::InvalidState)
        );
        // Reversing an idle animation of an infinite effect throws and keeps the rate.
        let mut r = Model::default();
        assert_eq!(r.reverse(&inf, &mut notes), Err(Throw::InvalidState));
        assert_eq!(r.pending_playback_rate, None);
        // Reversing a running one plays it backwards from where it is.
        let mut r = Model::default();
        r.play(&e, true, &mut notes).unwrap();
        r.run_pending_tasks(&e, 0.0, &mut notes);
        let e2 = env(400.0, 1000.0);
        r.reverse(&e2, &mut notes).unwrap();
        assert_eq!(r.effective_rate(), -1.0);
        r.run_pending_tasks(&e2, 400.0, &mut notes);
        assert_eq!(r.current_time(&env(500.0, 1000.0)), Some(300.0));
    }

    #[test]
    fn seeking_needs_a_resolved_time() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        let e = env(0.0, 1000.0);
        assert_eq!(m.set_current_time(&e, None, &mut notes), Ok(()));
        m.set_current_time(&e, Some(10.0), &mut notes).unwrap();
        assert_eq!(m.play_state(&e), PlayState::Paused);
        assert_eq!(m.set_current_time(&e, None, &mut notes), Err(Throw::Type));
        // Without a timeline only the hold time is set.
        let none = Env {
            has_timeline: false,
            timeline_time: None,
            effect_end: 1000.0,
        };
        let mut n = Model::default();
        n.set_start_time(&none, Some(5.0), &mut notes);
        assert_eq!(n.hold_time, None);
        assert_eq!(n.current_time(&none), None);
    }

    #[test]
    fn playback_rate_changes() {
        let mut m = Model::default();
        let mut notes = Vec::new();
        let e = env(0.0, 1000.0);
        m.play(&e, true, &mut notes).unwrap();
        m.run_pending_tasks(&e, 0.0, &mut notes);
        let e = env(100.0, 1000.0);
        m.set_playback_rate(&e, 2.0, &mut notes);
        assert_eq!(m.current_time(&e), Some(100.0));
        assert_eq!(m.current_time(&env(200.0, 1000.0)), Some(300.0));
        // A seamless update waits for the next pending play task.
        m.update_playback_rate(&env(200.0, 1000.0), 1.0, &mut notes)
            .unwrap();
        assert!(m.pending_play);
        assert_eq!(m.effective_rate(), 1.0);
        m.run_pending_tasks(&env(250.0, 1000.0), 250.0, &mut notes);
        assert_eq!(m.playback_rate, 1.0);
        assert_eq!(m.current_time(&env(250.0, 1000.0)), Some(400.0));
    }
}
