// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An OfflineAudioContext's rendering (§1.3.4 `startRendering()`): a thread made for the
//! occasion renders `length` sample-frames into a buffer, applying the control thread's messages
//! and stopping at its scheduled suspensions at render quantum boundaries.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::graph::{Control, Graph, Notice};
use crate::RENDER_QUANTUM as N;

/// What the rendering tells the control thread.
#[derive(Debug, PartialEq)]
pub enum OfflineNotice {
    Graph(Notice),
    /// Rendering stopped at this frame for a scheduled `suspend()`.
    Suspended(u64),
    /// Rendering is done: the rendered channels.
    Complete(Vec<Vec<f32>>),
}

/// `suspend()` refused the time (InvalidStateError).
#[derive(Debug, PartialEq, Eq)]
pub enum SuspendError {
    /// The frame is already rendered.
    Past,
    /// A suspension is already scheduled at that frame.
    Taken,
}

#[derive(Default)]
struct State {
    controls: Vec<Control>,
    /// Sample-frames rendered (whole quanta).
    frame: u64,
    suspends: BTreeSet<u64>,
    resume: bool,
    stop: bool,
    started: bool,
    /// Stopped at a suspension, waiting for `resume()`.
    waiting: bool,
    notices: Vec<OfflineNotice>,
}

/// The control thread's end of an offline context: its control message queue, its scheduled
/// suspensions, and the notices coming back. Before rendering starts the messages wait here.
pub struct Link {
    state: Mutex<State>,
    cv: Condvar,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Link {
    /// A link whose rendering thread calls `wake` after queueing notices.
    pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Arc<Link> {
        Arc::new(Link {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            wake,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a control message.
    pub fn send(&self, msg: Control) {
        self.lock().controls.push(msg);
    }

    /// The sample-frame the rendering has reached (a multiple of the render quantum).
    pub fn frame(&self) -> u64 {
        self.lock().frame
    }

    /// Schedules a suspension at `frame` (a render quantum boundary).
    pub fn add_suspend(&self, frame: u64) -> Result<(), SuspendError> {
        let mut s = self.lock();
        if frame < s.frame {
            return Err(SuspendError::Past);
        }
        if !s.suspends.insert(frame) {
            return Err(SuspendError::Taken);
        }
        Ok(())
    }

    /// Lets a suspended rendering continue.
    pub fn resume(&self) {
        let mut s = self.lock();
        s.resume = true;
        // (busy from now on, before the rendering thread wakes)
        s.waiting = false;
        drop(s);
        self.cv.notify_all();
    }

    /// Ends the rendering where it is (the context went away).
    pub fn stop(&self) {
        self.lock().stop = true;
        self.cv.notify_all();
    }

    /// Takes the notices queued so far.
    pub fn take_notices(&self) -> Vec<OfflineNotice> {
        std::mem::take(&mut self.lock().notices)
    }

    /// Whether rendering started and has not completed or stopped.
    pub fn rendering(&self) -> bool {
        let s = self.lock();
        s.started && !s.stop
    }

    /// Whether the rendering will report without the control thread doing anything: it runs (not
    /// waiting at a suspension), or notices wait to be taken.
    pub fn busy(&self) -> bool {
        let s = self.lock();
        (s.started && !s.stop && !s.waiting) || !s.notices.is_empty()
    }
}

/// Starts rendering `graph` into `channels` channels of `length` frames on a new thread.
pub fn start(graph: Graph, channels: usize, length: u64, link: Arc<Link>) -> std::io::Result<()> {
    link.lock().started = true;
    std::thread::Builder::new()
        .name("offline audio".into())
        .spawn(move || render(graph, channels, length, &link))
        .map(|_| ())
}

fn render(mut graph: Graph, channels: usize, length: u64, link: &Link) {
    let mut out = vec![vec![0.0f32; length as usize]; channels];
    let sr = graph.sample_rate();
    let mut frame = 0u64;
    while frame < length {
        {
            let mut s = link.lock();
            if s.stop {
                return;
            }
            let now = frame as f64 / sr;
            for c in std::mem::take(&mut s.controls) {
                graph.apply(c, now);
            }
            if s.suspends.remove(&frame) {
                s.notices.push(OfflineNotice::Suspended(frame));
                s.waiting = true;
                drop(s);
                (link.wake)();
                s = link.lock();
                while !s.resume && !s.stop {
                    s = link.cv.wait(s).unwrap_or_else(|e| e.into_inner());
                }
                s.resume = false;
                s.waiting = false;
                if s.stop {
                    return;
                }
                for c in std::mem::take(&mut s.controls) {
                    graph.apply(c, now);
                }
            }
        }
        graph.render(frame);
        let n = (length - frame).min(N as u64) as usize;
        let rendered = graph.output();
        for (c, dst) in out.iter_mut().enumerate() {
            if c < rendered.channels() {
                dst[frame as usize..frame as usize + n].copy_from_slice(&rendered.channel(c)[..n]);
            }
        }
        frame += N as u64;
        let notices = graph.take_notices();
        let mut s = link.lock();
        s.frame = frame;
        if !notices.is_empty() {
            s.notices
                .extend(notices.into_iter().map(OfflineNotice::Graph));
            drop(s);
            (link.wake)();
        }
    }
    let mut s = link.lock();
    s.notices.push(OfflineNotice::Complete(out));
    s.started = false;
    drop(s);
    (link.wake)();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ChannelConfig, CountMode, Interpretation};
    use crate::graph::{NewParam, SharedValue, DESTINATION};
    use crate::node::{Kind, Schedule};
    use crate::param::{ParamDesc, Rate};
    use std::sync::mpsc;

    fn wait_for(link: &Link, rx: &mpsc::Receiver<()>) -> Vec<OfflineNotice> {
        loop {
            let n = link.take_notices();
            if !n.is_empty() {
                return n;
            }
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .expect("a notice");
        }
    }

    #[test]
    fn renders_suspends_and_completes() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let link = Link::new(Box::new(move || {
            let _ = tx.lock().unwrap().send(());
        }));
        let mut g = Graph::new(1000.0);
        g.apply(
            Control::AddNode {
                id: DESTINATION,
                kind: Kind::Destination,
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(1, CountMode::Explicit, Interpretation::Speakers),
                params: vec![],
            },
            0.0,
        );
        link.add_suspend(256).unwrap();
        assert_eq!(link.add_suspend(256), Err(SuspendError::Taken));
        start(g, 1, 300, link.clone()).unwrap();
        assert_eq!(wait_for(&link, &rx), [OfflineNotice::Suspended(256)]);
        assert_eq!(link.frame(), 256);
        assert_eq!(link.add_suspend(128), Err(SuspendError::Past));
        // Add a source while suspended: it plays from frame 256.
        link.send(Control::AddNode {
            id: 1,
            kind: Kind::ConstantSource(Schedule::default()),
            inputs: 0,
            outputs: 1,
            config: ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            params: vec![NewParam {
                id: 0,
                desc: ParamDesc::unbounded(1.0, Rate::A),
                value: 0.5,
                shared: SharedValue::default(),
            }],
        });
        link.send(Control::Start {
            node: 1,
            when: 0.0,
            offset: 0.0,
            duration: None,
        });
        link.send(Control::Connect {
            from: 1,
            output: 0,
            to: DESTINATION,
            input: 0,
        });
        link.resume();
        let n = wait_for(&link, &rx);
        let Some(OfflineNotice::Complete(out)) = n.last() else {
            panic!("{n:?}");
        };
        assert_eq!(out[0].len(), 300);
        assert_eq!(out[0][255], 0.0);
        assert_eq!(out[0][256], 0.5);
        assert_eq!(out[0][299], 0.5);
    }
}
