// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Cryptography's SubtleCrypto through script: algorithm normalization errors, the key operations'
//! round trips, CryptoKey's cached objects, and structured clone of keys.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn secure_page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("https://example.test/").unwrap(),
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

/// Runs an async function body and returns what it resolved with (or the rejection's name).
fn run_async(page: &mut Page, body: &str) -> String {
    eval(
        page,
        &format!(
            "var out = 'pending'; (async () => {{ {body} }})().then(v => out = String(v), e => out = 'ERR ' + e.name + ': ' + e.message); 'ok'"
        ),
    );
    page.run_until_settled(Duration::from_secs(20));
    eval(page, "out")
}

#[test]
fn normalization_errors_reject_with_the_spec_errors() {
    let mut page = secure_page();
    let r = run_async(
        &mut page,
        r#"
        const s = crypto.subtle;
        const names = [];
        const key = await s.generateKey({ name: 'AES-GCM', length: 128 }, false, ['encrypt']);
        for (const p of [
          s.encrypt({ name: 'AES-GCM' }, key, new Uint8Array(1)),
          s.encrypt({ name: 'AES-XYZ', iv: new Uint8Array(12) }, key, new Uint8Array(1)),
          s.encrypt({ name: 'AES-GCM', iv: new Uint8Array(12) }, key, {}),
          s.decrypt({ name: 'AES-GCM', iv: new Uint8Array(12) }, key, new Uint8Array(16)),
          s.importKey('raw', { kty: 'oct' }, 'AES-GCM', true, ['encrypt']),
          s.importKey('jwk', new Uint8Array(16), 'AES-GCM', true, ['encrypt']),
          s.generateKey({ name: 'AES-GCM', length: 100 }, true, ['encrypt']),
          s.generateKey({ name: 'AES-GCM', length: 128 }, true, ['sign']),
          s.generateKey({ name: 'HMAC', hash: 'SHA-0' }, true, ['sign']),
          s.exportKey('raw', key),
        ]) {
          try { await p; names.push('ok'); } catch (e) { names.push(e.name); }
        }
        return names.join(' ');
        "#,
    );
    assert_eq!(
        r,
        "TypeError NotSupportedError TypeError InvalidAccessError TypeError TypeError \
         OperationError SyntaxError NotSupportedError InvalidAccessError"
    );
}

#[test]
fn keys_round_trip_and_cache_their_objects() {
    let mut page = secure_page();
    let r = run_async(
        &mut page,
        r#"
        const s = crypto.subtle;
        const key = await s.generateKey({ name: 'AES-GCM', length: 256 }, true, ['decrypt', 'encrypt', 'encrypt']);
        const iv = new Uint8Array(12);
        const ct = await s.encrypt({ name: 'aes-gcm', iv, tagLength: 96 }, key, new TextEncoder().encode('hi'));
        const pt = new TextDecoder().decode(await s.decrypt({ name: 'AES-GCM', iv, tagLength: 96 }, key, ct));
        const jwk = await s.exportKey('jwk', key);
        const again = await s.importKey('jwk', jwk, 'AES-GCM', false, ['decrypt']);
        const pt2 = new TextDecoder().decode(await s.decrypt({ name: 'AES-GCM', iv, tagLength: 96 }, again, ct));
        return [ct.byteLength, pt, pt2, jwk.alg, jwk.key_ops.join(','), key.type,
                key.algorithm === key.algorithm, key.usages === key.usages, key.usages.join(','),
                key.algorithm.name, key.algorithm.length, again.extractable].join(' ');
        "#,
    );
    assert_eq!(
        r,
        "14 hi hi A256GCM encrypt,decrypt secret true true encrypt,decrypt AES-GCM 256 false"
    );
}

#[test]
fn ecdh_derives_keys_and_ecdsa_signs() {
    let mut page = secure_page();
    let r = run_async(
        &mut page,
        r#"
        const s = crypto.subtle;
        const a = await s.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveKey']);
        const b = await s.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
        const k = await s.deriveKey({ name: 'ECDH', public: b.publicKey }, a.privateKey,
                                    { name: 'HMAC', hash: 'SHA-256', length: 256 }, true, ['sign']);
        const bits = await s.deriveBits({ name: 'ECDH', public: a.publicKey }, b.privateKey, 256);
        const raw = new Uint8Array(await s.exportKey('raw', k));
        const same = raw.every((x, i) => x === new Uint8Array(bits)[i]);
        const e = await s.generateKey({ name: 'ECDSA', namedCurve: 'P-384' }, false, ['sign', 'verify']);
        const data = new Uint8Array([1, 2, 3]);
        const sig = await s.sign({ name: 'ECDSA', hash: 'SHA-384' }, e.privateKey, data);
        const ok = await s.verify({ name: 'ECDSA', hash: 'SHA-384' }, e.publicKey, sig, data);
        const spki = await s.exportKey('spki', e.publicKey);
        return [same, k.algorithm.hash.name, sig.byteLength, ok, spki.byteLength,
                a.publicKey.usages.length, e.privateKey.algorithm.namedCurve].join(' ');
        "#,
    );
    assert_eq!(r, "true SHA-256 96 true 120 0 P-384");
}

#[test]
fn keys_survive_structured_clone() {
    let mut page = secure_page();
    let r = run_async(
        &mut page,
        r#"
        const s = crypto.subtle;
        const pair = await s.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
        const { priv, pub } = structuredClone({ priv: pair.privateKey, pub: pair.publicKey });
        const data = new Uint8Array([9]);
        const sig = await s.sign('Ed25519', priv, data);
        const ok = await s.verify('Ed25519', pair.publicKey, sig, data);
        const raw1 = new Uint8Array(await s.exportKey('raw', pub));
        const raw2 = new Uint8Array(await s.exportKey('raw', pair.publicKey));
        return [ok, priv !== pair.privateKey, priv.extractable, priv.usages.join(','),
                raw1.every((x, i) => x === raw2[i])].join(' ');
        "#,
    );
    assert_eq!(r, "true true false sign true");
}
