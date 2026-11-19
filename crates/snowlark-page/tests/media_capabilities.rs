// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Capabilities through the page: `navigator.mediaCapabilities`, the configuration checks
//! (a TypeError for an invalid one), and decoding answered from what the media stack plays.

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
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e);"),
        "test.js",
    )
    .unwrap();
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    page.eval("out", "test.js").unwrap()
}

const VIDEO: &str = "{contentType: 'video/mp4; codecs=\"avc1.42E01E\"', width: 1280, height: 720, bitrate: 2e6, framerate: 30}";

#[test]
fn invalid_configurations_reject_with_a_type_error() {
    let mut p = page();
    let r = run_async(
        &mut p,
        &format!(
            r#"
        const mc = navigator.mediaCapabilities;
        if (mc !== navigator.mediaCapabilities || !(mc instanceof MediaCapabilities)) return 'not the same object';
        const video = {VIDEO};
        const cases = [
            {{type: 'file'}},
            {{type: 'file', video: {{...video, framerate: 0}}}},
            {{type: 'file', video: {{...video, contentType: 'video/mp4'}}}},
            {{type: 'file', video: {{...video, contentType: 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"'}}}},
            {{type: 'file', video: {{...video, contentType: 'audio/mp4; codecs="avc1.42E01E"'}}}},
            {{type: 'file', audio: {{contentType: 'audio/mpeg; codecs=mp3'}}}},
            {{type: 'file', video: {{...video, contentType: 'application/ogg; codecs=vorbis'}}}},
            {{type: 'file', video: {{...video, scalabilityMode: 'L1T1'}}}},
            {{type: 'webrtc', video: {{...video, contentType: 'video/VP8'}}, keySystemConfiguration: {{keySystem: 'org.w3.clearkey'}}}},
        ];
        const out = [];
        for (const c of cases) {{
            try {{ await mc.decodingInfo(c); out.push('resolved'); }} catch (e) {{ out.push(e.name); }}
        }}
        return out.join();
        "#
        ),
    );
    assert_eq!(r, ["TypeError"; 9].join(","));
}

#[test]
fn decoding_is_answered_from_what_plays() {
    let mut p = page();
    let r = run_async(
        &mut p,
        &format!(
            r#"
        const mc = navigator.mediaCapabilities;
        const order = [];
        const v = mc.decodingInfo({{type: 'file', video: {VIDEO}}}).then(i => {{ order.push('info'); return i; }});
        Promise.resolve().then(() => order.push('microtask'));
        const i = await v;
        const keys = Object.keys(i).join(' ');
        const echo = i.configuration.video.contentType === 'video/mp4; codecs="avc1.42E01E"';
        const a = await mc.decodingInfo({{type: 'file', audio: {{contentType: 'audio/mpeg'}}}});
        const m = await mc.decodingInfo({{type: 'media-source', audio: {{contentType: 'audio/mp4; codecs="mp4a.40.2"'}}}});
        const n = await mc.decodingInfo({{type: 'file', video: {{...{VIDEO}, contentType: 'video/x-unknown; codecs=foo'}}}});
        const w = await mc.decodingInfo({{type: 'webrtc', video: {{...{VIDEO}, contentType: 'video/VP8'}}}});
        const e = await mc.encodingInfo({{type: 'record', video: {VIDEO}}});
        return [order.join(' '), keys, echo, i.supported, i.smooth, typeof i.powerEfficient,
                i.keySystemAccess, a.supported, a.powerEfficient, m.supported,
                n.supported, n.smooth, n.powerEfficient, w.supported, e.supported].join();
        "#
        ),
    );
    assert_eq!(
        r,
        "microtask info,powerEfficient smooth supported configuration keySystemAccess,true,true,true,boolean,,true,true,true,false,false,false,false,false"
    );
}

#[test]
fn a_key_system_configuration_is_not_supported() {
    // No key system: not supported, and no MediaKeySystemAccess.
    let mut p = page();
    let r = run_async(
        &mut p,
        &format!(
            r#"
        const i = await navigator.mediaCapabilities.decodingInfo({{type: 'file', video: {VIDEO},
            keySystemConfiguration: {{keySystem: 'org.w3.clearkey'}}}});
        return [i.supported, i.keySystemAccess === null].join();
        "#
        ),
    );
    assert_eq!(r, "false,true");
}
