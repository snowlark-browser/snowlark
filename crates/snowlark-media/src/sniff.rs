// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MIME Sniffing §7.2 "rules for sniffing audio and video specifically", for media served without a
//! usable `Content-Type` (`application/octet-stream`, none, or a wrong one).

/// The media type the first bytes of a resource show, if they match a known signature.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    // §6.2.1 "matches the signature for MP4": a box size, then "ftyp" and a brand starting "mp4"
    // (or another ISO BMFF brand among the compatible ones).
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if size >= 8 && size.is_multiple_of(4) && bytes.len() >= size.min(bytes.len()) {
            let brands = &bytes[8..size.min(bytes.len())];
            let audio = brands.chunks(4).any(|b| b == b"M4A " || b == b"M4B ");
            return Some(if audio { "audio/mp4" } else { "video/mp4" });
        }
    }
    // §6.2.2 WebM: the EBML header with a "webm" doc type.
    if bytes.len() >= 4 && bytes[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        let head = &bytes[..bytes.len().min(64)];
        if head.windows(4).any(|w| w == b"webm") {
            return Some("video/webm");
        }
    }
    if bytes.starts_with(b"OggS\0") {
        return Some("audio/ogg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    if bytes.starts_with(b"fLaC") {
        return Some("audio/flac");
    }
    if bytes.starts_with(b"ID3") {
        return Some("audio/mpeg");
    }
    // §6.2.3 MP3 without ID3: a frame sync.
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0 {
        // (ADTS AAC has layer 0)
        return Some(if bytes[1] & 0x06 == 0 {
            "audio/aac"
        } else {
            "audio/mpeg"
        });
    }
    None
}

/// The type to give the decoder: the response's type when it names a container Snowlark knows,
/// else what the bytes look like, else the `<source type>` hint.
pub fn effective_type(
    content_type: &str,
    hint: Option<&str>,
    first_bytes: &[u8],
) -> Option<String> {
    let known = |t: &str| {
        crate::caps::parse_type(t)
            .and_then(|p| p.container)
            .is_some()
    };
    if known(content_type) {
        return Some(content_type.to_owned());
    }
    if let Some(s) = sniff(first_bytes) {
        return Some(s.to_owned());
    }
    hint.filter(|h| known(h)).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypisom\0\0\x02\0isomiso2");
        assert_eq!(sniff(&mp4), Some("video/mp4"));
        assert_eq!(
            sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']),
            Some("video/webm")
        );
        assert_eq!(sniff(b"ID3\x04"), Some("audio/mpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(
            effective_type("application/octet-stream", Some("video/mp4"), b"xxxx"),
            Some("video/mp4".into())
        );
        assert_eq!(
            effective_type("video/webm", None, &mp4),
            Some("video/webm".into())
        );
        assert_eq!(
            effective_type("text/plain", None, &mp4),
            Some("video/mp4".into())
        );
    }
}
