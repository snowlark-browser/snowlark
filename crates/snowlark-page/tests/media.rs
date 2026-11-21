// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media elements (HTML §4.8.11) end to end: the fixture clip (snowlark-media's test pattern, 4 s of
//! 320×180 H.264 with an AAC tone) played by Media Foundation, as scripts see it.

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::media::{AutoplayPolicy, MediaConfig};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn fixtures() -> url::Url {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../snowlark-media/tests/fixtures/")
        .canonicalize()
        .unwrap();
    url::Url::from_directory_path(dir).unwrap()
}

fn page(net: &NetThread, html: &str, autoplay: AutoplayPolicy) -> Page {
    page_in_tab(net, html, autoplay, None)
}

/// A page whose media plays in a browser tab's audio group (with sound, unless the group is
/// muted).
fn page_in_tab(
    net: &NetThread,
    html: &str,
    autoplay: AutoplayPolicy,
    audio: Option<std::sync::Arc<snowlark_media::AudioGroup>>,
) -> Page {
    let silent = audio.is_none();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.engine().set_media_config(MediaConfig {
        videos: snowlark_paint::Gpu::new()
            .ok()
            .map(|g| snowlark_paint::video::VideoStore::new(Some(g))),
        autoplay,
        copy_frames: true,
        silent,
        audio,
        audio_out: None,
        cdm: None,
    });
    page.load_html(html, &fixtures().join("test.html").unwrap());
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Turns the event loop until `js` is "true" (or the time runs out).
fn wait_for(page: &mut Page, js: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        page.turn();
        if eval(page, js) == "true" {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

const LOG: &str = "<script>
window.log = [];
const v = document.querySelector('video,audio');
for (const t of ['loadstart','durationchange','loadedmetadata','loadeddata','canplay','canplaythrough',
  'play','playing','pause','seeking','seeked','ended','error','emptied','abort','volumechange',
  'ratechange','resize','waiting','suspend'])
  v.addEventListener(t, () => log.push(t));
</script>";

#[test]
fn loads_plays_seeks_and_ends() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!("<video src=pattern.mp4></video>{LOG}"),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('canplaythrough')", 20),
        "{}",
        eval(&mut page, "log.join()")
    );
    let r = eval(
        &mut page,
        "[log.filter(e => e != 'suspend').slice(0, 6).join(), v.networkState > 0, v.readyState, v.videoWidth, v.videoHeight,
          Math.round(v.duration), v.paused, v.currentSrc.endsWith('pattern.mp4'),
          v.buffered.length >= 0, v.seekable.length, v.videoTracks.length, v.audioTracks.length].join(' ')",
    );
    assert_eq!(
        r,
        "loadstart,durationchange,resize,loadedmetadata,loadeddata,canplay true 4 320 180 4 true true true 1 1 1"
    );
    // play(): its promise resolves after `play` and `playing`; time moves.
    eval(
        &mut page,
        "window.played = false; v.play().then(() => played = true);",
    );
    assert!(wait_for(&mut page, "played && v.currentTime > 0.3", 10));
    assert!(
        eval(
            &mut page,
            "log.includes('play') && log.includes('playing') && !v.paused"
        ) == "true"
    );
    // pause(): `pause`, and the time holds.
    eval(&mut page, "v.pause(); window.held = v.currentTime;");
    assert!(wait_for(&mut page, "log.includes('pause')", 5));
    std::thread::sleep(Duration::from_millis(200));
    page.turn();
    assert_eq!(
        eval(&mut page, "v.paused && v.currentTime === held"),
        "true"
    );
    // Seeking: `seeking`, then `seeked` at the new time.
    eval(&mut page, "log = []; v.currentTime = 3.7;");
    assert_eq!(eval(&mut page, "v.seeking"), "true");
    assert!(
        wait_for(&mut page, "log.includes('seeked')", 10),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(
            &mut page,
            "[log.join(), v.seeking, v.currentTime].join(' ')"
        ),
        "seeking,seeked false 3.7"
    );
    // To the end: `pause` and `ended`, `ended` is true.
    eval(&mut page, "log = []; v.play();");
    assert!(
        wait_for(&mut page, "log.includes('ended')", 10),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(&mut page, "[v.ended, v.paused, v.played.length > 0].join()"),
        "true,true,true"
    );
}

#[test]
fn source_children_and_failures() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    // The first source's type can't play, the second is missing, the third plays.
    let mut page = page(
        &net,
        &format!(
            "<video><source src=a.ogv type='video/x-unknown'><source src=missing.mp4 type=video/mp4>\
             <source src=pattern.mp4 type=video/mp4></video>{LOG}\
             <script>document.querySelectorAll('source').forEach((s, i) =>
               s.addEventListener('error', () => log.push('source error ' + i)));</script>"
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('loadedmetadata')", 20),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(&mut page, "[log.filter(e => e.startsWith('source')).join(), v.currentSrc.split('/').pop()].join(' ')"),
        "source error 0,source error 1 pattern.mp4"
    );
    // A src that is no media: `error` with MEDIA_ERR_SRC_NOT_SUPPORTED, and play() rejects.
    eval(
        &mut page,
        "window.wlog = []; const w = document.createElement('video');
         w.addEventListener('error', () => wlog.push('error ' + w.error.code + ' ' + w.networkState));
         w.src = 'nothing-here.mp4'; document.body.append(w);",
    );
    assert!(wait_for(&mut page, "wlog.length > 0", 10));
    assert_eq!(eval(&mut page, "wlog.join()"), "error 4 3");
    eval(
        &mut page,
        "window.why = ''; w.play().catch(e => why = e.name);",
    );
    assert!(wait_for(&mut page, "why !== ''", 5));
    assert_eq!(eval(&mut page, "why"), "NotSupportedError");
    // canPlayType from the installed codecs.
    assert_eq!(
        eval(
            &mut page,
            "[v.canPlayType('video/mp4'), v.canPlayType('video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\"'),
              v.canPlayType('application/octet-stream'), v.canPlayType('video/x-nope')].join()"
        ),
        "maybe,probably,,"
    );
}

#[test]
fn autoplay_needs_muting_or_activation() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!("<video src=pattern.mp4 autoplay></video>{LOG}"),
        AutoplayPolicy::UserActivation,
    );
    assert!(wait_for(&mut page, "log.includes('canplaythrough')", 20));
    // Audible autoplay waits for the user; play() is not allowed.
    assert_eq!(eval(&mut page, "v.paused"), "true");
    eval(
        &mut page,
        "window.why = ''; v.play().catch(e => why = e.name);",
    );
    assert!(wait_for(&mut page, "why !== ''", 5));
    assert_eq!(eval(&mut page, "why"), "NotAllowedError");
    // Muted, it plays.
    eval(
        &mut page,
        "v.muted = true; window.ok = false; v.play().then(() => ok = true);",
    );
    assert!(wait_for(&mut page, "ok", 10));
    assert!(eval(&mut page, "log.includes('volumechange') && !v.paused") == "true");
    // Volume out of range throws.
    assert_eq!(
        eval(&mut page, "try { v.volume = 2; 'no' } catch (e) { e.name }"),
        "IndexSizeError"
    );
}

#[test]
fn preload_none_waits_for_play() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!("<video src=pattern.mp4 preload=none></video>{LOG}"),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('suspend')", 10),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(&mut page, "[v.readyState, v.networkState].join()"),
        "0,1"
    );
    eval(&mut page, "v.play()");
    assert!(
        wait_for(&mut page, "log.includes('playing')", 20),
        "{}",
        eval(&mut page, "log.join()")
    );
}

#[test]
fn text_tracks_and_cues() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<video></video>", AutoplayPolicy::Allowed);
    let r = eval(
        &mut page,
        "const v = document.querySelector('video');
         const t = v.addTextTrack('captions', 'English', 'en');
         t.addCue(new VTTCue(1, 2, 'one'));
         t.addCue(new VTTCue(0, 1, 'zero'));
         const c = new VTTCue(5, 6, 'x'); c.id = 'c';
         t.addCue(c);
         [v.textTracks.length, t.kind, t.mode, t.cues.length, t.cues[0].text,
          t.cues.getCueById('c').startTime, c.track === t, c.align, c.line].join()",
    );
    assert_eq!(r, "1,captions,hidden,3,zero,5,true,center,auto");
    let r = eval(
        &mut page,
        "t.removeCue(c); t.mode = 'disabled';
         [t.cues, (() => { try { t.removeCue(c); } catch (e) { return e.name } })()].join()",
    );
    assert_eq!(r, ",NotFoundError");
}

/// HTML's track processing model: a `<track>` whose mode is set loads after a stable state (so a
/// `src` set in the same script is the one fetched), its cues are parsed from WebVTT and kept in
/// order through time changes, `getCueAsHTML()` builds the cue text's elements, and a new `src`
/// empties the list of cues at once and loads again.
#[test]
fn track_elements_load_webvtt() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<video></video>", AutoplayPolicy::Allowed);
    let r = eval(
        &mut page,
        r#"const v = document.querySelector('video');
         const tr = document.createElement('track');
         tr.label = 'L'; tr.srclang = 'fr';
         v.appendChild(tr);
         tr.track.mode = 'hidden';
         tr.src = 'data:text/vtt,' + encodeURIComponent(
           'WEBVTT\nheader\n\n1\n00:00.000 --> 00:01.000 line:-1\n<v.loud Bob>hi &amp; <b>bye</b><00:00.500>\n' +
           '00:00.000 --> 00:02.000\nsecond');
         window.r = [tr.readyState, tr.track.label, tr.track.language];
         tr.onload = () => r.push('load');
         r.join()"#,
    );
    assert_eq!(r, "0,L,fr");
    assert!(wait_for(&mut page, "r.includes('load')", 10));
    let r = eval(
        &mut page,
        r#"const cues = tr.track.cues;
         const f = cues[1].getCueAsHTML();
         const span = f.firstChild;
         const out = [tr.readyState, cues.length, cues[0].text, cues[1].id, cues[1].line,
           span.localName, span.className, span.title, span.lastChild.nodeType, span.lastChild.data,
           f.textContent];
         cues[1].startTime = -1;
         out.push(cues[0].id);
         tr.src = 'data:text/vtt,WEBVTT';
         out.push(cues.length, tr.track.cues === cues);
         r = [];
         out.join('|')"#,
    );
    assert_eq!(
        r,
        "2|2|second|1|-1|span|loud|Bob|7|00:00:00.500|hi & bye|1|0|true"
    );
    assert!(wait_for(&mut page, "r.includes('load')", 10));
    assert_eq!(eval(&mut page, "tr.track.cues.length"), "0");
}

/// While a media element's network state is LOADING, its `progress` checks run on a 350 ms timer
/// (HTML §4.8.11.5; Chromium's `HTMLMediaElement::StartProgressEventTimer`): a check that finds no
/// new data leaves the loop asleep until the next one, rather than due again at once.
#[test]
fn a_loading_media_element_does_not_keep_the_loop_awake() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<video></video><script>
         window.log = [];
         const v = document.querySelector('video');
         const ms = new MediaSource();
         ms.addEventListener('sourceopen', () => {
           const sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42E01E\"');
           sb.addEventListener('updateend', () => log.push('updateend'));
           sb.appendBuffer(new Uint8Array([0, 0, 0, 8, 0x66, 0x72, 0x65, 0x65]));
         });
         v.src = URL.createObjectURL(ms);
         </script>",
        AutoplayPolicy::Allowed,
    );
    assert!(wait_for(&mut page, "log.includes('updateend')", 20));
    let settle = Instant::now() + Duration::from_secs(1);
    while Instant::now() < settle {
        page.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(&mut page, "v.networkState"), "2", "NETWORK_LOADING");
    // A second of the loop, sleeping until its next wakeup each time: a timer wakes it a few
    // times, a wakeup left in the past thousands.
    let end = Instant::now() + Duration::from_secs(1);
    let mut wakes = 0;
    while Instant::now() < end {
        page.turn();
        wakes += 1;
        let next = page.engine().next_wakeup().unwrap_or(end).min(end);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    assert!(wakes < 100, "the loop woke {wakes} times in a second");
}

#[test]
fn media_source_opens_and_appends() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<video></video><script>
         window.log = [];
         const v = document.querySelector('video');
         const ms = new MediaSource();
         log.push(ms.readyState);
         ms.addEventListener('sourceopen', () => {
           log.push('sourceopen ' + ms.readyState);
           const sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42E01E\"');
           log.push('buffers ' + ms.sourceBuffers.length);
           sb.addEventListener('updateend', () => log.push('updateend'));
           sb.appendBuffer(new Uint8Array([0, 0, 0, 8, 0x66, 0x72, 0x65, 0x65]));
           log.push('updating ' + sb.updating);
         });
         v.src = URL.createObjectURL(ms);
         </script>",
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('updateend')", 20),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(&mut page, "log.join()"),
        "closed,sourceopen open,buffers 1,updating true,updateend"
    );
    assert_eq!(
        eval(
            &mut page,
            "[MediaSource.isTypeSupported('video/mp4; codecs=\"avc1.42E01E\"'),
              MediaSource.isTypeSupported('video/ogg; codecs=\"theora\"'),
              MediaSource.isTypeSupported('')].join()"
        ),
        "true,false,false"
    );
    // Parameters other than `codecs` don't change the answer (Chromium's
    // `MediaSource::IsTypeSupportedInternal` reads only the type and `codecs`).
    assert_eq!(
        eval(
            &mut page,
            "[MediaSource.isTypeSupported('video/mp4; codecs=\"avc1.42E01E\"; eotf=catavision'),
              MediaSource.isTypeSupported('video/mp4; codecs=\"avc1.42E01E\"; width=99999'),
              MediaSource.isTypeSupported('video/mp4; codecs=\"avc1.4D401F\"; framerate=9999')].join()"
        ),
        "true,true,true"
    );
    // VP9 by profile and depth, from the decoders' and the GPU's decoder profiles: profile 1
    // (4:4:4) has no GPU decoder profile and no decoder here takes it, though Media Foundation's own
    // MSE check, which brings up a decoder per question, says yes to every VP9 profile.
    if snowlark_media::support()
        .codecs
        .contains(&snowlark_media::caps::Codec::Vp9)
    {
        assert_eq!(
            eval(
                &mut page,
                "[MediaSource.isTypeSupported('video/webm; codecs=\"vp09.00.51.08\"'),
                  MediaSource.isTypeSupported('video/webm; codecs=\"vp09.01.51.08\"')].join()"
            ),
            "true,false"
        );
    }
}

/// MSE §5.5.7: the element has its metadata once every `SourceBuffer` has had its first
/// initialization segment, whatever the media data appended (Chromium: `ChunkDemuxer::
/// OnSourceInitDone`, then the pipeline's `ReportMetadata` before any preroll). Here the data
/// starts at 2 s, as a page that starts a video part way does (it appends from the key frame
/// before its start time, and seeks there after `loadedmetadata`): Media Foundation's player
/// opens only once it has data at the playback position, so before the fix `loadedmetadata`
/// never came and the seek never happened.
#[test]
fn media_source_metadata_comes_with_the_initialization_segments() {
    let clip = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern-frag.mp4"),
    )
    .unwrap();
    // The initialization segment is everything before the first `moof`.
    let moof = clip.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let bytes = |b: &[u8]| {
        b.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!(
            "<video></video>{LOG}<script>
             const init = new Uint8Array([{}]), media = new Uint8Array([{}]);
             const ms = new MediaSource();
             ms.addEventListener('sourceopen', () => {{
               const sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42C00D\"');
               sb.addEventListener('error', () => log.push('sb error'));
               sb.addEventListener('updateend', () => {{
                 log.push('updateend ' + v.readyState);
                 if (sb.buffered.length) return;
                 sb.timestampOffset = 2;
                 sb.appendBuffer(media);
               }});
               sb.appendBuffer(init);
             }});
             v.src = URL.createObjectURL(ms);
             </script>",
            bytes(&clip[..moof]),
            bytes(&clip[moof..])
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('loadedmetadata')", 20),
        "{} readyState {}",
        eval(&mut page, "log.join()"),
        eval(&mut page, "v.readyState")
    );
    // `loadedmetadata` before the first append's `updateend`: the initialization segment is
    // received inside that append.
    assert_eq!(
        eval(
            &mut page,
            "log.filter(e => e != 'suspend').slice(0, 4).join()"
        ),
        "loadstart,durationchange,resize,loadedmetadata"
    );
    assert_eq!(
        eval(
            &mut page,
            "[v.readyState >= 1, v.videoWidth, v.videoHeight, v.duration > 0, ms.duration === v.duration].join()"
        ),
        "true,160,90,true,true"
    );
    assert!(
        wait_for(&mut page, "v.buffered.length == 1", 10),
        "{}",
        eval(
            &mut page,
            "[log.join(), ms.readyState, ms.sourceBuffers[0].buffered.length, v.error && v.error.code].join(' ')"
        )
    );
    assert_eq!(
        eval(&mut page, "Math.round(v.buffered.start(0) * 10) / 10"),
        "2"
    );
    // The seek into the data: `seeked`, a frame, and playback from there.
    eval(&mut page, "log = []; v.currentTime = 2.5;");
    assert!(
        wait_for(&mut page, "log.includes('seeked') && v.readyState >= 2", 20),
        "{} readyState {}",
        eval(&mut page, "log.join()"),
        eval(&mut page, "v.readyState")
    );
    eval(&mut page, "v.play();");
    assert!(
        wait_for(&mut page, "v.currentTime > 2.8 && !v.paused", 10),
        "{} currentTime {}",
        eval(&mut page, "log.join()"),
        eval(&mut page, "v.currentTime")
    );
}

/// Turns the event loop for `ms` milliseconds.
fn settle(page: &mut Page, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        page.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A seek outside the buffered ranges of a `MediaSource` (HTML §4.8.11.9 "seeking", MSE §2.4.4
/// "seeking"): `readyState` drops to HAVE_METADATA, `waiting` fires while playing, and the seek
/// waits for the page to append data at the new position; then `seeked`, `canplay` and `playing`
/// in that order and playback goes on from there. Chromium: `WebMediaPlayerImpl::DoSeek` sets
/// HAVE_METADATA, `HTMLMediaElement::SetReadyState` fires `waiting` and finishes the seek once the
/// state is back to HAVE_CURRENT_DATA or more, `ChunkDemuxer::Seek` holds the seek until
/// `SourceBufferStream::Seek` finds a range with the position.
#[test]
fn a_seek_past_the_buffered_data_waits_for_the_page_to_append_there() {
    let clip = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern-frag.mp4"),
    )
    .unwrap();
    let moof = clip.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let bytes = |b: &[u8]| {
        b.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!(
            "<video muted></video>{LOG}<script>
             const init = new Uint8Array([{}]), media = new Uint8Array([{}]);
             const ms = new MediaSource();
             let sb;
             ms.addEventListener('sourceopen', () => {{
               sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42C00D\"');
               ms.duration = 30;
               sb.addEventListener('updateend', function first() {{
                 sb.removeEventListener('updateend', first);
                 sb.appendBuffer(media);
               }});
               sb.appendBuffer(init);
             }});
             v.src = URL.createObjectURL(ms);
             window.appendAt = t => {{ sb.timestampOffset = t; sb.appendBuffer(media); }};
             window.events = () => log.filter(e => e != 'suspend').join();
             </script>",
            bytes(&clip[..moof]),
            bytes(&clip[moof..])
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "v.buffered.length == 1 && v.readyState == 4", 20),
        "{} readyState {}",
        eval(&mut page, "events()"),
        eval(&mut page, "v.readyState")
    );
    eval(&mut page, "v.play();");
    assert!(
        wait_for(&mut page, "v.currentTime > 0.5", 10),
        "{}",
        eval(&mut page, "events()")
    );

    // While playing: the seek drops to HAVE_METADATA at once and fires `waiting`.
    assert_eq!(
        eval(
            &mut page,
            "log = []; v.currentTime = 20; [v.seeking, v.readyState].join()"
        ),
        "true,1"
    );
    settle(&mut page, 1500);
    assert_eq!(
        eval(
            &mut page,
            "[events(), v.seeking, v.readyState, v.currentTime, v.paused].join(' ')"
        ),
        "seeking,waiting true 1 20 false"
    );
    // The page appends at the new position: the seek ends and playback goes on from there.
    eval(&mut page, "appendAt(20);");
    assert!(
        wait_for(&mut page, "log.includes('canplaythrough')", 20),
        "{} readyState {}",
        eval(&mut page, "events()"),
        eval(&mut page, "v.readyState")
    );
    assert_eq!(
        eval(&mut page, "[events(), v.seeking, v.readyState].join(' ')"),
        "seeking,waiting,seeked,canplay,playing,canplaythrough false 4"
    );
    assert!(
        wait_for(&mut page, "v.currentTime > 20.5", 10),
        "currentTime {}",
        eval(&mut page, "v.currentTime")
    );

    // Paused, then a seek out of the data and `play()` (YouTube's order): `waiting` from `play()`,
    // not `playing`, until the data is there.
    eval(
        &mut page,
        "v.pause(); log = []; v.currentTime = 26; v.play();",
    );
    settle(&mut page, 1500);
    assert_eq!(
        eval(
            &mut page,
            "[events(), v.seeking, v.readyState, v.currentTime].join(' ')"
        ),
        "pause,seeking,play,waiting true 1 26"
    );
    eval(&mut page, "appendAt(25);");
    assert!(
        wait_for(&mut page, "log.includes('canplaythrough')", 20),
        "{} readyState {}",
        eval(&mut page, "events()"),
        eval(&mut page, "v.readyState")
    );
    assert_eq!(
        eval(&mut page, "[events(), v.seeking, v.readyState].join(' ')"),
        "pause,seeking,play,waiting,seeked,canplay,playing,canplaythrough false 4"
    );
    assert!(
        wait_for(&mut page, "v.currentTime > 26.5", 10),
        "currentTime {}",
        eval(&mut page, "v.currentTime")
    );
}

/// Whether the page's display list (at 1 device px per CSS px) shows a video's frames.
fn paints_a_video(page: &Page) -> bool {
    page.engine()
        .paint(snowlark_paint::Point::new(0.0, 0.0), 1.0)
        .is_some_and(|l| l.items.iter().any(|i| i.is_video()))
}

/// A paused video that seeks shows the last frame it rendered until the new one is there (HTML
/// §4.8.9 "the video element": "when the video element is paused and the frame of video
/// corresponding to the current playback position is not available (e.g. because the video is
/// seeking or buffering), the video element represents the last frame of the video to have been
/// rendered"), though `readyState` is HAVE_METADATA meanwhile: only HAVE_NOTHING, or
/// HAVE_METADATA before any video data was obtained, shows the poster. A page that scrubs a video
/// by seeking it as it scrolls would otherwise blink between the frame and the poster.
#[test]
fn a_seeking_video_shows_its_last_frame() {
    let clip = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern-frag.mp4"),
    )
    .unwrap();
    let moof = clip.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let bytes = |b: &[u8]| {
        b.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!(
            "<video muted style='width:320px;height:180px'></video>{LOG}<script>
             const init = new Uint8Array([{}]), media = new Uint8Array([{}]);
             const ms = new MediaSource();
             let sb;
             ms.addEventListener('sourceopen', () => {{
               sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42C00D\"');
               ms.duration = 30;
               sb.addEventListener('updateend', function first() {{
                 sb.removeEventListener('updateend', first);
                 sb.appendBuffer(media);
               }});
               sb.appendBuffer(init);
             }});
             v.src = URL.createObjectURL(ms);
             </script>",
            bytes(&clip[..moof]),
            bytes(&clip[moof..])
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "v.buffered.length == 1 && v.readyState == 4", 20),
        "readyState {}",
        eval(&mut page, "v.readyState")
    );
    // (a seek inside the data first: the show poster flag is unset, and a frame is rendered)
    eval(&mut page, "log = []; v.currentTime = 1;");
    assert!(wait_for(&mut page, "log.includes('seeked')", 10));
    settle(&mut page, 300);
    assert!(paints_a_video(&page), "no frame before the seek");
    // Out of the buffered data: the seek waits at HAVE_METADATA, and the last frame stays.
    assert_eq!(
        eval(
            &mut page,
            "v.currentTime = 20; [v.seeking, v.readyState].join()"
        ),
        "true,1"
    );
    settle(&mut page, 500);
    assert_eq!(
        eval(&mut page, "[v.seeking, v.readyState].join()"),
        "true,1"
    );
    assert!(paints_a_video(&page), "the frame went away while seeking");
}

#[test]
fn content_security_policy_blocks_media() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!(
            "<meta http-equiv=Content-Security-Policy content=\"media-src 'none'\">\
             <video src=pattern.mp4></video>{LOG}"
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "log.includes('error')", 10),
        "{}",
        eval(&mut page, "log.join()")
    );
    assert_eq!(
        eval(&mut page, "[v.error.code, v.readyState].join()"),
        "4,0"
    );
}

/// HTML lazy loading of media: a `loading=lazy` video's poster waits, as its resource does, until
/// the element is rendered near the viewport or `loading` turns eager. A video without a source is
/// its poster's size once that loads (300×150 before).
#[test]
fn a_lazy_videos_poster_waits_for_the_viewport() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let svg = |w: u32| {
        format!("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='{w}' height='10'/%3E")
    };
    let mut page = page(
        &net,
        &format!(
            "<video id=near loading=lazy poster=\"{}\"></video>
             <video id=eager poster=\"{}\"></video>
             <div style='height: 5000px'></div>
             <video id=far1 loading=lazy poster=\"{}\"></video>
             <video id=far2 loading=lazy poster=\"{}\"></video>
             <script>
               const w = id => document.getElementById(id).getBoundingClientRect().width;
             </script>",
            svg(20),
            svg(40),
            svg(30),
            svg(50)
        ),
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "w('near') == 20 && w('eager') == 40", 10),
        "{}",
        eval(&mut page, "[w('near'), w('eager')].join()")
    );
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        page.turn();
    }
    assert_eq!(eval(&mut page, "[w('far1'), w('far2')].join()"), "300,300");
    eval(
        &mut page,
        "document.getElementById('far1').loading = 'eager'",
    );
    assert!(
        wait_for(&mut page, "w('far1') == 30", 10),
        "{}",
        eval(&mut page, "w('far1')")
    );
    assert_eq!(eval(&mut page, "w('far2')"), "300");
    eval(
        &mut page,
        "document.getElementById('far2').scrollIntoView()",
    );
    assert!(
        wait_for(&mut page, "w('far2') == 50", 10),
        "{}",
        eval(&mut page, "w('far2')")
    );
}

#[test]
fn lazy_media_waits_for_the_viewport_and_cues_enter_and_exit() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<video id=near src=pattern.mp4 loading=lazy muted></video>
         <video id=far src=pattern.mp4 loading=lazy style='display: block; margin-top: 5000px'></video>
         <script>
           window.ev = [];
           const far = document.getElementById('far'), near = document.getElementById('near');
           far.onloadstart = () => ev.push('far loadstart');
           near.onloadstart = () => ev.push('near loadstart');
           const t = near.addTextTrack('captions');
           const c = new VTTCue(0.1, 0.4, 'hello');
           c.onenter = () => ev.push('enter');
           c.onexit = () => ev.push('exit');
           t.oncuechange = () => ev.push('cuechange ' + t.activeCues.length);
           t.addCue(c);
           const lazy = document.createElement('video');
           lazy.loading = 'lazy'; lazy.src = 'pattern.mp4';
           lazy.onloadstart = () => ev.push('detached loadstart');
         </script>",
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "ev.includes('near loadstart')", 10),
        "{}",
        eval(&mut page, "ev.join()")
    );
    eval(&mut page, "near.play()");
    assert!(
        wait_for(&mut page, "ev.includes('exit')", 10),
        "{}",
        eval(&mut page, "ev.join()")
    );
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        page.turn();
    }
    let ev = eval(&mut page, "ev.join()");
    assert!(
        !ev.contains("far loadstart") && !ev.contains("detached"),
        "{ev}"
    );
    assert!(
        ev.contains("enter,cuechange 1") && ev.contains("exit,cuechange 0"),
        "{ev}"
    );
}

#[test]
fn the_tab_counts_audible_media_and_its_mute_is_invisible_to_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let group = snowlark_media::AudioGroup::new();
    // (muted: the test makes no sound, and the tab's mute is below what counts as audible)
    group.set_muted(true);
    let mut page = page_in_tab(
        &net,
        "<audio src=pattern.mp4 autoplay loop></audio><video src=pattern.mp4 muted autoplay loop></video>",
        AutoplayPolicy::Allowed,
        Some(group.clone()),
    );
    let audible = |page: &mut Page, want: bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            page.turn();
            if group.audible() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    };
    assert!(wait_for(
        &mut page,
        "document.querySelector('audio').currentTime > 0.2 && document.querySelector('video').currentTime > 0.2",
        20
    ));
    assert!(
        audible(&mut page, true),
        "the audio element plays with sound"
    );
    // The page sees its own values only.
    assert_eq!(
        eval(
            &mut page,
            "const a = document.querySelector('audio'); [a.muted, a.volume, a.paused].join()"
        ),
        "false,1,false"
    );
    // Muted by the page: nothing audible (the muted autoplaying video never was).
    eval(&mut page, "document.querySelector('audio').muted = true");
    assert!(audible(&mut page, false), "the page muted its audio");
    eval(&mut page, "document.querySelector('audio').muted = false");
    assert!(audible(&mut page, true));
    eval(&mut page, "document.querySelector('audio').volume = 0");
    assert!(audible(&mut page, false), "volume 0");
    eval(&mut page, "document.querySelector('audio').volume = 1");
    assert!(audible(&mut page, true));
    // Unmuting the tab changes nothing the page sees either.
    group.set_muted(false);
    group.set_muted(true);
    assert_eq!(
        eval(
            &mut page,
            "const b = document.querySelector('audio'); [b.muted, b.volume].join()"
        ),
        "false,1"
    );
    eval(&mut page, "document.querySelector('audio').pause()");
    assert!(audible(&mut page, false), "paused");
}

/// HTML "HAVE_ENOUGH_DATA": `canplaythrough` is queued before the autoplay steps fire `play` and
/// `playing`.
#[test]
fn canplaythrough_comes_before_autoplay() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!("<video src=pattern.mp4 autoplay muted></video>{LOG}"),
        AutoplayPolicy::Allowed,
    );
    assert!(wait_for(&mut page, "log.includes('playing')", 20));
    assert_eq!(
        eval(
            &mut page,
            "log.filter(e => ['canplay','canplaythrough','play','playing'].includes(e)).slice(0, 4).join()"
        ),
        "canplay,canplaythrough,play,playing"
    );
}

/// HTML "is muted": while the muted state is "default" the element follows its `muted` attribute
/// (with no `volumechange`); once the setter ran, the attribute has no further effect.
#[test]
fn the_muted_attribute_mutes_until_the_setter_runs() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<p>", AutoplayPolicy::Allowed);
    let r = eval(
        &mut page,
        "window.changes = 0; const a = document.createElement('audio');
         a.onvolumechange = () => changes++;
         const r = [a.muted]; a.setAttribute('muted', ''); r.push(a.muted);
         a.removeAttribute('muted'); r.push(a.muted); a.defaultMuted = true; r.push(a.muted);
         a.muted = false; a.removeAttribute('muted'); a.setAttribute('muted', ''); r.push(a.muted);
         const b = document.createElement('video'); b.setAttribute('muted', ''); b.muted = true;
         b.removeAttribute('muted'); r.push(b.muted); r.join()",
    );
    assert_eq!(r, "false,true,false,true,false,true");
    page.run_for(Duration::from_millis(100));
    // (only the setter that changed `a.muted`, from true to false, fired one)
    assert_eq!(eval(&mut page, "changes"), "1");
}

/// HTML resource fetch algorithm, "once the entire media resource has been fetched": a task fires
/// `progress` (the network state still NETWORK_LOADING), then sets NETWORK_IDLE and fires
/// `suspend`.
#[test]
fn a_fetched_resource_fires_progress_then_suspend() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<video src=pattern.mp4 autoplay muted></video><script>window.seen = [];
         const v = document.querySelector('video');
         v.onprogress = () => seen.push('progress:' + v.networkState);
         v.onsuspend = () => seen.push('suspend:' + v.networkState);</script>",
        AutoplayPolicy::Allowed,
    );
    assert!(
        wait_for(&mut page, "seen.some(e => e.startsWith('suspend'))", 20),
        "{}",
        eval(&mut page, "seen.join()")
    );
    assert_eq!(
        eval(&mut page, "seen.slice(-2).join()"),
        "progress:2,suspend:1"
    );
}

/// MSE §3.5.? "segment parser loop": an append ends once, setting `updating` to false and then
/// queuing `update` and `updateend`. A page may append again as soon as `updating` is false (here
/// from its `update` handler, as players that stream small chunks do), and the earlier append's
/// `updateend` then comes while the new one runs: that append must keep `updating` true until its
/// own end, so a page that appends whenever `updating` is false never appends into a buffer that
/// is still busy (which failed the append and ended the stream with a decode error: a video that
/// switched to 4K on YouTube errored out and was reloaded at 480p).
#[test]
fn an_append_ends_once_however_the_media_side_reports_it() {
    let clip = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern-frag.mp4"),
    )
    .unwrap();
    let moof = clip.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let bytes = |b: &[u8]| {
        b.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!(
            "<video></video>{LOG}<script>
             const init = new Uint8Array([{}]), media = new Uint8Array([{}]);
             const CHUNK = 256, ROUNDS = 12, per = Math.ceil(media.length / CHUNK);
             window.started = 0; window.updates = 0; window.ends = 0; window.errors = 0;
             window.done = false; window.threw = '';
             const ms = new MediaSource();
             window.ms = ms;
             let sb, i = 0;
             function next() {{
               if (sb.updating || errors || done) return;
               if (i >= per * ROUNDS) {{ done = true; return; }}
               const k = i % per;
               try {{ sb.appendBuffer(media.subarray(k * CHUNK, (k + 1) * CHUNK)); }}
               catch (e) {{ threw = e.name; return; }}
               i++; started++;
             }}
             ms.addEventListener('sourceopen', () => {{
               sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42C00D\"');
               sb.mode = 'sequence';
               sb.addEventListener('update', () => {{ updates++; next(); }});
               sb.addEventListener('updateend', () => {{ ends++; next(); }});
               sb.addEventListener('error', () => {{ errors++; }});
               sb.appendBuffer(init);
               started++;
             }});
             v.src = URL.createObjectURL(ms);
             </script>",
            bytes(&clip[..moof]),
            bytes(&clip[moof..])
        ),
        AutoplayPolicy::Allowed,
    );
    // The event loop turns without a pause, as a busy window's does, so the media side's reports
    // of one append can come in separate turns.
    let deadline = Instant::now() + Duration::from_secs(60);
    let state = "(done && !ms.sourceBuffers[0].updating) || errors > 0 || !!v.error || !!threw";
    loop {
        page.turn();
        if eval(&mut page, state) == "true" || Instant::now() > deadline {
            break;
        }
    }
    settle(&mut page, 300);
    assert_eq!(
        eval(
            &mut page,
            "[errors, v.error && v.error.code, threw, ms.readyState, done, \
              updates == started, ends == started].join()"
        ),
        "0,,,open,true,true,true",
        "{} appends, {} update, {} updateend; log {}",
        eval(&mut page, "started"),
        eval(&mut page, "updates"),
        eval(&mut page, "ends"),
        eval(&mut page, "log.join()")
    );
}

/// An HTTP/1.1 server for the fixture clip and a WebVTT file sent after `vtt_delay`, so a page
/// served from it fetches its tracks same-origin.
fn serve_clip_and_track(vtt_delay: Duration) -> url::Url {
    use std::io::{BufRead, BufReader, Write};
    let clip = std::fs::read(fixtures().to_file_path().unwrap().join("pattern.mp4")).unwrap();
    let clip = std::sync::Arc::new(clip);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let clip = clip.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut first = String::new();
                    if reader.read_line(&mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
                    let (mime, body): (&str, Vec<u8>) = if path.ends_with(".vtt") {
                        std::thread::sleep(vtt_delay);
                        (
                            "text/vtt",
                            b"WEBVTT\n\n00:00.000 --> 00:10.000\nhi\n".to_vec(),
                        )
                    } else if path.ends_with(".mp4") {
                        ("video/mp4", clip.to_vec())
                    } else {
                        ("text/plain", Vec::new())
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err()
                        || stream.write_all(&body).is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    url::Url::parse(&format!("http://{addr}/test.html")).unwrap()
}

/// HTML ready states: HAVE_FUTURE_DATA needs "the text tracks are ready", so a `default` track
/// (enabled by automatic selection once the resource selection began) that is still loading holds
/// the element at HAVE_CURRENT_DATA: `canplay` comes after the track's `load`.
#[test]
fn the_ready_state_waits_for_the_text_tracks() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve_clip_and_track(Duration::from_millis(1500));
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.engine().set_media_config(MediaConfig {
        videos: snowlark_paint::Gpu::new()
            .ok()
            .map(|g| snowlark_paint::video::VideoStore::new(Some(g))),
        autoplay: AutoplayPolicy::Allowed,
        copy_frames: true,
        silent: true,
        audio: None,
        audio_out: None,
        cdm: None,
    });
    page.load_html(
        &format!(
            "<video src=pattern.mp4><track default kind=captions src=slow.vtt></video>{LOG}\
             <script>
             const tr = document.querySelector('track');
             tr.onload = () => log.push('track load');
             v.addEventListener('loadeddata', () => log.push('ready ' + v.readyState));
             </script>"
        ),
        &base,
    );
    assert!(
        wait_for(&mut page, "log.includes('canplaythrough')", 20),
        "{}",
        eval(&mut page, "log.join()")
    );
    let r = eval(
        &mut page,
        "const at = e => log.indexOf(e);
         [at('loadeddata') >= 0, at('ready 2') >= 0,
          at('track load') >= 0 && at('track load') < at('canplay'),
          tr.readyState, v.readyState].join()",
    );
    assert_eq!(r, "true,true,true,2,4", "{}", eval(&mut page, "log.join()"));
}

/// The load algorithm takes the ready state back to HAVE_NOTHING: no cue stays active once the
/// element's `src` is set to one that fails.
#[test]
fn no_cue_is_active_after_the_media_is_unloaded() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        &format!("<video src=pattern.mp4 muted></video>{LOG}"),
        AutoplayPolicy::Allowed,
    );
    eval(
        &mut page,
        "const t = v.addTextTrack('captions'); t.mode = 'showing';
         t.addCue(new VTTCue(0, 100, 'all along'));
         v.play();",
    );
    assert!(
        wait_for(
            &mut page,
            "t.activeCues.length == 1 && v.currentTime > 0",
            20
        ),
        "{}",
        eval(&mut page, "log.join()")
    );
    eval(&mut page, "v.src = ''");
    assert!(wait_for(&mut page, "log.includes('error')", 10));
    assert_eq!(
        eval(&mut page, "[v.networkState, t.activeCues.length].join()"),
        "3,0"
    );
}
