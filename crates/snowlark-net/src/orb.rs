// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opaque-response blocking: the pieces of Fetch's "opaque-response-safelist check" (the ORB
//! proposal, annevk/orb, as integrated in whatwg/fetch#1442) that need no body stream: the MIME
//! type sets, the byte-pattern sniffers of MIME Sniffing §6, the partial-response check and the
//! JSON test. The check itself runs in main fetch ([`crate::fetch`]), after redirects, on every
//! `no-cors` response with opaque tainting, before any byte of it reaches the requester.
//!
//! The one step that needs a script parser, "determine if response is JavaScript and not JSON",
//! goes through a [`ScriptCheck`] the owner of the network thread installs
//! ([`crate::NetThread::set_script_check`]); without one, a response that reaches that step is
//! blocked.

use crate::headers::HeaderList;
use crate::mime::MimeType;

/// Parses scripts for the network layer: "ParseText(sourceText, Script) returns a Script Record".
/// `reply` is called, from any thread, with whether `source` parses as a classic script.
pub trait ScriptCheck: Send + Sync {
    fn parses_as_script(&self, source: String, reply: Box<dyn FnOnce(bool) + Send>);
}

/// Fetch "opaque-response-safelisted MIME type": a JavaScript MIME type, `text/css` or
/// `image/svg+xml`. Audio and video types (and `application/ogg`) are admitted here too, without
/// sniffing: media formats whose first bytes MIME Sniffing §6.2 does not know (MPEG-TS, ADTS, FLAC)
/// are common enough that a type the server labels is taken at its word, as the reference
/// implementations do.
pub fn is_safelisted(mime: &MimeType) -> bool {
    if mime.is_javascript() {
        return true;
    }
    let essence = mime.essence();
    essence == "text/css"
        || essence == "image/svg+xml"
        || essence == "application/ogg"
        || mime.type_ == "audio"
        || mime.type_ == "video"
}

/// Fetch "opaque-response-blocklisted MIME type": an HTML, JSON or XML MIME type (MIME Sniffing
/// §4.6).
pub fn is_blocklisted(mime: &MimeType) -> bool {
    let essence = mime.essence();
    essence == "text/html"
        || essence == "application/json"
        || essence == "text/json"
        || essence == "application/xml"
        || essence == "text/xml"
        || mime.subtype.ends_with("+json")
        || mime.subtype.ends_with("+xml")
}

/// Fetch "opaque-response-blocklisted-never-sniffed MIME type", with the proposal's four media
/// playlist and caption types (`application/dash+xml`, `application/vnd.apple.mpegurl`,
/// `audio/mpegurl`, `text/vtt`) that the Fetch text leaves out and the tests expect.
pub fn is_never_sniffed(essence: &str) -> bool {
    matches!(
        essence,
        "application/dash+xml"
            | "application/gzip"
            | "application/msexcel"
            | "application/mspowerpoint"
            | "application/msword"
            | "application/msword-template"
            | "application/pdf"
            | "application/vnd.apple.mpegurl"
            | "application/vnd.ces-quickpoint"
            | "application/vnd.ces-quicksheet"
            | "application/vnd.ces-quickword"
            | "application/vnd.ms-excel"
            | "application/vnd.ms-excel.sheet.macroenabled.12"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
            | "application/vnd.ms-word"
            | "application/vnd.ms-word.document.12"
            | "application/vnd.ms-word.document.macroenabled.12"
            | "application/vnd.msword"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.openxmlformats-officedocument.presentationml.template"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
            | "application/vnd.presentation-openxml"
            | "application/vnd.presentation-openxmlm"
            | "application/vnd.spreadsheet-openxml"
            | "application/vnd.wordprocessing-openxml"
            | "application/x-gzip"
            | "application/x-protobuf"
            | "application/x-protobuffer"
            | "application/zip"
            | "audio/mpegurl"
            | "multipart/byteranges"
            | "multipart/signed"
            | "text/event-stream"
            | "text/csv"
            | "text/vtt"
    )
}

/// Fetch "determine nosniff".
pub fn determine_nosniff(headers: &HeaderList) -> bool {
    headers
        .get_decode_split("x-content-type-options")
        .and_then(|v| v.into_iter().next())
        .is_some_and(|v| v.eq_ignore_ascii_case("nosniff"))
}

/// Background Fetch "validate a partial response" given 0 and the response (no previous one): the
/// `Content-Range` parses as a single byte content-range and starts at byte 0.
pub fn partial_response_starts_at_zero(headers: &HeaderList) -> bool {
    headers
        .get("content-range")
        .and_then(|v| content_range_values(v.trim()))
        .is_some_and(|(first, _, _)| first == 0)
}

/// Background Fetch "extract content-range values": `bytes <first>-<last>/<complete>`, where
/// `complete` is `*` or a number, as `(first, last, complete)`.
pub fn content_range_values(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.strip_prefix("bytes ")?;
    let (range, complete) = rest.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    fn digits(s: &str) -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    }
    let first = digits(first)?;
    let last = digits(last)?;
    if last < first {
        return None;
    }
    let complete = if complete == "*" {
        None
    } else {
        let n = digits(complete)?;
        if n <= last {
            return None;
        }
        Some(n)
    };
    Some((first, last, complete))
}

/// "Parse JSON bytes to a JavaScript value" does not throw: the body is JSON.
pub fn is_json(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// MIME Sniffing "pattern matching algorithm" with no leading bytes to ignore.
fn matches_pattern(input: &[u8], pattern: &[u8], mask: &[u8]) -> bool {
    input.len() >= pattern.len()
        && pattern
            .iter()
            .zip(mask)
            .zip(input)
            .all(|((p, m), b)| b & m == *p)
}

/// MIME Sniffing §6.1 "image type pattern matching algorithm".
pub fn image_type_pattern(input: &[u8]) -> Option<&'static str> {
    const ROWS: &[(&[u8], &[u8], &str)] = &[
        (b"\x00\x00\x01\x00", b"\xFF\xFF\xFF\xFF", "image/x-icon"),
        (b"\x00\x00\x02\x00", b"\xFF\xFF\xFF\xFF", "image/x-icon"),
        (b"BM", b"\xFF\xFF", "image/bmp"),
        (b"GIF87a", b"\xFF\xFF\xFF\xFF\xFF\xFF", "image/gif"),
        (b"GIF89a", b"\xFF\xFF\xFF\xFF\xFF\xFF", "image/gif"),
        (
            b"RIFF\x00\x00\x00\x00WEBPVP",
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
            "image/webp",
        ),
        (
            b"\x89PNG\r\n\x1A\n",
            b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
            "image/png",
        ),
        (b"\xFF\xD8\xFF", b"\xFF\xFF\xFF", "image/jpeg"),
    ];
    ROWS.iter()
        .find(|(p, m, _)| matches_pattern(input, p, m))
        .map(|(_, _, t)| *t)
        .or_else(|| matches_avif(input).then_some("image/avif"))
}

/// An AVIF image (AV1 Image File Format §9.1.1): an ISO base media file whose first box is a
/// `ftyp` with the brand `avif` (still image) or `avis` (image sequence), major or compatible.
/// MIME Sniffing's image table has no row for it, so without this the opaque-response check, which
/// blocks a body labelled as an image that sniffs as none, would block every cross-origin AVIF
/// image; the box is read as §6.2.1 reads an MP4's.
fn matches_avif(s: &[u8]) -> bool {
    if s.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize;
    if box_size < 16 || s.len() < box_size || !box_size.is_multiple_of(4) || &s[4..8] != b"ftyp" {
        return false;
    }
    let avif = |brand: &[u8]| brand == b"avif" || brand == b"avis";
    avif(&s[8..12]) || s[16..box_size].as_chunks::<4>().0.iter().any(|b| avif(b))
}

/// MIME Sniffing §6.2 "audio or video type pattern matching algorithm".
pub fn audio_or_video_type_pattern(input: &[u8]) -> Option<&'static str> {
    const ROWS: &[(&[u8], &[u8], &str)] = &[
        (
            b"FORM\x00\x00\x00\x00AIFF",
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            "audio/aiff",
        ),
        (b"ID3", b"\xFF\xFF\xFF", "audio/mpeg"),
        (b"OggS\x00", b"\xFF\xFF\xFF\xFF\xFF", "application/ogg"),
        (
            b"MThd\x00\x00\x00\x06",
            b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
            "audio/midi",
        ),
        (
            b"RIFF\x00\x00\x00\x00AVI ",
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            "video/avi",
        ),
        (
            b"RIFF\x00\x00\x00\x00WAVE",
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            "audio/wave",
        ),
    ];
    if let Some((_, _, t)) = ROWS.iter().find(|(p, m, _)| matches_pattern(input, p, m)) {
        return Some(t);
    }
    if matches_mp4(input) {
        return Some("video/mp4");
    }
    if matches_webm(input) {
        return Some("video/webm");
    }
    if matches_mp3_without_id3(input) {
        return Some("audio/mpeg");
    }
    None
}

/// MIME Sniffing §6.2.1 "signature for MP4".
fn matches_mp4(s: &[u8]) -> bool {
    if s.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize;
    if s.len() < box_size || !box_size.is_multiple_of(4) {
        return false;
    }
    if &s[4..8] != b"ftyp" {
        return false;
    }
    if &s[8..11] == b"mp4" {
        return true;
    }
    let mut read = 16;
    while read + 3 <= box_size {
        if &s[read..read + 3] == b"mp4" {
            return true;
        }
        read += 4;
    }
    false
}

/// MIME Sniffing §6.2.2 "signature for WebM": the EBML header, then a DocType element (`42 82`)
/// whose padded value is `webm` within the first 38 bytes.
fn matches_webm(s: &[u8]) -> bool {
    if s.len() < 4 || s[..4] != [0x1A, 0x45, 0xDF, 0xA3] {
        return false;
    }
    let mut iter = 4;
    while iter + 1 < s.len() && iter < 38 {
        if s[iter] == 0x42 && s[iter + 1] == 0x82 {
            iter += 2;
            if iter >= s.len() {
                return false;
            }
            let number_size = vint_size(s[iter]);
            iter += number_size;
            if iter + 4 >= s.len() {
                return false;
            }
            // "matching a padded sequence": `webm`, possibly after NUL bytes, at `iter`.
            let mut at = iter;
            while at < s.len() && s[at] == 0 {
                at += 1;
            }
            return s.len() > at + 3 && &s[at..at + 4] == b"webm";
        }
        iter += 1;
    }
    false
}

/// The length in bytes of an EBML variable-size integer, from its first byte.
fn vint_size(first: u8) -> usize {
    let mut mask = 0x80u8;
    let mut size = 1;
    while size < 8 && first & mask == 0 {
        mask >>= 1;
        size += 1;
    }
    size
}

/// MIME Sniffing §6.2.3 "signature for MP3 without ID3": a frame header, and another where its
/// frame size says the next frame starts.
fn matches_mp3_without_id3(s: &[u8]) -> bool {
    let Some(size) = mp3_frame_size(s, 0) else {
        return false;
    };
    if size < 4 || size > s.len() {
        return false;
    }
    mp3_frame_size(s, size).is_some()
}

/// "Match an mp3 header" at `at`, then "parse an mp3 frame" and "compute an mp3 frame size": the
/// size, when the four bytes there are a valid MPEG layer III frame header.
fn mp3_frame_size(s: &[u8], at: usize) -> Option<usize> {
    const MP3_RATES: [u32; 15] = [
        0, 32000, 40000, 48000, 56000, 64000, 80000, 96000, 112000, 128000, 160000, 192000, 224000,
        256000, 320000,
    ];
    const MP25_RATES: [u32; 15] = [
        0, 8000, 16000, 24000, 32000, 40000, 48000, 56000, 64000, 80000, 96000, 112000, 128000,
        144000, 160000,
    ];
    const SAMPLE_RATES: [u32; 3] = [44100, 48000, 32000];
    if s.len() < at + 4 {
        return None;
    }
    let (b1, b2, b3) = (s[at], s[at + 1], s[at + 2]);
    if b1 != 0xFF || b2 & 0xE0 != 0xE0 {
        return None;
    }
    let layer = (b2 & 0x06) >> 1;
    if layer == 0 {
        return None;
    }
    let bitrate_index = ((b3 & 0xF0) >> 4) as usize;
    if bitrate_index == 15 {
        return None;
    }
    let sample_rate_index = ((b3 & 0x0C) >> 2) as usize;
    if sample_rate_index == 3 {
        return None;
    }
    if 4 - layer != 3 {
        return None;
    }
    let version = (b2 & 0x18) >> 3;
    let bitrate = if version & 0x01 != 0 {
        MP3_RATES[bitrate_index]
    } else {
        MP25_RATES[bitrate_index]
    };
    let mut sample_rate = SAMPLE_RATES[sample_rate_index];
    if version == 2 {
        sample_rate /= 2;
    } else if version == 0 {
        sample_rate /= 4;
    }
    let pad = (b3 & 0x02) >> 1;
    let scale = if version == 1 { 72 } else { 144 };
    if bitrate == 0 {
        return None;
    }
    let mut size = (bitrate * scale / sample_rate) as usize;
    if pad != 0 {
        size += 1;
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mime(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    #[test]
    fn the_mime_type_sets() {
        for t in [
            "text/javascript",
            "application/x-javascript",
            "text/css",
            "image/svg+xml",
            "audio/mpeg",
            "video/mp4",
        ] {
            assert!(is_safelisted(&mime(t)), "{t}");
        }
        for t in [
            "text/html",
            "application/json",
            "application/ld+json",
            "text/xml",
            "image/png",
        ] {
            assert!(!is_safelisted(&mime(t)), "{t}");
        }
        for t in [
            "text/html",
            "text/json",
            "application/ld+json",
            "application/xhtml+xml",
        ] {
            assert!(is_blocklisted(&mime(t)), "{t}");
        }
        assert!(!is_blocklisted(&mime("text/plain")));
        assert!(is_never_sniffed("application/pdf"));
        assert!(is_never_sniffed("text/vtt"));
        assert!(!is_never_sniffed("font/ttf"));
    }

    #[test]
    fn content_ranges() {
        assert_eq!(
            content_range_values("bytes 0-99/1010"),
            Some((0, 99, Some(1010)))
        );
        assert_eq!(content_range_values("bytes 10-99/*"), Some((10, 99, None)));
        assert_eq!(content_range_values("bytes 10-9/100"), None);
        assert_eq!(content_range_values("bytes 0-99/50"), None);
        assert_eq!(content_range_values("bytes */100"), None);
        let mut h = HeaderList::new();
        h.append("Content-Range", "bytes 0-99/1010");
        assert!(partial_response_starts_at_zero(&h));
        let mut h = HeaderList::new();
        h.append("Content-Range", "bytes 10-99/1010");
        assert!(!partial_response_starts_at_zero(&h));
        assert!(!partial_response_starts_at_zero(&HeaderList::new()));
    }

    #[test]
    fn json_detection() {
        assert!(is_json(b"{\"a\": 1}"));
        assert!(is_json(b"\xEF\xBB\xBF[1, 2]"));
        assert!(is_json(b" 3 "));
        assert!(!is_json(b"var a = 1;"));
        assert!(!is_json(b"{\"a\": 1,}"));
        assert!(!is_json(b""));
    }

    #[test]
    fn image_patterns() {
        assert_eq!(
            image_type_pattern(b"\x89PNG\r\n\x1A\n...."),
            Some("image/png")
        );
        assert_eq!(image_type_pattern(b"\xFF\xD8\xFF\xE0"), Some("image/jpeg"));
        assert_eq!(image_type_pattern(b"GIF89a"), Some("image/gif"));
        assert_eq!(
            image_type_pattern(b"RIFF\x10\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_type_pattern(b"BM"), Some("image/bmp"));
        assert_eq!(
            image_type_pattern(b"\x00\x00\x01\x00"),
            Some("image/x-icon")
        );
        // AVIF by its major brand, by a compatible brand, and not an MP4's ftyp box.
        let avif = b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00mif1avifmiaf";
        assert_eq!(image_type_pattern(avif), Some("image/avif"));
        let avis = b"\x00\x00\x00\x18ftypmsf1\x00\x00\x00\x00msf1avis";
        assert_eq!(image_type_pattern(avis), Some("image/avif"));
        let mp4 = b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00mp42";
        assert_eq!(image_type_pattern(mp4), None);
        assert_eq!(image_type_pattern(&avif[..20]), None);
        assert_eq!(image_type_pattern(b"<html>"), None);
        assert_eq!(image_type_pattern(b"\x89PN"), None);
    }

    #[test]
    fn audio_and_video_patterns() {
        assert_eq!(
            audio_or_video_type_pattern(b"ID3\x04\x00"),
            Some("audio/mpeg")
        );
        assert_eq!(
            audio_or_video_type_pattern(b"OggS\x00\x02"),
            Some("application/ogg")
        );
        assert_eq!(
            audio_or_video_type_pattern(b"RIFF\x24\x08\x00\x00WAVEfmt "),
            Some("audio/wave")
        );
        // MP4: a 20-byte ftyp box with the major brand `isom` and the compatible brand `mp42`.
        let mp4 = b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00mp42more";
        assert_eq!(audio_or_video_type_pattern(mp4), Some("video/mp4"));
        let not_mp4 = b"\x00\x00\x00\x14ftypqt  \x00\x00\x02\x00qt  more";
        assert_eq!(audio_or_video_type_pattern(not_mp4), None);
        // WebM: the EBML header, then DocType (42 82) with a one-byte size and `webm`.
        let webm = b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01\x42\x82\x84webm\x42\x87\x81\x02";
        assert_eq!(audio_or_video_type_pattern(webm), Some("video/webm"));
        let mkv = b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01\x42\x82\x88matroska\x42\x87";
        assert_eq!(audio_or_video_type_pattern(mkv), None);
        // MP3 without ID3: MPEG-1 layer III, 128 kbit/s, 44.1 kHz, no padding: 417-byte frames.
        let mut mp3 = vec![0u8; 900];
        for at in [0, 417] {
            mp3[at] = 0xFF;
            mp3[at + 1] = 0xFB;
            mp3[at + 2] = 0x90;
        }
        assert_eq!(audio_or_video_type_pattern(&mp3), Some("audio/mpeg"));
        mp3[417] = 0;
        assert_eq!(audio_or_video_type_pattern(&mp3), None);
        assert_eq!(audio_or_video_type_pattern(b"\x89PNG\r\n\x1A\n"), None);
    }
}
