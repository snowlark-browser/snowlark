// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An AudioContext's rendering (§1.2 "The AudioContext Interface"): a rendering thread renders the
//! graph period by period into an output [`AudioSink`] at the rate the sink consumes it, and
//! answers the control thread's `resume()`, `suspend()` and `close()` messages in the order they
//! were queued, between graph changes.
//!
//! The sink is message-shaped: everything that crosses it is plain data (a stream configuration,
//! planar sample buffers, the time a period will play), so a device in another process can sit
//! behind it. [`NullSink`] is the output when there is no device: it takes each period when its
//! clock says the period is due and drops the samples.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::graph::{Control, Graph, Notice};
use crate::RENDER_QUANTUM as N;

/// §1.2.4 `AudioContextOptions.latencyHint`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LatencyHint {
    /// The lowest latency the output allows without glitching.
    Interactive,
    /// A balance of latency and power use.
    Balanced,
    /// Sustained playback first: the highest latency the output offers.
    Playback,
    /// A latency in seconds, which the output's range bounds.
    Seconds(f64),
}

/// "balanced": about two device periods of a typical shared-mode output.
const BALANCED_SECONDS: f64 = 0.02;

/// What an output offers, known before a context is made (its sample rate is the context's
/// default).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceInfo {
    /// The rate the output mixes at.
    pub sample_rate: f32,
    /// Its channels (the destination's `maxChannelCount`).
    pub channels: u32,
    /// The shortest and longest period it takes, in seconds.
    pub min_period: f64,
    pub max_period: f64,
    /// Seconds from a period handed over to its first frame leaving the output.
    pub output_latency: f64,
}

impl DeviceInfo {
    /// The frames per period for `hint` at `sample_rate`: whole render quanta, at least one, within
    /// the output's range.
    pub fn period_frames(&self, hint: LatencyHint, sample_rate: f32) -> usize {
        let (min, max) = (self.min_period, self.max_period.max(self.min_period));
        let seconds = match hint {
            LatencyHint::Interactive => min,
            LatencyHint::Balanced => BALANCED_SECONDS.clamp(min, max),
            LatencyHint::Playback => max,
            LatencyHint::Seconds(s) if s.is_nan() => min,
            LatencyHint::Seconds(s) => s.clamp(min, max),
        };
        let frames = (seconds * sample_rate as f64).round().max(1.0) as usize;
        frames.div_ceil(N) * N
    }
}

/// How a stream is opened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamConfig {
    /// The context's sample rate (an output at another rate resamples).
    pub sample_rate: f32,
    /// The channels each period carries.
    pub channels: u32,
    /// The frames each period carries (whole render quanta).
    pub period_frames: usize,
}

/// The output failed: no device, or it went away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkError(pub String);

/// Where an AudioContext's rendered audio goes. The rendering thread calls it: `start` before the
/// first period and after a `stop`, then per period `wait_period` and `write`.
pub trait AudioSink: Send {
    /// Acquires the output and starts its stream.
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError>;
    /// Blocks until the output wants the next period. Returns when that period's first frame will
    /// leave the output.
    fn wait_period(&mut self) -> Result<Instant, SinkError>;
    /// Hands over the period: `config.channels` planar channels of `config.period_frames` frames.
    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError>;
    /// Stops the stream and releases the output; `start` may follow.
    fn stop(&mut self);
}

/// Where an AudioContext finds its output: the default output device of the embedder (in the
/// browser's process, or reached from a renderer through messages). Message-shaped, like
/// [`AudioSink`]: what it answers is plain data.
pub trait AudioOutputs: Send + Sync {
    /// What the default output offers; none when there is no output device.
    fn device(&self) -> Option<DeviceInfo>;
    /// A sink on the default output; its stream opens at [`AudioSink::start`].
    fn open(&self) -> Box<dyn AudioSink>;
}

/// The output when there is no device: it consumes periods at the clock rate and drops them.
#[derive(Default)]
pub struct NullSink {
    /// The stream's start and the periods consumed since, for deadlines that do not drift.
    clock: Option<(Instant, u64)>,
    /// Seconds per period.
    period: f64,
}

impl NullSink {
    /// What a null sink offers: 48 kHz stereo (no device to take a mix rate from), periods of one
    /// render quantum up to 80 ms, and nothing between a period handed over and its output.
    pub const DEVICE: DeviceInfo = DeviceInfo {
        sample_rate: 48000.0,
        channels: 2,
        min_period: 0.0,
        max_period: 0.08,
        output_latency: 0.0,
    };
}

impl AudioSink for NullSink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.period = config.period_frames as f64 / config.sample_rate as f64;
        self.clock = Some((Instant::now(), 0));
        Ok(())
    }

    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        let (start, periods) = self
            .clock
            .ok_or_else(|| SinkError("The stream is not started".into()))?;
        let due = start + Duration::from_secs_f64(self.period * periods as f64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        Ok(due)
    }

    fn write(&mut self, _channels: &[Vec<f32>]) -> Result<(), SinkError> {
        if let Some((_, periods)) = &mut self.clock {
            *periods += 1;
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.clock = None;
    }
}

/// What the control thread asks of the rendering thread, in order with the graph's messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start processing (the constructor) or resume: acquire the output, render.
    Start,
    /// Suspend: release the output, stop rendering. The tag names the `suspend()` call.
    Suspend(u64),
    /// Close: release the output and end the rendering thread.
    Close,
}

/// What the rendering thread tells the control thread.
#[derive(Debug, PartialEq)]
pub enum RealtimeNotice {
    Graph(Notice),
    /// A `Start` was carried out: the rendering thread state is running.
    Running,
    /// A `Suspend` was carried out.
    Suspended(u64),
    /// The `Close` was carried out; nothing follows.
    Closed,
    /// The output could not start, or failed while running: the rendering thread state is
    /// suspended.
    Failed(SinkError),
}

enum Msg {
    Graph(Control),
    Command(Command),
}

#[derive(Default)]
struct State {
    msgs: Vec<Msg>,
    /// Commands queued and not answered yet.
    unanswered: usize,
    /// Sample-frames rendered.
    frame: u64,
    /// The first frame of the last period handed to the output, and when it leaves the output.
    position: Option<(u64, Instant)>,
    notices: Vec<RealtimeNotice>,
    /// The control thread is gone: end without answering.
    stop: bool,
    /// Closed: messages are dropped.
    closed: bool,
}

/// The control thread's end of an AudioContext's rendering: its control message queue and the
/// notices coming back.
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

    fn push(&self, msg: Msg) {
        let mut s = self.lock();
        if s.closed {
            return;
        }
        if let Msg::Command(c) = msg {
            s.unanswered += 1;
            if c == Command::Close {
                s.closed = true;
            }
        }
        s.msgs.push(msg);
        drop(s);
        self.cv.notify_all();
    }

    /// Queues a graph change.
    pub fn send(&self, msg: Control) {
        self.push(Msg::Graph(msg));
    }

    /// Queues a state change; the rendering thread answers each with a notice.
    pub fn command(&self, c: Command) {
        self.push(Msg::Command(c));
    }

    /// The sample-frames rendered (whole render quanta).
    pub fn frame(&self) -> u64 {
        self.lock().frame
    }

    /// The first frame of the last period handed to the output and when it leaves the output;
    /// none before the first.
    pub fn position(&self) -> Option<(u64, Instant)> {
        self.lock().position
    }

    /// Takes the notices queued so far.
    pub fn take_notices(&self) -> Vec<RealtimeNotice> {
        std::mem::take(&mut self.lock().notices)
    }

    /// Whether the rendering will report without the control thread doing anything: a command
    /// waits for its answer, or notices wait to be taken.
    pub fn busy(&self) -> bool {
        let s = self.lock();
        s.unanswered > 0 || !s.notices.is_empty()
    }

    /// Ends the rendering without answering (the context's document went away).
    pub fn stop(&self) {
        let mut s = self.lock();
        s.stop = true;
        s.closed = true;
        drop(s);
        self.cv.notify_all();
    }

    fn notify(&self, notices: impl IntoIterator<Item = RealtimeNotice>, answered: usize) {
        let mut s = self.lock();
        let before = s.notices.len();
        s.notices.extend(notices);
        s.unanswered = s.unanswered.saturating_sub(answered);
        let any = s.notices.len() > before;
        drop(s);
        if any {
            (self.wake)();
        }
    }
}

/// Starts the rendering thread of a context: it waits for a [`Command::Start`] on `link`, then
/// renders `graph` into `sink`.
pub fn start(
    graph: Graph,
    sink: Box<dyn AudioSink>,
    config: StreamConfig,
    link: Arc<Link>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("audio rendering".into())
        .spawn(move || Renderer::new(graph, sink, config).run(&link))
        .map(|_| ())
}

struct Renderer {
    graph: Graph,
    sink: Box<dyn AudioSink>,
    config: StreamConfig,
    running: bool,
    frame: u64,
    buffer: Vec<Vec<f32>>,
}

impl Renderer {
    fn new(graph: Graph, sink: Box<dyn AudioSink>, config: StreamConfig) -> Renderer {
        let buffer = vec![vec![0.0; config.period_frames]; config.channels as usize];
        Renderer {
            graph,
            sink,
            config,
            running: false,
            frame: 0,
            buffer,
        }
    }

    fn run(mut self, link: &Link) {
        loop {
            let msgs = {
                let mut s = link.lock();
                while !self.running && s.msgs.is_empty() && !s.stop {
                    s = link.cv.wait(s).unwrap_or_else(|e| e.into_inner());
                }
                if s.stop {
                    break;
                }
                std::mem::take(&mut s.msgs)
            };
            if !self.apply(msgs, link) {
                return;
            }
            if self.running {
                if let Err(e) = self.period(link) {
                    self.sink.stop();
                    self.running = false;
                    link.notify([RealtimeNotice::Failed(e)], 0);
                }
            }
        }
        if self.running {
            self.sink.stop();
        }
    }

    /// Applies the queued messages in order. Returns false once closed.
    fn apply(&mut self, msgs: Vec<Msg>, link: &Link) -> bool {
        let now = self.frame as f64 / self.graph.sample_rate();
        for m in msgs {
            let notice = match m {
                Msg::Graph(c) => {
                    self.graph.apply(c, now);
                    continue;
                }
                Msg::Command(Command::Start) => {
                    if self.running {
                        RealtimeNotice::Running
                    } else {
                        match self.sink.start(&self.config) {
                            Ok(()) => {
                                self.running = true;
                                RealtimeNotice::Running
                            }
                            Err(e) => RealtimeNotice::Failed(e),
                        }
                    }
                }
                Msg::Command(Command::Suspend(tag)) => {
                    if self.running {
                        self.sink.stop();
                        self.running = false;
                    }
                    RealtimeNotice::Suspended(tag)
                }
                Msg::Command(Command::Close) => {
                    if self.running {
                        self.sink.stop();
                        self.running = false;
                    }
                    link.notify([RealtimeNotice::Closed], 1);
                    return false;
                }
            };
            link.notify([notice], 1);
        }
        true
    }

    /// Waits for the output to want a period, renders it and hands it over.
    fn period(&mut self, link: &Link) -> Result<(), SinkError> {
        let at = self.sink.wait_period()?;
        let first = self.frame;
        for q in 0..self.config.period_frames / N {
            self.graph.render(self.frame);
            let out = self.graph.output();
            for (c, dst) in self.buffer.iter_mut().enumerate() {
                let dst = &mut dst[q * N..(q + 1) * N];
                if c < out.channels() {
                    dst.copy_from_slice(&out.channel(c)[..N]);
                } else {
                    dst.fill(0.0);
                }
            }
            self.frame += N as u64;
        }
        self.sink.write(&self.buffer)?;
        {
            let mut s = link.lock();
            s.frame = self.frame;
            s.position = Some((first, at));
        }
        let notices = self.graph.take_notices();
        if !notices.is_empty() {
            link.notify(notices.into_iter().map(RealtimeNotice::Graph), 0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ChannelConfig, CountMode, Interpretation};
    use crate::graph::{NewParam, SharedValue, DESTINATION};
    use crate::node::{Kind, Schedule};
    use crate::param::{ParamDesc, Rate};
    use std::sync::mpsc;

    fn link() -> (Arc<Link>, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let link = Link::new(Box::new(move || {
            let _ = tx.lock().unwrap().send(());
        }));
        (link, rx)
    }

    fn wait_for(link: &Link, rx: &mpsc::Receiver<()>) -> Vec<RealtimeNotice> {
        loop {
            let n = link.take_notices();
            if !n.is_empty() {
                return n;
            }
            rx.recv_timeout(Duration::from_secs(10)).expect("a notice");
        }
    }

    fn graph(rate: f32, channels: u32) -> Graph {
        let mut g = Graph::new(rate);
        g.apply(
            Control::AddNode {
                id: DESTINATION,
                kind: Kind::Destination,
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(channels, CountMode::Explicit, Interpretation::Speakers),
                params: vec![],
            },
            0.0,
        );
        g
    }

    fn config(rate: f32, period_frames: usize) -> StreamConfig {
        StreamConfig {
            sample_rate: rate,
            channels: 2,
            period_frames,
        }
    }

    #[test]
    fn latency_hints_pick_whole_quanta_within_the_range() {
        let d = DeviceInfo {
            sample_rate: 48000.0,
            channels: 2,
            min_period: 0.01,
            max_period: 0.08,
            output_latency: 0.0,
        };
        let i = d.period_frames(LatencyHint::Interactive, 48000.0);
        let b = d.period_frames(LatencyHint::Balanced, 48000.0);
        let p = d.period_frames(LatencyHint::Playback, 48000.0);
        assert_eq!((i, b, p), (512, 1024, 3840));
        assert_eq!(d.period_frames(LatencyHint::Seconds(0.0), 48000.0), i);
        assert_eq!(d.period_frames(LatencyHint::Seconds(-1.0), 48000.0), i);
        assert_eq!(d.period_frames(LatencyHint::Seconds(10.0), 48000.0), p);
        assert_eq!(d.period_frames(LatencyHint::Seconds(0.04), 48000.0), 1920);
        // The null sink's shortest period is one render quantum.
        let n = NullSink::DEVICE;
        assert_eq!(n.period_frames(LatencyHint::Interactive, 44100.0), N);
    }

    #[test]
    fn renders_at_the_clock_rate_and_only_while_running() {
        let (link, rx) = link();
        let rate = 8000.0;
        start(
            graph(rate, 2),
            Box::new(NullSink::default()),
            config(rate, N),
            link.clone(),
        )
        .unwrap();
        // Nothing renders before a Start.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(link.frame(), 0);
        assert_eq!(link.position(), None);
        let t0 = Instant::now();
        link.command(Command::Start);
        assert!(link.busy());
        assert_eq!(wait_for(&link, &rx), [RealtimeNotice::Running]);
        std::thread::sleep(Duration::from_millis(400));
        let frames = link.frame();
        let elapsed = t0.elapsed().as_secs_f64();
        // At most one period ahead of the clock, and not far behind it.
        assert!(
            frames as f64 <= elapsed * rate as f64 + 2.0 * N as f64,
            "{frames} frames in {elapsed} s"
        );
        assert!(
            frames as f64 >= 0.6 * 0.4 * rate as f64,
            "{frames} frames in {elapsed} s"
        );
        let (first, at) = link.position().unwrap();
        assert_eq!(first + N as u64, frames);
        assert!(at >= t0);
        // Suspended: the frame stops.
        link.command(Command::Suspend(7));
        assert_eq!(wait_for(&link, &rx), [RealtimeNotice::Suspended(7)]);
        let held = link.frame();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(link.frame(), held);
        // Resumed: it goes on from there.
        link.command(Command::Start);
        assert_eq!(wait_for(&link, &rx), [RealtimeNotice::Running]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(link.frame() > held);
        link.command(Command::Close);
        assert_eq!(wait_for(&link, &rx), [RealtimeNotice::Closed]);
        assert!(!link.busy());
        // Closed: nothing more is queued or rendered.
        let end = link.frame();
        link.command(Command::Start);
        assert!(!link.busy());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(link.frame(), end);
        assert!(link.take_notices().is_empty());
    }

    /// A sink that takes `periods` periods at once, then fails.
    struct Recorder {
        periods: usize,
        written: Arc<Mutex<Vec<Vec<Vec<f32>>>>>,
    }

    impl AudioSink for Recorder {
        fn start(&mut self, _: &StreamConfig) -> Result<(), SinkError> {
            Ok(())
        }
        fn wait_period(&mut self) -> Result<Instant, SinkError> {
            if self.written.lock().unwrap().len() == self.periods {
                return Err(SinkError("unplugged".into()));
            }
            Ok(Instant::now())
        }
        fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
            self.written.lock().unwrap().push(channels.to_vec());
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn hands_the_rendered_graph_to_the_sink_and_reports_its_failure() {
        let (link, rx) = link();
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = Recorder {
            periods: 3,
            written: written.clone(),
        };
        // A constant source of 0.5 into the mono destination.
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
        start(
            graph(1000.0, 1),
            Box::new(sink),
            config(1000.0, 2 * N),
            link.clone(),
        )
        .unwrap();
        link.command(Command::Start);
        let mut notices = wait_for(&link, &rx);
        while notices.len() < 2 {
            notices.extend(wait_for(&link, &rx));
        }
        assert_eq!(notices[0], RealtimeNotice::Running);
        assert_eq!(
            notices[1],
            RealtimeNotice::Failed(SinkError("unplugged".into()))
        );
        let w = written.lock().unwrap();
        assert_eq!(w.len(), 3);
        // The destination has one channel: the sink's second is silent.
        assert!(w
            .iter()
            .all(|p| p[0].len() == 2 * N && p[0].iter().all(|&s| s == 0.5)));
        assert!(w.iter().all(|p| p[1].iter().all(|&s| s == 0.0)));
        assert_eq!(link.frame(), 6 * N as u64);
        link.stop();
    }
}
