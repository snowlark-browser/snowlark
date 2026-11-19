// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Script syntax checks for the network layer: whether a source text parses as a classic script
//! (ECMAScript `ParseText(sourceText, Script)` returns a Script Record), the last step of Fetch's
//! opaque-response-safelist check. The checks run on a thread of their own with one isolate of its
//! own, started on the first check; nothing is ever run, and the thread ends with the checker.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;

struct Job {
    source: String,
    reply: Box<dyn FnOnce(bool) + Send>,
}

/// A checker: one thread and isolate, started on the first check.
#[derive(Default)]
pub struct ScriptChecker {
    queue: Mutex<Option<Sender<Job>>>,
}

impl ScriptChecker {
    pub fn new() -> ScriptChecker {
        ScriptChecker::default()
    }

    /// Calls `reply`, on the checker's thread, with whether `source` parses as a classic script;
    /// with `false` if the thread cannot be started.
    pub fn check(&self, source: String, reply: Box<dyn FnOnce(bool) + Send>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.is_none() {
            let (tx, rx) = mpsc::channel::<Job>();
            let started = std::thread::Builder::new()
                .name("snowlark-script-check".into())
                .spawn(move || {
                    let mut runtime = crate::Runtime::new();
                    for job in rx {
                        let ok = runtime.parses(&job.source);
                        let large = job.source.len() > 4 * 1024 * 1024;
                        (job.reply)(ok);
                        if large {
                            runtime.low_memory();
                        }
                    }
                });
            if started.is_err() {
                reply(false);
                return;
            }
            *queue = Some(tx);
        }
        let job = Job { source, reply };
        if let Err(mpsc::SendError(job)) = queue.as_ref().expect("set above").send(job) {
            (job.reply)(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn parses(checker: &ScriptChecker, source: &str) -> bool {
        let (tx, rx) = mpsc::channel();
        checker.check(
            source.into(),
            Box::new(move |ok| {
                let _ = tx.send(ok);
            }),
        );
        rx.recv().unwrap()
    }

    #[test]
    fn classic_scripts_parse_and_other_text_does_not() {
        let checker = ScriptChecker::new();
        assert!(parses(&checker, "var a = 1;"));
        assert!(parses(&checker, "window.has_executed_script = true;"));
        assert!(parses(&checker, "function f() { return [1, 2]; }"));
        assert!(parses(&checker, "[1, 2]"));
        // A JSON object is a block whose statement is a labelled nothing: a syntax error.
        assert!(!parses(&checker, "{\"a\": 1}"));
        assert!(!parses(&checker, "<!DOCTYPE html><p>hi"));
        assert!(!parses(&checker, "import x from './x.js';"));
        assert!(!parses(&checker, "function f( {"));
        assert!(!parses(&checker, "\u{FFFD}\u{0}binary"));
    }
}
