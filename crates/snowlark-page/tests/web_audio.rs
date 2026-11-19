// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1: OfflineAudioContext rendering, AudioParam automation, AudioBuffer, the
//! source nodes and their events.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs `js`, which sets `out` when done, and returns `out`.
fn run(js: &str) -> String {
    let mut page = page();
    eval(&mut page, &format!("var out = 'pending'; {js}; 'ok'"));
    page.run_until_settled(Duration::from_secs(10));
    eval(&mut page, "String(out)")
}

#[test]
fn renders_a_ramped_constant_source_and_reports_completion() {
    // §1.6.3: a linear ramp from 0 at 0 s to 1 at 1000/8000 s, through a gain of 0.5.
    let r = run(r#"
        const ctx = new OfflineAudioContext(2, 1024, 8000);
        const log = [ctx.state, ctx.length, ctx.sampleRate, ctx.destination.channelCount];
        ctx.onstatechange = () => log.push('state:' + ctx.state);
        const src = new ConstantSourceNode(ctx, { offset: 1 });
        const gain = ctx.createGain();
        gain.gain.value = 0.5;
        src.offset.setValueAtTime(0, 0).linearRampToValueAtTime(1, 1000 / 8000);
        src.connect(gain).connect(ctx.destination);
        src.start();
        ctx.oncomplete = e => log.push('complete:' + (e.renderedBuffer === rendered) + ':' + e.isTrusted);
        let rendered;
        ctx.startRendering().then(b => {
          rendered = b;
          const d = b.getChannelData(0), r = b.getChannelData(1);
          let worst = 0;
          for (let i = 0; i < 1024; i++) {
            const want = 0.5 * Math.min(i / 1000, 1);
            worst = Math.max(worst, Math.abs(d[i] - want), Math.abs(r[i] - want));
          }
          log.push(b.numberOfChannels, b.length, b.sampleRate, worst < 1e-6, ctx.currentTime);
          setTimeout(() => { out = log.join(' '); }, 0);
        });
    "#);
    assert_eq!(
        r,
        "suspended 1024 8000 2 state:running 2 1024 8000 true 0.128 state:closed complete:true:true"
    );
}

#[test]
fn suspend_stops_at_the_quantum_and_changes_apply_after_resume() {
    let r = run(r#"
        const ctx = new OfflineAudioContext(1, 512, 4000);
        const src = ctx.createConstantSource();
        src.start();
        const log = [];
        ctx.suspend(0.05).then(() => {
          // 0.05 s is frame 200, rounded up to the quantum boundary at 256.
          log.push('suspended:' + ctx.currentTime + ':' + ctx.state);
          src.connect(ctx.destination);
          return ctx.resume();
        });
        let e1, e2;
        ctx.suspend(0.05).catch(e => e1 = e.name);
        try { ctx.suspend(1) } catch (e) {}
        ctx.suspend(1).catch(e => e2 = e.name);
        ctx.startRendering().then(b => {
          const d = b.getChannelData(0);
          log.push(d[255], d[256], d[511], e1, e2);
          out = log.join(' ');
        });
    "#);
    assert_eq!(
        r,
        "suspended:0.064:suspended 0 1 1 InvalidStateError InvalidStateError"
    );
}

#[test]
fn a_buffer_source_plays_its_buffer_and_fires_ended() {
    let r = run(r#"
        const ctx = new OfflineAudioContext({ length: 256, sampleRate: 4000 });
        const buf = ctx.createBuffer(1, 4, 4000);
        buf.copyToChannel(new Float32Array([1, 2, 3, 4]), 0);
        const src = new AudioBufferSourceNode(ctx, { buffer: buf, playbackRate: 0.5 });
        src.connect(ctx.destination);
        src.start(10 / 4000);
        const log = [];
        src.onended = () => log.push('ended');
        ctx.startRendering().then(b => {
          const d = Array.from(b.getChannelData(0).slice(8, 20));
          log.push(d.join(','));
          setTimeout(() => { out = log.join(' '); }, 0);
        });
    "#);
    // Half speed: each frame of the buffer, then the midpoint to the next (past the last frame, on
    // the line through the last two); then the playhead leaves the buffer and playback ends, which
    // the page hears before the rendering completes.
    assert_eq!(r, "ended 0,0,1,1.5,2,2.5,3,3.5,4,4.5,0,0");
}

#[test]
fn the_api_raises_the_spec_errors() {
    let r = run(r#"
        const name = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const ctx = new OfflineAudioContext(1, 128, 8000);
        const other = new OfflineAudioContext(1, 128, 8000);
        const g = ctx.createGain();
        const p = g.gain;
        p.setValueCurveAtTime([0, 1], 0.25, 0.25);
        out = [
          name(() => new OfflineAudioContext(0, 128, 8000)),
          name(() => new OfflineAudioContext(1, 0, 8000)),
          name(() => new OfflineAudioContext(1, 128, 100)),
          name(() => new AudioBuffer({ length: 1, sampleRate: 1e6 })),
          name(() => ctx.createBuffer(1, 1, 8000).getChannelData(1)),
          name(() => g.connect(other.destination)),
          name(() => g.connect(ctx.destination, 1)),
          name(() => g.disconnect(ctx.destination)),
          name(() => p.setValueAtTime(1, 0.3)),
          name(() => p.setValueAtTime(1, 0.5)),
          name(() => p.exponentialRampToValueAtTime(0, 1)),
          name(() => p.setValueAtTime(1, -1)),
          name(() => ctx.createBufferSource().playbackRate.automationRate = 'a-rate'),
          name(() => ctx.destination.channelCount = 2),
          name(() => ctx.createConstantSource().stop()),
          g.connect(ctx.destination) === ctx.destination,
          p.defaultValue, p.minValue === -3.4028234663852886e38, p.automationRate,
        ].join(' ');
    "#);
    assert_eq!(
        r,
        "NotSupportedError NotSupportedError NotSupportedError NotSupportedError IndexSizeError \
         InvalidAccessError IndexSizeError InvalidAccessError NotSupportedError none RangeError \
         RangeError InvalidStateError InvalidStateError InvalidStateError true 1 true a-rate"
    );
}

#[test]
fn decoding_fails_for_a_context_whose_document_went_away() {
    // §1.1.3 decodeAudioData: InvalidStateError when the document is not fully active; otherwise
    // the data is detached, and with no decoder for it, decoding fails with an EncodingError.
    let r = run(r#"
        const iframe = document.createElement('iframe');
        document.body.appendChild(iframe);
        const gone = new iframe.contentWindow.OfflineAudioContext(1, 1, 8000);
        iframe.remove();
        const ctx = new OfflineAudioContext(1, 1, 8000);
        const data = new ArrayBuffer(8);
        const log = [];
        gone.decodeAudioData(new ArrayBuffer(1)).catch(e => log.push(e.name));
        ctx.decodeAudioData(data, null, e => log.push('cb:' + e.name))
          .catch(e => { log.push(e.name, data.byteLength); out = log.join(' '); });
    "#);
    assert_eq!(r, "InvalidStateError cb:EncodingError EncodingError 0");
}

#[test]
fn an_oscillator_renders_its_wave_and_a_compressor_reduces_it() {
    // Â§1.26: a 441 Hz sine at 44.1 kHz is sin(2Ï€Â·441Â·n/44100); Â§1.27: a custom wave of one
    // cosine partial, normalized, is cos. The fingerprinting graph (a triangle through a
    // compressor) must report a reduction and stay finite.
    let r = run(r#"
        const ctx = new OfflineAudioContext(1, 1024, 44100);
        const sine = new OscillatorNode(ctx, { frequency: 441 });
        const cos = ctx.createOscillator();
        cos.setPeriodicWave(new PeriodicWave(ctx, { real: [0, 3], imag: [0, 0] }));
        const g = new GainNode(ctx, { gain: 0 });
        sine.connect(ctx.destination);
        cos.connect(g).connect(ctx.destination);
        sine.start();
        cos.start();
        const log = [sine.type, cos.type, sine.frequency.maxValue];
        const name = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        log.push(name(() => sine.type = 'custom'),
                 name(() => new OscillatorNode(ctx, { type: 'custom' })),
                 name(() => ctx.createPeriodicWave([0, 1], [0])),
                 name(() => ctx.createDynamicsCompressor().channelCount = 3),
                 name(() => ctx.createDynamicsCompressor().channelCountMode = 'max'));
        ctx.startRendering().then(b => {
          const d = b.getChannelData(0);
          let worst = 0;
          for (let i = 0; i < 1024; i++)
            worst = Math.max(worst, Math.abs(d[i] - Math.sin(2 * Math.PI * 441 * i / 44100)));
          log.push(worst < 1e-4);
          const fp = new OfflineAudioContext(1, 5000, 44100);
          const tri = fp.createOscillator();
          tri.type = 'triangle';
          tri.frequency.setValueAtTime(10000, fp.currentTime);
          const comp = fp.createDynamicsCompressor();
          tri.connect(comp).connect(fp.destination);
          tri.start(0);
          return fp.startRendering().then(fb => {
            const v = fb.getChannelData(0);
            let sum = 0;
            for (let i = 4500; i < 5000; i++) sum += Math.abs(v[i]);
            log.push(comp.reduction < -1, Number.isFinite(sum) && sum > 1);
            out = log.join(' ');
          });
        });
    "#);
    assert_eq!(
        r,
        "sine custom 22050 InvalidStateError InvalidStateError IndexSizeError NotSupportedError \
         NotSupportedError true true true"
    );
}

#[test]
fn the_processing_nodes_render_and_check_their_arguments() {
    // §1.17: a delay in a cycle reads a quantum back instead of muting the cycle; §1.31: a pan of
    // 1 puts a mono input on the right; §1.33: a curve maps -1..1 onto itself here; §1.24: a
    // one-pole IIR halves an impulse each frame; §1.16: an unnormalized unit response passes the
    // input; §1.13.3: a lowpass passes DC, and frequencies past Nyquist are NaN.
    let r = run(r#"
        const log = [];
        const name = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const ctx = new OfflineAudioContext(1, 512, 8192);
        const src = new ConstantSourceNode(ctx);
        const delay = ctx.createDelay(1);
        delay.delayTime.value = 128 / 8192;
        const fb = new GainNode(ctx, { gain: 0.5 });
        src.connect(delay).connect(fb).connect(delay);
        delay.connect(ctx.destination);
        src.start(0);
        src.stop(128 / 8192);
        const st = new OfflineAudioContext(2, 128, 8192);
        const one = new ConstantSourceNode(st);
        one.connect(new StereoPannerNode(st, { pan: 1 })).connect(st.destination);
        one.start();
        const fx = new OfflineAudioContext(3, 128, 8192);
        const imp = fx.createBuffer(1, 128, 8192);
        imp.getChannelData(0)[0] = 1;
        const merger = fx.createChannelMerger(3);
        const ws = new WaveShaperNode(fx, { curve: [-1, 0, 1] });
        const iir = fx.createIIRFilter([1], [2, -1]);
        const unit = fx.createBuffer(1, 1, 8192);
        unit.getChannelData(0)[0] = 1;
        const conv = new ConvolverNode(fx, { buffer: unit, disableNormalization: true });
        for (const [i, n] of [ws, iir, conv].entries()) {
          const s = new AudioBufferSourceNode(fx, { buffer: imp });
          s.connect(n).connect(merger, 0, i);
          s.start();
        }
        merger.connect(fx.destination);
        const lp = ctx.createBiquadFilter();
        const f = new Float32Array([0, 5000]), m = new Float32Array(2), p = new Float32Array(2);
        lp.getFrequencyResponse(f, m, p);
        log.push(m[0].toFixed(3), Number.isNaN(m[1]), ctx.createAnalyser().frequencyBinCount,
                 ctx.listener.forwardZ.value, ctx.createPanner().panningModel);
        log.push(name(() => ctx.createDelay(200)),
                 name(() => ctx.createChannelSplitter(0)),
                 name(() => ctx.createChannelSplitter(2).channelCount = 3),
                 name(() => ctx.createIIRFilter([0], [1])),
                 name(() => ctx.createAnalyser().fftSize = 100),
                 name(() => ctx.createWaveShaper().curve = new Float32Array(1)),
                 name(() => ctx.createPanner().coneOuterGain = 2),
                 name(() => ctx.createConvolver().buffer = ctx.createBuffer(3, 1, 8192)),
                 name(() => lp.getFrequencyResponse(f, new Float32Array(1), p)));
        Promise.all([ctx.startRendering(), st.startRendering(), fx.startRendering()])
          .then(([a, b, c]) => {
            const d = a.getChannelData(0);
            log.push(d[5], d[133], d[261], d[389]);
            log.push(b.getChannelData(0)[9].toFixed(3), b.getChannelData(1)[9].toFixed(3));
            log.push(...[0, 1, 2].map(i => Array.from(c.getChannelData(i).slice(0, 3)).join(',')));
            out = log.join(' ');
          });
    "#);
    assert_eq!(
        r,
        "1.000 true 1024 -1 equalpower NotSupportedError IndexSizeError InvalidStateError \
         InvalidStateError IndexSizeError InvalidStateError InvalidStateError NotSupportedError \
         InvalidAccessError 0 1 0.5 0.25 0.000 1.000 1,0,0 0.5,0.25,0.125 1,0,0"
    );
}

#[test]
fn decode_audio_data_reads_a_wave_file_and_resamples_it() {
    // §1.1.3: the decoded buffer is at the context's rate; the success callback gets it too.
    let r = run(r#"
        function wav(rate, samples) {
          const n = samples.length, b = new ArrayBuffer(44 + 2 * n), v = new DataView(b);
          const tag = (o, t) => { for (let i = 0; i < 4; i++) v.setUint8(o + i, t.charCodeAt(i)); };
          tag(0, 'RIFF'); v.setUint32(4, 36 + 2 * n, true); tag(8, 'WAVE'); tag(12, 'fmt ');
          v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
          v.setUint32(24, rate, true); v.setUint32(28, rate * 2, true); v.setUint16(32, 2, true);
          v.setUint16(34, 16, true); tag(36, 'data'); v.setUint32(40, 2 * n, true);
          samples.forEach((x, i) => v.setInt16(44 + 2 * i, x * 16384, true));
          return b;
        }
        const log = [];
        const same = new OfflineAudioContext(1, 1, 8000);
        const up = new OfflineAudioContext(1, 1, 16000);
        const tone = Array.from({ length: 800 }, (_, i) => Math.sin(2 * Math.PI * 100 * i / 8000));
        same.decodeAudioData(wav(8000, [0.5, -0.5, 1.5]), b => log.push('cb'))
          .then(b => {
            log.push(b.sampleRate, b.length, Array.from(b.getChannelData(0)).join(','));
            return up.decodeAudioData(wav(8000, tone));
          })
          .then(b => {
            const d = b.getChannelData(0);
            log.push(b.sampleRate, b.length, Math.abs(d[800] - 0.5 * Math.sin(2 * Math.PI * 100 * 800 / 16000)) < 1e-3);
            out = log.join(' ');
          }, e => { out = 'error ' + e.name; });
    "#);
    assert_eq!(r, "cb 8000 3 0.25,-0.25,0.75 16000 1600 true");
}
