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
