// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Encrypted Media Extensions through the page: the Clear Key key system's configuration,
//! sessions, licence request and response, and key statuses.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    page
}

/// Runs an async script and returns what it resolves to (or "threw ...").
fn run_async(page: &mut Page, js: &str) -> String {
    page.eval(
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e.name + ': ' + e.message);"),
        "test.js",
    )
    .unwrap();
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    page.eval("out", "test.js").unwrap()
}

use snowlark_media::fmp4::{Sample, Track};
use snowlark_page::media::cdm::{
    Cdm, CdmEvent, CdmInbox, KeyStatus, MessageKind, PlayerControl, PlayerReport, PlayerState,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// An embedder's module that answers at once. Its instances are 7, 8, ...; its sessions 70, 71, ...
#[derive(Default)]
struct Fake {
    inbox: Mutex<Option<Arc<CdmInbox>>>,
    origin: Mutex<String>,
    instances: AtomicU64,
    sessions: AtomicU64,
    released: Mutex<Vec<u64>>,
    /// What its protected players were told: "play <keys> <tracks>", "sample <track> <time>
    /// <key>", "<control>", "stop".
    player: Mutex<Vec<String>>,
}
impl Fake {
    fn send(&self, e: CdmEvent) {
        self.inbox.lock().unwrap().as_ref().unwrap().deliver(e);
    }
}
impl Cdm for Fake {
    fn supports(&self, key_system: &str, content_type: &str) -> bool {
        matches!(key_system, "org.test.module" | "org.test.module.3000")
            && content_type.starts_with("video/mp4")
    }
    fn create_keys(&self, request: u64, _: &str, origin: &str, inbox: &Arc<CdmInbox>) -> u64 {
        *self.inbox.lock().unwrap() = Some(inbox.clone());
        *self.origin.lock().unwrap() = origin.to_owned();
        self.send(CdmEvent::Done {
            request,
            result: Ok(String::new()),
        });
        7 + self.instances.fetch_add(1, Ordering::Relaxed)
    }
    fn create_session(&self, keys: u64) -> u64 {
        assert!(!self.released.lock().unwrap().contains(&keys));
        70 + self.sessions.fetch_add(1, Ordering::Relaxed)
    }
    fn generate_request(&self, request: u64, session: u64, kind: &str, mut data: Vec<u8>) {
        assert_eq!(kind, "cenc");
        data.reverse();
        self.send(CdmEvent::Message {
            session,
            kind: MessageKind::LicenseRequest,
            data,
        });
        self.send(CdmEvent::Done {
            request,
            result: Ok("module-session".into()),
        });
    }
    fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        self.send(CdmEvent::KeyStatuses {
            session,
            statuses: vec![(response, KeyStatus::Usable)],
        });
        self.send(CdmEvent::Expiration {
            session,
            time: 1234.0,
        });
        self.send(CdmEvent::Done {
            request,
            result: Ok(String::new()),
        });
    }
    fn close(&self, request: u64, session: u64) {
        self.send(CdmEvent::Closed { session });
        self.send(CdmEvent::Done {
            request,
            result: Ok(String::new()),
        });
    }
    fn remove(&self, _: u64, _: u64) {}
    fn release(&self, keys: u64) {
        let mut r = self.released.lock().unwrap();
        assert!(!r.contains(&keys), "instance {keys} released twice");
        r.push(keys);
    }
    fn play(&self, keys: u64, tracks: Vec<Track>) -> u64 {
        let t: Vec<String> = tracks
            .iter()
            .map(|t| format!("{}:{}", t.id, if t.video { "video" } else { "audio" }))
            .collect();
        let mut log = self.player.lock().unwrap();
        log.push(format!("play {keys} {}", t.join(",")));
        500
    }
    fn sample(&self, player: u64, track: u32, sample: Sample) {
        assert_eq!(player, 500);
        self.player.lock().unwrap().push(format!(
            "sample {track} {} {}",
            sample.time,
            if sample.key { "key" } else { "-" }
        ));
    }
    fn control(&self, player: u64, control: PlayerControl) {
        assert_eq!(player, 500);
        self.player.lock().unwrap().push(format!("{control:?}"));
    }
    fn stop(&self, player: u64) {
        assert_eq!(player, 500);
        self.player.lock().unwrap().push("stop".into());
    }
}

/// A page that renders and plays media, as in a window, with the fake module.
fn media_page(fake: &Arc<Fake>) -> Page {
    let mut p = Page::new(PageOptions {
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    p.engine()
        .set_media_config(snowlark_page::media::MediaConfig {
            videos: snowlark_paint::Gpu::new()
                .ok()
                .map(|g| snowlark_paint::video::VideoStore::new(Some(g))),
            autoplay: snowlark_page::media::AutoplayPolicy::Allowed,
            silent: true,
            cdm: Some(fake.clone()),
            ..Default::default()
        });
    p.load_html(
        "<!doctype html>",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    p.run_until_settled(Duration::from_secs(5));
    p
}

fn page_with(fake: &Arc<Fake>) -> Page {
    let p = page();
    p.engine()
        .set_media_config(snowlark_page::media::MediaConfig {
            cdm: Some(fake.clone()),
            ..Default::default()
        });
    p
}

#[test]
fn clear_key_configuration() {
    let mut p = page();
    let r = run_async(
        &mut p,
        r#"
        const video = [{contentType: 'video/mp4; codecs="avc1.4d401e"'}];
        const out = [];
        for (const [ks, c] of [
            ['org.w3.clearkey', {initDataTypes: ['cenc', 'nope'], videoCapabilities: video, label: 'x'}],
            ['com.example.nope', {videoCapabilities: video}],
            ['org.w3.clearkey', {videoCapabilities: video, persistentState: 'required'}],
            ['org.w3.clearkey', {}],
            ['org.w3.clearkey', {videoCapabilities: [{contentType: 'video/mp4; codecs="avc1.4d401e"', robustness: 'HW_SECURE_ALL'}]}],
        ]) {
            try {
                const a = await navigator.requestMediaKeySystemAccess(ks, [c]);
                const g = a.getConfiguration();
                out.push([a.keySystem, g.label, g.initDataTypes.join('+'), g.videoCapabilities.length,
                    g.sessionTypes.join('+'), g.persistentState, g.distinctiveIdentifier].join(' '));
            } catch (e) { out.push(e.name); }
        }
        try { await navigator.requestMediaKeySystemAccess('', [{}]); } catch (e) { out.push(e.name); }
        try { await navigator.requestMediaKeySystemAccess('org.w3.clearkey', []); } catch (e) { out.push(e.name); }
        return out.join(' | ');
    "#,
    );
    assert_eq!(
        r,
        "org.w3.clearkey x cenc 1 temporary not-allowed not-allowed | NotSupportedError | \
         NotSupportedError | NotSupportedError | NotSupportedError | TypeError | TypeError"
    );
}

#[test]
fn clear_key_session_request_licence_and_statuses() {
    let mut p = page();
    let r = run_async(
        &mut p,
        r#"
        const access = await navigator.requestMediaKeySystemAccess('org.w3.clearkey',
            [{initDataTypes: ['keyids'], videoCapabilities: [{contentType: 'video/mp4; codecs="avc1.4d401e"'}]}]);
        const keys = await access.createMediaKeys();
        const s = keys.createSession();
        const out = [s.sessionId === '' ? 'no id' : 'id', String(s.expiration), s.keyStatuses.size];
        const kid = 'AAECAwQFBgcICQoLDA0ODw';
        const message = new Promise(r => s.onmessage = r);
        const changed = new Promise(r => s.addEventListener('keystatuseschange', r));
        await s.generateRequest('keyids', new TextEncoder().encode(JSON.stringify({kids: [kid]})));
        const m = await message;
        out.push(m instanceof MediaKeyMessageEvent, m.messageType, new TextDecoder().decode(m.message));
        out.push(s.sessionId !== '' ? 'has id' : 'no id');
        try { await s.update(new TextEncoder().encode('{}')); } catch (e) { out.push(e.name); }
        await s.update(new TextEncoder().encode(JSON.stringify(
            {keys: [{kty: 'oct', kid, k: 'EREREREREREREREREREREQ'}]})));
        await changed;
        const [[id, st]] = [...s.keyStatuses];
        out.push(new Uint8Array(id)[15], st, s.keyStatuses.has(new Uint8Array(id)), String(s.keyStatuses.get(new Uint8Array(16))));
        await s.close();
        out.push(await s.closed, s.keyStatuses.size);
        try { await s.generateRequest('keyids', new Uint8Array(1)); } catch (e) { out.push(e.name); }
        return out.join(' ');
    "#,
    );
    assert_eq!(
        r,
        "no id NaN 0 true license-request {\"kids\":[\"AAECAwQFBgcICQoLDA0ODw\"],\"type\":\"temporary\"} \
         has id TypeError 15 usable true undefined closed-by-application 0 InvalidStateError"
    );
}

#[test]
fn set_media_keys_attaches_to_one_element() {
    let mut p = page();
    let r = run_async(
        &mut p,
        r#"
        const access = await navigator.requestMediaKeySystemAccess('org.w3.clearkey',
            [{videoCapabilities: [{contentType: 'video/mp4; codecs="avc1.4d401e"'}]}]);
        const keys = await access.createMediaKeys();
        const a = document.createElement('video'), b = document.createElement('video');
        document.body.append(a, b);
        const out = [String(a.mediaKeys)];
        await a.setMediaKeys(keys);
        out.push(a.mediaKeys === keys);
        try { await b.setMediaKeys(keys); } catch (e) { out.push(e.name); }
        await a.setMediaKeys(null);
        await b.setMediaKeys(keys);
        out.push(String(a.mediaKeys), b.mediaKeys === keys);
        const ev = new MediaEncryptedEvent('encrypted', {initDataType: 'cenc'});
        out.push(ev.initDataType, String(ev.initData));
        return out.join(' ');
    "#,
    );
    assert_eq!(r, "null true QuotaExceededError null true cenc null");
}

/// A key system of the embedder's module (`MediaConfig::cdm`), here a fake that answers at once:
/// the configurations it is offered with, its instance and session, the module's message, key
/// statuses and expiration, and the page's close.
#[test]
fn a_key_system_of_the_embedders_module() {
    let fake = Arc::new(Fake::default());
    let mut p = page_with(&fake);
    let r = run_async(
        &mut p,
        r#"
        const video = [{contentType: 'video/mp4; codecs="avc1.64001f"'}];
        const out = [];
        for (const c of [{videoCapabilities: video, distinctiveIdentifier: 'not-allowed'},
                         {initDataTypes: ['keyids'], videoCapabilities: video},
                         {videoCapabilities: [{contentType: 'audio/webm'}]}]) {
            try { await navigator.requestMediaKeySystemAccess('org.test.module', [c]); out.push('yes'); }
            catch (e) { out.push(e.name); }
        }
        const a = await navigator.requestMediaKeySystemAccess('org.test.module',
            [{initDataTypes: ['cenc'], videoCapabilities: video}]);
        const g = a.getConfiguration();
        out.push(a.keySystem, g.initDataTypes.join('+'), g.distinctiveIdentifier, g.persistentState);
        const keys = await a.createMediaKeys();
        const s = keys.createSession();
        const message = new Promise(r => s.onmessage = r);
        const changed = new Promise(r => s.addEventListener('keystatuseschange', r));
        await s.generateRequest('cenc', new Uint8Array([1, 2, 3]));
        const m = await message;
        out.push(s.sessionId, m.messageType, new Uint8Array(m.message).join(','));
        const s2 = keys.createSession();
        try { await s2.generateRequest('keyids', new Uint8Array([1])); } catch (e) { out.push(e.name); }
        await s.update(new Uint8Array([9, 8]));
        await changed;
        const [[id, st]] = [...s.keyStatuses];
        out.push(new Uint8Array(id).join(','), st, s.expiration);
        await s.close();
        out.push(await s.closed);
        return out.join(' ');
    "#,
    );
    assert_eq!(
        r,
        "NotSupportedError NotSupportedError NotSupportedError org.test.module cenc required \
         required module-session license-request 3,2,1 NotSupportedError 9,8 usable 1234 \
         closed-by-application"
    );
    assert_eq!(*fake.origin.lock().unwrap(), "https://example.test");
}

/// A key system whose name ends in its security level (PlayReady's `.3000`) takes a robustness
/// that repeats that level, and no other; one with no level in its name takes none.
#[test]
fn a_robustness_that_repeats_the_key_systems_level() {
    let fake = Arc::new(Fake::default());
    let mut p = page_with(&fake);
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        for (const [ks, robustness] of [['org.test.module.3000', '3000'],
                                         ['org.test.module.3000', '2000'],
                                         ['org.test.module', '3000'],
                                         ['org.test.module.3000', '']]) {
            const c = {videoCapabilities: [{contentType: 'video/mp4; codecs="avc1.64001f"', robustness}]};
            try {
                const a = await navigator.requestMediaKeySystemAccess(ks, [c]);
                out.push(a.getConfiguration().videoCapabilities[0].robustness || 'empty');
            } catch (e) { out.push(e.name); }
        }
        return out.join(' ');
    "#,
    );
    assert_eq!(r, "3000 NotSupportedError NotSupportedError empty");
}

/// A module instance is released when the document that made it unloads (a removed frame, a
/// navigation) or the engine goes, once: its `MediaKeys` makes no session after that and its
/// sessions are closed.
#[test]
fn module_instances_are_released_with_their_document() {
    let fake = Arc::new(Fake::default());
    let mut p = page_with(&fake);
    let r = run_async(
        &mut p,
        r#"
        const f = document.createElement('iframe');
        document.body.append(f);
        const w = f.contentWindow;
        const config = [{initDataTypes: ['cenc'], videoCapabilities: [{contentType: 'video/mp4; codecs="avc1.64001f"'}]}];
        const a = await w.navigator.requestMediaKeySystemAccess('org.test.module', config);
        const keys = await a.createMediaKeys();
        const s = keys.createSession();
        const out = [];
        f.remove();
        try { keys.createSession(); out.push('made'); } catch (e) { out.push(e.name); }
        try { await s.generateRequest('cenc', new Uint8Array([1])); out.push('asked'); } catch (e) { out.push(e.name); }
        const own = await navigator.requestMediaKeySystemAccess('org.test.module', config);
        window.mine = await own.createMediaKeys();
        mine.createSession();
        return out.join(' ');
    "#,
    );
    assert_eq!(r, "InvalidStateError InvalidStateError");
    assert_eq!(*fake.released.lock().unwrap(), vec![7]);
    drop(p);
    assert_eq!(*fake.released.lock().unwrap(), vec![7, 8]);
}

/// A media element whose `MediaKeys` are the embedder's module's and whose source is a
/// `MediaSource` plays through the module's protected player: it is made with the tracks of the
/// initialization segment, handed every sample and the element's play, seek and end of stream; a
/// seek hands it the samples again from the key frame at or before the new position; what it
/// reports becomes the element's state (`playing`, `currentTime`, `seeked`, the playback quality).
#[test]
fn a_modules_player_plays_a_media_source() {
    let clip = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern-frag.mp4"),
    )
    .unwrap();
    let bytes: Vec<String> = clip.iter().map(|b| b.to_string()).collect();
    let fake = Arc::new(Fake::default());
    let mut p = media_page(&fake);
    let r = run_async(
        &mut p,
        &format!(
            r#"
        const clip = new Uint8Array([{}]);
        const v = window.v = document.createElement('video');
        v.muted = true;
        document.body.append(v);
        window.log = [];
        for (const e of ['playing', 'seeked', 'waiting', 'ended', 'error'])
            v.addEventListener(e, () => log.push(e));
        const a = await navigator.requestMediaKeySystemAccess('org.test.module',
            [{{initDataTypes: ['cenc'], videoCapabilities: [{{contentType: 'video/mp4; codecs="avc1.64001f"'}}]}}]);
        await v.setMediaKeys(await a.createMediaKeys());
        const ms = window.ms = new MediaSource();
        v.src = URL.createObjectURL(ms);
        await new Promise(r => ms.addEventListener('sourceopen', r, {{once: true}}));
        const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.42C00D"');
        sb.appendBuffer(clip);
        await new Promise(r => sb.addEventListener('updateend', r, {{once: true}}));
        v.play().catch(() => {{}});
        return [v.readyState, sb.buffered.length, v.paused].join();
    "#,
            bytes.join(",")
        ),
    );
    assert_eq!(r, "1,1,false");
    let log = || fake.player.lock().unwrap().clone();
    let first = log();
    assert_eq!(first[0], "play 7 1:video");
    let samples: Vec<&String> = first.iter().filter(|l| l.starts_with("sample")).collect();
    assert!(samples.len() > 10, "{first:?}");
    assert!(samples[0].starts_with("sample 1 ") && samples[0].ends_with(" key"));
    assert!(first.contains(&"Play".to_string()), "{first:?}");

    // The player reports: the element plays from its time, its frames counted.
    let send = |report| {
        fake.send(CdmEvent::Player {
            player: 500,
            report,
        })
    };
    send(PlayerReport::State {
        state: PlayerState::Playing,
        time: 0.5,
    });
    send(PlayerReport::Progress {
        time: 1.0,
        rendered: 24,
        dropped: 1,
    });
    let r = run_async(
        &mut p,
        "await new Promise(r => setTimeout(r, 50));
         const q = v.getVideoPlaybackQuality();
         return [log.join('+'), v.readyState, v.currentTime >= 1, q.totalVideoFrames, q.droppedVideoFrames].join();",
    );
    // (`waiting` first: playing at HAVE_METADATA, before the player's first frame)
    assert_eq!(r, "waiting+playing,4,true,25,1");

    // A seek: the player drops what it has and is handed the samples again from a key frame.
    fake.player.lock().unwrap().clear();
    let r = run_async(&mut p, "v.currentTime = 3.5; return v.seeking;");
    assert_eq!(r, "true");
    let again = log();
    assert_eq!(again[..2], ["Flush".to_string(), "Seek(3.5)".to_string()]);
    let resent: Vec<i64> = again
        .iter()
        .filter_map(|l| l.strip_prefix("sample 1 "))
        .map(|l| l.split(' ').next().unwrap().parse().unwrap())
        .collect();
    assert!(again[2].ends_with(" key"), "{again:?}");
    // (the clip's key frames are 3 s apart: from the one at 3 s)
    assert_eq!(resent[0], 30_000_000, "{again:?}");
    assert!(resent.len() < samples.len(), "{again:?}");
    send(PlayerReport::State {
        state: PlayerState::Seeked,
        time: 3.5,
    });
    let r = run_async(
        &mut p,
        "await new Promise(r => setTimeout(r, 50)); ms.endOfStream(); return [log.join('+'), v.seeking].join();",
    );
    assert!(
        r.starts_with("waiting+playing+") && r.contains("seeked") && r.ends_with(",false"),
        "{r}"
    );
    assert!(log().contains(&"End(1)".to_string()), "{:?}", log());

    // A new source: the player is stopped.
    run_async(&mut p, "v.removeAttribute('src'); v.load(); return 1;");
    assert!(log().contains(&"stop".to_string()), "{:?}", log());
}

/// By hand (`SNOWLARK_WPT`): the WPT `encrypted-media` H.264 file appended to a `MediaSource` of
/// an element with the module's keys: the media side keeps the encrypted stream without an error
/// (no `error` or `sourceclose`), and the player is handed its samples.
#[test]
#[ignore]
fn an_encrypted_stream_stays_with_the_media_side() {
    let Some(wpt) = std::env::var_os("SNOWLARK_WPT") else {
        eprintln!("SNOWLARK_WPT is not set: skipped");
        return;
    };
    let clip = std::fs::read(
        std::path::Path::new(&wpt)
            .join("encrypted-media/content/video_512x288_h264-360k_enc_dashinit.mp4"),
    )
    .unwrap();
    let bytes: Vec<String> = clip.iter().map(|b| b.to_string()).collect();
    let fake = Arc::new(Fake::default());
    let mut p = media_page(&fake);
    let r = run_async(
        &mut p,
        &format!(
            r#"
        const clip = new Uint8Array([{}]);
        const v = window.v = document.createElement('video');
        v.muted = true;
        document.body.append(v);
        const log = window.log = [];
        for (const e of ['error', 'encrypted', 'waitingforkey', 'loadedmetadata'])
            v.addEventListener(e, () => log.push(e));
        const a = await navigator.requestMediaKeySystemAccess('org.test.module',
            [{{initDataTypes: ['cenc'], videoCapabilities: [{{contentType: 'video/mp4; codecs="avc1.64001f"'}}]}}]);
        await v.setMediaKeys(await a.createMediaKeys());
        const ms = new MediaSource();
        ms.addEventListener('sourceclose', () => log.push('sourceclose'));
        v.src = URL.createObjectURL(ms);
        await new Promise(r => ms.addEventListener('sourceopen', r, {{once: true}}));
        const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
        sb.addEventListener('error', () => log.push('sb error'));
        sb.addEventListener('updateend', () => log.push('updateend'));
        sb.appendBuffer(clip);
        log.push('appended');
        await new Promise(r => sb.addEventListener('updateend', r, {{once: true}}));
        await new Promise(r => setTimeout(r, 3000));
        return [log.join('+'), ms.readyState, sb.buffered.length && sb.buffered.end(0).toFixed(1), v.error && v.error.code].join();
    "#,
            bytes.join(",")
        ),
    );
    let log = fake.player.lock().unwrap().clone();
    let samples = log.iter().filter(|l| l.starts_with("sample")).count();
    let events = p.eval("log.join('+')", "test.js").unwrap();
    eprintln!(
        "{r} ({events}); {samples} samples; {:?}",
        &log[..log.len().min(3)]
    );
    assert!(
        !r.contains("error") && !r.contains("sourceclose") && r != "pending",
        "{r} ({events})"
    );
    assert!(samples > 50, "{samples}");
}
