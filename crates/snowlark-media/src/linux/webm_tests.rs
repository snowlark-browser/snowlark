// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WebM byte stream reader, on streams written here: the initialization segment's tracks,
//! block times and durations, clusters in parts and of unknown size, and what is not a WebM byte
//! stream.

use super::*;

/// An element: its id, an 8-byte size, the body.
pub(crate) fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut v: Vec<u8> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|&b| b == 0)
        .collect();
    v.push(0x01);
    v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    v.extend_from_slice(body);
    v
}

/// An element of unknown size (its body follows).
fn unknown(id: u32) -> Vec<u8> {
    let mut v: Vec<u8> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|&b| b == 0)
        .collect();
    v.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    v
}

fn uint_el(id: u32, v: u64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// A track entry: number, type, codec id, more fields.
pub(crate) fn entry(number: u64, kind: u64, codec: &str, more: &[Vec<u8>]) -> Vec<u8> {
    let mut body = cat(&[
        uint_el(TRACK_NUMBER, number),
        uint_el(TRACK_TYPE, kind),
        el(CODEC_ID, codec.as_bytes()),
    ]);
    for m in more {
        body.extend_from_slice(m);
    }
    el(TRACK_ENTRY, &body)
}

/// An Opus identification header (RFC 7845 §5.1): one channel, 48 kHz.
pub(crate) fn opus_head() -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.extend_from_slice(&[1, 1, 0x38, 0x01]);
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&[0, 0, 0]);
    h
}

/// The EBML header, a segment of unknown size, `Info` (1 ms timecodes) and `Tracks` with these
/// entries.
pub(crate) fn init_segment(entries: &[Vec<u8>]) -> Vec<u8> {
    cat(&[
        el(EBML_HEADER, &el(0x4282, b"webm")),
        unknown(SEGMENT),
        el(SEEK_HEAD, &[0; 6]),
        el(INFO, &uint_el(TIMECODE_SCALE, 1_000_000)),
        el(TRACKS, &entries.concat()),
    ])
}

/// A VP9 video track 1 and an Opus sound track 2.
pub(crate) fn av_init() -> Vec<u8> {
    init_segment(&[
        entry(
            1,
            1,
            "V_VP9",
            &[el(
                VIDEO,
                &cat(&[uint_el(PIXEL_WIDTH, 320), uint_el(PIXEL_HEIGHT, 240)]),
            )],
        ),
        entry(2, 2, "A_OPUS", &[el(0x63A2, &opus_head())]),
    ])
}

/// A simple block of `track` at `timecode` (relative to the cluster), key or not, with `data`.
pub(crate) fn simple(track: u8, timecode: i16, key: bool, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&timecode.to_be_bytes());
    b.push(if key { 0x80 } else { 0 });
    b.extend_from_slice(data);
    el(SIMPLE_BLOCK, &b)
}

/// A 20 ms Opus packet (CELT full band, one frame: silence).
pub(crate) const OPUS_20MS: [u8; 3] = [0xF8, 0xFF, 0xFE];

pub(crate) fn cluster(time: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = uint_el(TIMECODE, time);
    body.extend_from_slice(&blocks.concat());
    el(CLUSTER, &body)
}

/// (track, time, duration, key) of each frame; the tracks' ids for an initialization segment.
fn frames(out: &[Output]) -> Vec<(u32, i64, i64, bool)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Sample(t, s) => {
                assert_eq!(s.time, s.decode);
                Some((*t, s.time, s.duration, s.key))
            }
            Output::Tracks(_) => None,
        })
        .collect()
}

const MS: i64 = 10_000;

#[test]
fn the_initialization_segment_gives_the_first_video_and_audio_track() {
    let mut r = WebmReader::new();
    let init = init_segment(&[
        entry(1, 1, "V_VP9", &[]),
        entry(2, 1, "V_VP8", &[]),
        entry(3, 0x11, "D_WEBVTT/SUBTITLES", &[]),
        entry(4, 2, "A_OPUS", &[]),
    ]);
    let out = r.push(&init);
    let [Output::Tracks(t)] = &out[..] else {
        panic!("{out:?}");
    };
    let got: Vec<_> = t.iter().map(|t| (t.id, t.video, t.codec.clone())).collect();
    assert_eq!(
        got,
        [
            (1, true, Codec::Other(*b"vp09")),
            (4, false, Codec::Other(*b"Opus"))
        ]
    );
    assert!(!r.failed());
    assert!(r.init().starts_with(&[0x1A, 0x45, 0xDF, 0xA3]));
    assert!(r.init().ends_with(&EMPTY_CLUSTER));
    // Blocks of the ignored tracks are dropped; one of no track is an error.
    let out = r.push(&cluster(
        0,
        &[
            simple(2, 0, true, &[1]),
            simple(3, 0, true, &[1]),
            simple(1, 0, true, &[1]),
        ],
    ));
    assert_eq!(frames(&out), [(1, 0, 63 * MS, true)]);
    assert!(!r.failed());
    r.push(&cluster(100, &[simple(9, 0, true, &[1])]));
    assert!(r.failed());
}

#[test]
fn a_codec_webm_does_not_carry_is_named_as_none() {
    let mut r = WebmReader::new();
    let out = r.push(&init_segment(&[entry(1, 1, "V_MPEG4/ISO/AVC", &[])]));
    let [Output::Tracks(t)] = &out[..] else {
        panic!("{out:?}");
    };
    assert_eq!(t[0].codec, Codec::Other([0; 4]));
}

#[test]
fn block_times_and_durations() {
    let mut r = WebmReader::new();
    r.push(&av_init());
    let out = r.push(&cluster(
        1000,
        &[
            simple(1, 0, true, &[0xAA]),
            simple(2, 0, true, &OPUS_20MS),
            simple(2, 20, true, &OPUS_20MS),
            simple(1, 40, false, &[0xBB]),
            simple(1, 80, false, &[0xCC]),
        ],
    ));
    // Opus by its table of contents at once; video by the next block, the last at the cluster's
    // end with the longest seen (40 ms).
    assert_eq!(
        frames(&out),
        [
            (2, 1000 * MS, 20 * MS, true),
            (2, 1020 * MS, 20 * MS, true),
            (1, 1000 * MS, 40 * MS, true),
            (1, 1040 * MS, 40 * MS, false),
            (1, 1080 * MS, 40 * MS, false),
        ]
    );
    let Output::Sample(_, s) = &out[2] else {
        panic!()
    };
    assert_eq!(s.data, [0xAA]);
    // A track with no duration seen gets the estimate.
    let mut r = WebmReader::new();
    r.push(&av_init());
    let out = r.push(&cluster(0, &[simple(1, 0, true, &[1])]));
    assert_eq!(frames(&out), [(1, 0, 63 * MS, true)]);
}

#[test]
fn a_block_group_has_its_duration_and_is_a_key_frame_unless_it_references_one() {
    let mut r = WebmReader::new();
    r.push(&av_init());
    let block = |tc: i16| {
        let mut b = vec![0x81];
        b.extend_from_slice(&tc.to_be_bytes());
        b.extend_from_slice(&[0, 7]);
        el(BLOCK, &b)
    };
    let out = r.push(&cluster(
        0,
        &[
            el(BLOCK_GROUP, &cat(&[block(0), uint_el(BLOCK_DURATION, 33)])),
            el(
                BLOCK_GROUP,
                &cat(&[
                    block(33),
                    uint_el(BLOCK_DURATION, 33),
                    el(REFERENCE_BLOCK, &[0xDF]),
                ]),
            ),
        ],
    ));
    assert_eq!(
        frames(&out),
        [(1, 0, 33 * MS, true), (1, 33 * MS, 33 * MS, false)]
    );
    // Two blocks in a group: an error.
    r.push(&cluster(
        100,
        &[el(BLOCK_GROUP, &cat(&[block(0), block(1)]))],
    ));
    assert!(r.failed());
}

#[test]
fn the_default_duration_and_the_timecode_scale() {
    let mut r = WebmReader::new();
    let init = cat(&[
        el(EBML_HEADER, &el(0x4282, b"webm")),
        unknown(SEGMENT),
        el(INFO, &uint_el(TIMECODE_SCALE, 100_000)),
        el(
            TRACKS,
            &entry(1, 1, "V_VP8", &[uint_el(DEFAULT_DURATION, 41_666_666)]),
        ),
    ]);
    r.push(&init);
    let out = r.push(&cluster(10, &[simple(1, 5, true, &[1])]));
    // (10 + 5) × 0.1 ms; 41.666666 ms
    assert_eq!(frames(&out), [(1, 15_000, 416_666, true)]);
}

#[test]
fn a_cluster_in_parts_hands_out_its_blocks_as_they_come() {
    let mut r = WebmReader::new();
    r.push(&av_init());
    let c = cluster(
        0,
        &[
            simple(1, 0, true, &[1; 50]),
            simple(1, 30, false, &[2; 50]),
            simple(1, 60, false, &[3; 50]),
        ],
    );
    let mut got = Vec::new();
    let mut seen = Vec::new();
    for chunk in c.chunks(7) {
        got.extend(frames(&r.push(chunk)));
        seen.push(got.len());
    }
    assert_eq!(
        got,
        [
            (1, 0, 30 * MS, true),
            (1, 30 * MS, 30 * MS, false),
            (1, 60 * MS, 30 * MS, false)
        ]
    );
    // (the first frame before the cluster's last bytes came)
    assert!(seen.contains(&1));
    assert!(!r.failed());
}

#[test]
fn a_cluster_of_unknown_size_ends_at_the_next_cluster() {
    let mut r = WebmReader::new();
    r.push(&av_init());
    let mut c = unknown(CLUSTER);
    c.extend(uint_el(TIMECODE, 0));
    c.extend(simple(1, 0, true, &[1]));
    c.extend(simple(1, 25, false, &[2]));
    let out = r.push(&c);
    // (the last block waits: the cluster may go on)
    assert_eq!(frames(&out), [(1, 0, 25 * MS, true)]);
    let out = r.push(&cluster(50, &[simple(1, 0, true, &[3])]));
    assert_eq!(
        frames(&out),
        [(1, 25 * MS, 25 * MS, false), (1, 50 * MS, 25 * MS, true)]
    );
    assert_eq!(r.segments(), 2);
    // Cues and a new initialization segment after a cluster.
    let out = r.push(&cat(&[el(CUES, &[0; 4]), av_init()]));
    assert!(matches!(&out[..], [Output::Tracks(_)]));
    assert!(!r.failed());
}

#[test]
fn reset_forgets_a_partial_element_and_the_held_back_frame() {
    let mut r = WebmReader::new();
    r.push(&av_init());
    let c = cluster(
        0,
        &[simple(1, 0, true, &[1]), simple(1, 30, false, &[2; 40])],
    );
    let out = r.push(&c[..c.len() - 10]);
    assert_eq!(frames(&out), []);
    assert!(!r.at_boundary());
    r.reset();
    assert!(r.at_boundary());
    let out = r.push(&cluster(100, &[simple(1, 0, true, &[3])]));
    assert_eq!(frames(&out), [(1, 100 * MS, 63 * MS, true)]);
}

#[test]
fn what_is_not_a_webm_byte_stream() {
    let fails = |bytes: Vec<u8>| {
        let mut r = WebmReader::new();
        r.push(&bytes);
        r.failed()
    };
    // a cluster before an initialization segment
    assert!(fails(cluster(0, &[simple(1, 0, true, &[1])])));
    // an ISO BMFF box
    assert!(fails(b"\0\0\0\x10ftypisom\0\0\0\0".to_vec()));
    // `Tracks` before `Info`
    assert!(fails(cat(&[
        unknown(SEGMENT),
        el(TRACKS, &entry(1, 1, "V_VP9", &[]))
    ])));
    let after_init = |blocks: &[Vec<u8>]| {
        let mut r = WebmReader::new();
        r.push(&av_init());
        r.push(&cluster(0, blocks));
        r.failed()
    };
    // laced (Xiph lacing flags)
    assert!(after_init(&[el(
        SIMPLE_BLOCK,
        &[0x81, 0, 0, 0x82, 1, 2, 3, 4]
    )]));
    // a block before the cluster's timecode
    let mut r = WebmReader::new();
    r.push(&av_init());
    r.push(&el(CLUSTER, &simple(1, 0, true, &[1])));
    assert!(r.failed());
    // a timecode going back in the cluster, across tracks
    assert!(after_init(&[
        simple(1, 10, true, &[1]),
        simple(2, 5, true, &OPUS_20MS)
    ]));
    // a track number above 127
    assert!(after_init(&[el(
        SIMPLE_BLOCK,
        &[0x40, 0x81, 0, 0, 0x80, 1]
    )]));
    // and a well formed one is not
    assert!(!after_init(&[
        simple(1, 10, true, &[1]),
        simple(2, 10, true, &OPUS_20MS)
    ]));
}

#[test]
fn opus_packet_durations() {
    // SILK 10 ms, code 0; CELT 2.5 ms, two frames; code 3 with 6 frames of 20 ms
    assert_eq!(opus_duration(&[0x00]), Some(10 * MS));
    assert_eq!(opus_duration(&[0x81]), Some(5 * MS));
    assert_eq!(opus_duration(&[0xFB, 0x06]), Some(120 * MS));
    assert_eq!(opus_duration(&[0xFB]), None);
    assert_eq!(opus_duration(&[]), None);
}

/// A WebM `SourceBuffer` through the media source, and the player's decoders from its
/// initialization segment (with the system's FFmpeg).
#[test]
fn a_webm_source_buffer_buffers_and_its_sound_decodes() {
    use crate::linux::ffmpeg::{self, Decoded, Frame};
    use crate::linux::mse::MseSource;
    use crate::linux::player::{Next, Packets};
    use crate::mse::MseEvent;

    let ms = MseSource::new().unwrap();
    ms.open();
    let id = ms.add_source_buffer("audio/webm; codecs=\"opus\"").unwrap();
    let init = init_segment(&[entry(1, 2, "A_OPUS", &[el(0x63A2, &opus_head())])]);
    ms.append(id, &init).unwrap();
    let blocks: Vec<Vec<u8>> = (0..50)
        .map(|i| simple(1, i * 20, true, &OPUS_20MS))
        .collect();
    ms.append(id, &cluster(0, &blocks)).unwrap();
    let events = ms.take_events();
    assert!(!events.contains(&MseEvent::Error(id)), "{events:?}");
    let r = ms.buffered(id);
    assert_eq!(r.as_slice(), [(0.0, 1.0)]);
    // A codec WebM does not carry: an append error.
    let other = ms.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    ms.append(other, &init_segment(&[entry(1, 1, "V_MPEG4/ISO/AVC", &[])]))
        .unwrap();
    assert!(ms.take_events().contains(&MseEvent::Error(other)));
    ms.remove_source_buffer(other);

    let Some(ff) = ffmpeg::get() else {
        eprintln!("no FFmpeg on this system: skipped");
        return;
    };
    ms.set_duration(1.0);
    ms.end_of_stream(None);
    let o = crate::linux::mse_player::open(ff, ms.clone()).unwrap();
    assert!(o.video.is_none());
    let (mut r, mut a) = (o.reader, o.audio.unwrap());
    let mut frame = Frame::new(ff);
    let mut decoded = 0;
    loop {
        let pkt = match r.next() {
            Next::Packet(p) => Some(p),
            _ => None,
        };
        a.decoder.send(pkt.as_ref()).unwrap();
        while let Decoded::Frame = a.decoder.receive(&mut frame).unwrap() {
            decoded += 1;
        }
        if pkt.is_none() {
            break;
        }
    }
    assert!(decoded >= 45, "{decoded} frames of sound");
}
