// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1 §1.2 AudioContext on the embedder's output device: the device's rate is the
//! context's default, the rendered periods reach the device while the context runs and stop when
//! it suspends or closes, a context not allowed to start opens no stream, and the tab's mute and
//! audible state apply below the page.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_audio::realtime::{
    AudioOutputs, AudioSink, DeviceInfo, NullSink, SinkError, StreamConfig,
};
use snowlark_page::media::{AutoplayPolicy, MediaConfig};
use snowlark_page::{Page, PageOptions};

const DEVICE: DeviceInfo = DeviceInfo {
    sample_rate: 44100.0,
    channels: 2,
    min_period: 0.01,
    max_period: 0.08,
    output_latency: 0.005,
};

/// What the device heard.
#[derive(Default)]
struct Heard {
    /// Each stream's configuration, as it started.
    starts: Vec<StreamConfig>,
    stops: usize,
    /// The peak of each period written, across its channels.
    peaks: Vec<f32>,
}

struct Device(Arc<Mutex<Heard>>);

struct Sink {
    clock: NullSink,
    heard: Arc<Mutex<Heard>>,
}

impl AudioSink for Sink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.heard.lock().unwrap().starts.push(*config);
        self.clock.start(config)
    }
    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        self.clock.wait_period()
    }
    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
        let peak = channels
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |m, s| m.max(s.abs()));
        self.heard.lock().unwrap().peaks.push(peak);
        self.clock.write(channels)
    }
    fn stop(&mut self) {
        self.heard.lock().unwrap().stops += 1;
        self.clock.stop()
    }
}

impl AudioOutputs for Device {
    fn device(&self) -> Option<DeviceInfo> {
        Some(DEVICE)
    }
    fn open(&self) -> Box<dyn AudioSink> {
        Box::new(Sink {
            clock: NullSink::default(),
            heard: self.0.clone(),
        })
    }
}

fn page(autoplay: AutoplayPolicy) -> (Page, Arc<Mutex<Heard>>, Arc<snowlark_media::AudioGroup>) {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let group = snowlark_media::AudioGroup::new();
    let mut page = Page::new(PageOptions::default());
    page.engine().set_media_config(MediaConfig {
        autoplay,
        silent: true,
        audio: Some(group.clone()),
        audio_out: Some(Arc::new(Device(heard.clone()))),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    (page, heard, group)
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs the page until `done` holds, for at most five seconds.
fn until(page: &mut Page, what: &str, done: impl Fn(&mut Page) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(page) {
        assert!(Instant::now() < deadline, "waited for {what}");
        page.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
}

const OSCILLATOR: &str = "window.c = new AudioContext(); \
    const o = c.createOscillator(); o.connect(c.destination); o.start(); 'made'";

#[test]
fn a_context_plays_on_the_embedders_output_at_its_rate() {
    let (mut page, heard, group) = page(AutoplayPolicy::Allowed);
    eval(&mut page, OSCILLATOR);
    // The device's rate is the context's; the latency is its period in whole render quanta
    // (0.01 s at 44.1 kHz is 441 frames, so 512).
    assert_eq!(eval(&mut page, "c.sampleRate"), "44100");
    assert_eq!(eval(&mut page, "c.destination.maxChannelCount"), "2");
    assert_eq!(
        eval(&mut page, "c.baseLatency"),
        (512.0 / 44100.0f64).to_string()
    );
    assert_eq!(eval(&mut page, "c.outputLatency"), "0.005");
    until(&mut page, "the context to run", |p| {
        eval(p, "c.state") == "running"
    });
    until(&mut page, "the oscillator to be heard", |_| {
        heard.lock().unwrap().peaks.iter().any(|&p| p > 0.5)
    });
    {
        let h = heard.lock().unwrap();
        assert_eq!(
            h.starts,
            vec![StreamConfig {
                sample_rate: 44100.0,
                channels: 2,
                period_frames: 512
            }]
        );
    }
    assert!(group.audible(), "the tab makes sound");
    // The tab's mute silences the output below the page: the context runs on and the tab still
    // counts as audible.
    group.set_muted(true);
    let from = heard.lock().unwrap().peaks.len() + 1;
    until(&mut page, "periods written muted", |_| {
        heard.lock().unwrap().peaks.len() > from + 4
    });
    assert!(heard.lock().unwrap().peaks[from..]
        .iter()
        .all(|&p| p == 0.0));
    assert_eq!(eval(&mut page, "c.state"), "running");
    assert!(group.audible());
    group.set_muted(false);
    // Suspending stops the stream and the tab's sound; closing after it opens nothing.
    eval(&mut page, "c.suspend().then(() => window.s = true); 0");
    until(&mut page, "the suspend", |p| {
        eval(p, "window.s === true") == "true"
    });
    assert_eq!(heard.lock().unwrap().stops, 1);
    assert!(!group.audible(), "a suspended context is silent");
    let written = heard.lock().unwrap().peaks.len();
    eval(&mut page, "c.resume().then(() => window.r = true); 0");
    until(&mut page, "the resume", |p| {
        eval(p, "window.r === true") == "true"
    });
    until(&mut page, "periods after the resume", |_| {
        heard.lock().unwrap().peaks.len() > written + 2
    });
    assert_eq!(heard.lock().unwrap().starts.len(), 2);
    eval(&mut page, "c.close().then(() => window.ended = true); 0");
    until(&mut page, "the close", |p| {
        eval(p, "window.ended === true") == "true"
    });
    assert_eq!(heard.lock().unwrap().stops, 2);
    assert!(!group.audible());
}

#[test]
fn a_context_not_allowed_to_start_opens_no_stream() {
    let (mut page, heard, group) = page(AutoplayPolicy::UserActivation);
    eval(&mut page, OSCILLATOR);
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        page.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(&mut page, "c.state"), "suspended");
    assert!(heard.lock().unwrap().starts.is_empty(), "no stream");
    assert!(!group.audible());
}

/// Without an output device (none attached), a context renders into a null sink at its usual
/// 48 kHz, as in headless runs.
#[test]
fn without_a_device_a_context_renders_silently() {
    struct NoDevice;
    impl AudioOutputs for NoDevice {
        fn device(&self) -> Option<DeviceInfo> {
            None
        }
        fn open(&self) -> Box<dyn AudioSink> {
            unreachable!("no device to open")
        }
    }
    let mut page = Page::new(PageOptions::default());
    page.engine().set_media_config(MediaConfig {
        autoplay: AutoplayPolicy::Allowed,
        silent: true,
        audio_out: Some(Arc::new(NoDevice)),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    eval(&mut page, OSCILLATOR);
    assert_eq!(eval(&mut page, "c.sampleRate"), "48000");
    until(&mut page, "the context to run", |p| {
        eval(p, "c.state") == "running"
    });
}
