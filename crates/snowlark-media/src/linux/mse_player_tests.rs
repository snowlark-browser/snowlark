// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The player's reading of MSE track buffers, from a real fragmented MP4 and the system's FFmpeg
//! (skipped without it): the decoders from the initialization segment, where a seek starts, what
//! is pending and what ends, and a decoded picture.

use super::*;
use crate::linux::ffmpeg::{Decoded, Frame};

const TYPE: &str = "video/mp4; codecs=\"avc1.42E01E\"";

fn clip() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pattern-frag.mp4"
    ))
    .expect("the fixture")
}

/// The clip split at its first `moof`: the initialization segment and the media segments.
fn parts() -> (Vec<u8>, Vec<u8>) {
    let c = clip();
    let moof = c.windows(4).position(|w| w == b"moof").unwrap() - 4;
    (c[..moof].to_vec(), c[moof..].to_vec())
}

fn ff() -> Option<&'static ffmpeg::Ffmpeg> {
    let ff = ffmpeg::get();
    if ff.is_none() {
        eprintln!("no FFmpeg on this system: skipped");
    }
    ff
}

/// An open source with one buffer and the initialization segment appended.
fn source() -> (MseSource, u32) {
    let ms = MseSource::new().unwrap();
    ms.open();
    let id = ms.add_source_buffer(TYPE).unwrap();
    ms.append(id, &parts().0).unwrap();
    (ms, id)
}

/// The next packet's (stream, pts, key), "pending" or "end".
fn next(r: &mut MseReader) -> Result<(usize, i64, bool), &'static str> {
    match r.next() {
        Next::Packet(p) => Ok((p.stream(), p.pts(), p.key())),
        Next::Pending => Err("pending"),
        Next::End => Err("end"),
    }
}

/// Every packet up to the first that is not one.
fn drain(r: &mut MseReader) -> (Vec<(usize, i64, bool)>, &'static str) {
    let mut out = Vec::new();
    loop {
        match next(r) {
            Ok(p) => out.push(p),
            Err(e) => return (out, e),
        }
    }
}

#[test]
fn the_player_opens_once_every_buffer_has_its_initialization_segment_and_the_duration() {
    let ms = MseSource::new().unwrap();
    ms.open();
    assert!(!ready_to_open(&ms), "no buffers");
    let id = ms.add_source_buffer(TYPE).unwrap();
    let second = ms.add_source_buffer(TYPE).unwrap();
    ms.append(id, &parts().0).unwrap();
    ms.set_duration(30.0);
    assert!(!ready_to_open(&ms), "the second buffer has none");
    ms.append(second, &parts().0).unwrap();
    assert!(ready_to_open(&ms));
    let ms = source().0;
    assert!(!ready_to_open(&ms), "the page has not set the duration");
    ms.set_duration(f64::INFINITY);
    assert!(ready_to_open(&ms));
}

#[test]
fn the_decoders_come_from_the_initialization_segment() {
    let Some(ff) = ff() else { return };
    let (ms, _) = source();
    let o = open(ff, ms).unwrap();
    let v = o.video.as_ref().expect("the clip's video");
    assert_eq!((v.width, v.height, v.index), (160, 90, VIDEO));
    assert!(o.audio.is_none(), "the clip has no sound");
}

#[test]
fn frames_go_out_in_decode_order_and_the_source_is_pending_until_it_ends() {
    let Some(ff) = ff() else { return };
    let (ms, id) = source();
    let mut r = open(ff, ms.clone()).unwrap().reader;
    assert_eq!(next(&mut r), Err("pending"), "nothing appended");
    ms.append(id, &parts().1).unwrap();
    let (packets, after) = drain(&mut r);
    assert_eq!(after, "pending", "not ended: more may come");
    assert!(packets.len() > 10, "{packets:?}");
    assert_eq!(packets[0], (VIDEO, 0, true), "from the first key frame");
    // (decode order: the next appended frames come after these)
    ms.end_of_stream(None);
    assert_eq!(next(&mut r), Err("end"));
}

#[test]
fn a_seek_starts_at_the_key_frame_before_its_time() {
    let Some(ff) = ff() else { return };
    let (ms, id) = source();
    ms.append(id, &parts().1).unwrap();
    let mut r = open(ff, ms).unwrap().reader;
    r.seek(3.5);
    let (packets, _) = drain(&mut r);
    // (the clip's key frames are 3 s apart)
    assert_eq!(packets[0], (VIDEO, 30_000_000, true), "{packets:?}");
    let all = {
        r.seek(0.0);
        drain(&mut r).0
    };
    assert!(packets.len() < all.len());
}

/// Data that starts within a second of zero is the stream's start: playback from zero starts
/// there; data that starts later waits for the page to seek into it.
#[test]
fn data_that_starts_just_after_zero_plays_from_its_start() {
    let Some(ff) = ff() else { return };
    for (offset, first) in [(0.5, Ok((VIDEO, 5_000_000, true))), (1.5, Err("pending"))] {
        let (ms, id) = source();
        ms.set_timestamp_offset(id, offset);
        ms.append(id, &parts().1).unwrap();
        let mut r = open(ff, ms).unwrap().reader;
        assert_eq!(next(&mut r), first, "data from {offset} s");
    }
}

#[test]
fn a_seek_past_the_buffered_data_waits_for_an_append_there() {
    let Some(ff) = ff() else { return };
    let (ms, id) = source();
    let media = parts().1;
    ms.append(id, &media).unwrap();
    let mut r = open(ff, ms.clone()).unwrap().reader;
    r.seek(20.0);
    assert_eq!(next(&mut r), Err("pending"));
    ms.set_timestamp_offset(id, 20.0);
    ms.append(id, &media).unwrap();
    assert_eq!(next(&mut r), Ok((VIDEO, 200_000_000, true)));
    // An ended source: a seek past its data is the end.
    ms.end_of_stream(None);
    r.seek(500.0);
    assert_eq!(next(&mut r), Err("end"));
}

#[test]
fn a_removal_behind_the_position_does_not_move_it_and_one_ahead_skips_to_a_key_frame() {
    let Some(ff) = ff() else { return };
    let (ms, id) = source();
    ms.append(id, &parts().1).unwrap();
    let mut r = open(ff, ms.clone()).unwrap().reader;
    r.seek(3.5);
    let first = next(&mut r).unwrap();
    assert_eq!(first.1, 30_000_000);
    let second = next(&mut r).unwrap();
    ms.remove(id, 0.0, 3.0).unwrap();
    let third = next(&mut r).unwrap();
    assert!(
        third.1 != second.1 && !third.2,
        "on with the GOP: {third:?}"
    );
    // The rest of this GOP goes (to the end: the clip's last key frame is at 3 s), and the clip
    // comes again 220 ms later (frames are 40 ms apart: none at the position): the frames after the position are then the end of its first
    // GOP, which cannot be decoded without its key frame, so the reader goes on from the next
    // key frame.
    ms.remove(id, 3.0, 3.1).unwrap();
    assert_eq!(drain(&mut r), (vec![], "pending"));
    ms.set_timestamp_offset(id, 0.22);
    ms.append(id, &parts().1).unwrap();
    assert_eq!(next(&mut r), Ok((VIDEO, 32_200_000, true)));
}

#[test]
fn the_packets_decode_to_pictures() {
    let Some(ff) = ff() else { return };
    let (ms, id) = source();
    ms.append(id, &parts().1).unwrap();
    let o = open(ff, ms.clone()).unwrap();
    let (mut r, mut v) = (o.reader, o.video.unwrap());
    let mut frame = Frame::new(ff);
    let mut pictures = 0;
    ms.end_of_stream(None);
    loop {
        let pkt = match r.next() {
            Next::Packet(p) => Some(p),
            _ => None,
        };
        v.decoder.send(pkt.as_ref()).unwrap();
        while let Decoded::Frame = v.decoder.receive(&mut frame).unwrap() {
            assert_eq!((frame.width(), frame.height()), (160, 90));
            pictures += 1;
        }
        if pkt.is_none() {
            break;
        }
    }
    assert!(pictures > 10, "{pictures} pictures");
}
