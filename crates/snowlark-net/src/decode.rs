// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content codings (Fetch "handle content codings"): gzip, deflate (zlib, or raw as some servers send it),
//! br and zstd, decoded incrementally as body chunks arrive. A stack of codings ("gzip, br") is undone in
//! reverse order. An unknown coding leaves the body as it is.

use std::io::{self, Write};

use flate2::write::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};

enum Deflate {
    /// Waiting for the first two bytes to tell zlib from raw deflate.
    Undecided(Vec<u8>),
    Zlib(ZlibDecoder<Vec<u8>>),
    Raw(DeflateDecoder<Vec<u8>>),
}

enum Stage {
    Gzip(MultiGzDecoder<Vec<u8>>),
    Deflate(Deflate),
    Brotli(Option<Box<brotli_decompressor::DecompressorWriter<Vec<u8>>>>),
    Zstd(zstd::stream::write::Decoder<'static, Vec<u8>>),
}

fn is_zlib_header(b: &[u8]) -> bool {
    b.len() >= 2
        && (b[0] & 0x0F) == 8
        && (b[0] >> 4) <= 7
        && ((b[0] as u16) << 8 | b[1] as u16).is_multiple_of(31)
}

impl Stage {
    fn write(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Stage::Gzip(d) => {
                d.write_all(input)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Stage::Deflate(state) => {
                if let Deflate::Undecided(buffer) = state {
                    buffer.extend_from_slice(input);
                    if buffer.len() < 2 {
                        return Ok(Vec::new());
                    }
                    let buffered = std::mem::take(buffer);
                    *state = if is_zlib_header(&buffered) {
                        Deflate::Zlib(ZlibDecoder::new(Vec::new()))
                    } else {
                        Deflate::Raw(DeflateDecoder::new(Vec::new()))
                    };
                    return Stage::write_deflate(state, &buffered);
                }
                Stage::write_deflate(state, input)
            }
            Stage::Brotli(d) => {
                let d = d.as_mut().expect("brotli stage used after finish");
                d.write_all(input)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Stage::Zstd(d) => {
                d.write_all(input)?;
                d.flush()?;
                Ok(std::mem::take(d.get_mut()))
            }
        }
    }

    fn write_deflate(state: &mut Deflate, input: &[u8]) -> io::Result<Vec<u8>> {
        match state {
            Deflate::Zlib(d) => {
                d.write_all(input)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Deflate::Raw(d) => {
                d.write_all(input)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Deflate::Undecided(_) => Ok(Vec::new()),
        }
    }

    /// The rest of the output at the end of the stream. A truncated stream gives what it has (browsers
    /// render a page whose gzip trailer was cut off).
    fn finish(&mut self) -> io::Result<Vec<u8>> {
        match self {
            Stage::Gzip(d) => {
                let _ = d.try_finish();
                Ok(std::mem::take(d.get_mut()))
            }
            Stage::Deflate(state) => {
                if let Deflate::Undecided(buffer) = state {
                    let buffered = std::mem::take(buffer);
                    if buffered.is_empty() {
                        return Ok(Vec::new());
                    }
                    *state = Deflate::Raw(DeflateDecoder::new(Vec::new()));
                    Stage::write_deflate(state, &buffered)?;
                }
                match state {
                    Deflate::Zlib(d) => {
                        let _ = d.try_finish();
                        Ok(std::mem::take(d.get_mut()))
                    }
                    Deflate::Raw(d) => {
                        let _ = d.try_finish();
                        Ok(std::mem::take(d.get_mut()))
                    }
                    Deflate::Undecided(_) => Ok(Vec::new()),
                }
            }
            Stage::Brotli(d) => match d.take() {
                Some(mut d) => {
                    let _ = d.flush();
                    Ok(match d.into_inner() {
                        Ok(v) | Err(v) => v,
                    })
                }
                None => Ok(Vec::new()),
            },
            Stage::Zstd(d) => {
                d.flush()?;
                Ok(std::mem::take(d.get_mut()))
            }
        }
    }
}

/// An incremental decoder for a response's content codings.
pub(crate) struct ContentDecoder {
    stages: Vec<Stage>,
}

impl ContentDecoder {
    /// A decoder for `Content-Encoding` values in the order they were applied; `None` when there is nothing
    /// to undo, or a coding we don't know (then the body passes through unchanged).
    pub fn for_codings(codings: &[String]) -> io::Result<Option<ContentDecoder>> {
        let mut stages = Vec::new();
        for coding in codings.iter().rev() {
            let stage = match coding.trim().to_ascii_lowercase().as_str() {
                "" | "identity" => continue,
                "gzip" | "x-gzip" => Stage::Gzip(MultiGzDecoder::new(Vec::new())),
                "deflate" => Stage::Deflate(Deflate::Undecided(Vec::new())),
                "br" => Stage::Brotli(Some(Box::new(
                    brotli_decompressor::DecompressorWriter::new(Vec::new(), 16 * 1024),
                ))),
                "zstd" => Stage::Zstd(zstd::stream::write::Decoder::new(Vec::new())?),
                _ => return Ok(None),
            };
            stages.push(stage);
        }
        Ok((!stages.is_empty()).then_some(ContentDecoder { stages }))
    }

    /// Decode the next chunk of the body.
    pub fn push(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        let mut data = input.to_vec();
        for stage in &mut self.stages {
            if data.is_empty() {
                break;
            }
            data = stage.write(&data)?;
        }
        Ok(data)
    }

    /// The end of the body.
    pub fn finish(&mut self) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        for stage in &mut self.stages {
            if !data.is_empty() {
                data = stage.write(&data)?;
            }
            data.extend(stage.finish()?);
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;

    fn decode_chunked(codings: &[&str], encoded: &[u8], chunk: usize) -> Vec<u8> {
        let codings: Vec<String> = codings.iter().map(|s| s.to_string()).collect();
        let mut d = ContentDecoder::for_codings(&codings).unwrap().unwrap();
        let mut out = Vec::new();
        for piece in encoded.chunks(chunk) {
            out.extend(d.push(piece).unwrap());
        }
        out.extend(d.finish().unwrap());
        out
    }

    fn text() -> Vec<u8> {
        b"Hello, Snowlark! ".repeat(500)
    }

    #[test]
    fn gzip() {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(&text()).unwrap();
        let encoded = e.finish().unwrap();
        assert_eq!(decode_chunked(&["gzip"], &encoded, 7), text());
        assert_eq!(decode_chunked(&["x-gzip"], &encoded, 4096), text());
    }

    #[test]
    fn deflate_both_framings() {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&text()).unwrap();
        assert_eq!(
            decode_chunked(&["deflate"], &z.finish().unwrap(), 1),
            text()
        );
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), Compression::default());
        raw.write_all(&text()).unwrap();
        assert_eq!(
            decode_chunked(&["deflate"], &raw.finish().unwrap(), 3),
            text()
        );
    }

    #[test]
    fn zstd_and_stacked() {
        let z = zstd::encode_all(&text()[..], 3).unwrap();
        assert_eq!(decode_chunked(&["zstd"], &z, 11), text());
        // "gzip, zstd": gzip first, then zstd
        let mut g = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(&text()).unwrap();
        let both = zstd::encode_all(&g.finish().unwrap()[..], 3).unwrap();
        assert_eq!(decode_chunked(&["gzip", "zstd"], &both, 5), text());
    }

    #[test]
    fn brotli() {
        let mut e = brotli::CompressorWriter::new(Vec::new(), 4096, 9, 22);
        e.write_all(&text()).unwrap();
        let encoded = e.into_inner();
        assert_eq!(decode_chunked(&["br"], &encoded, 13), text());
    }

    #[test]
    fn unknown_or_identity() {
        assert!(ContentDecoder::for_codings(&["identity".into()])
            .unwrap()
            .is_none());
        assert!(ContentDecoder::for_codings(&["compress".into()])
            .unwrap()
            .is_none());
    }
}
