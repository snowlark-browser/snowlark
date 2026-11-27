// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The track buffers of MSE on Linux, from a real fragmented MP4: what `buffered` says after
//! appends in any pieces, under a timestamp offset, an append window and sequence mode, after
//! removals, and the events each operation reports.

use super::*;

const TYPE: &str = "video/mp4; codecs=\"avc1.42E01E\"";

fn clip() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pattern-frag.mp4"
    ))
    .expect("the fixture")
}

/// An open source with one buffer.
fn source() -> (MseSource, u32) {
    let ms = MseSource::new().unwrap();
    assert_eq!(ms.ready_state(), "closed");
    assert_eq!(
        ms.add_source_buffer(TYPE),
        Err(AddError::InvalidState),
        "closed"
    );
    ms.open();
    assert_eq!(ms.take_events(), [MseEvent::Opened]);
    let id = ms.add_source_buffer(TYPE).unwrap();
    (ms, id)
}

fn ranges(ms: &MseSource, id: u32) -> Vec<(f64, f64)> {
    ms.buffered(id)
        .as_slice()
        .iter()
        .map(|&(s, e)| ((s * 1000.0).round() / 1000.0, (e * 1000.0).round() / 1000.0))
        .collect()
}

/// The clip's whole buffered range (from one append).
fn whole() -> (f64, f64) {
    let (ms, id) = source();
    ms.append(id, &clip()).unwrap();
    let r = ranges(&ms, id);
    assert_eq!(r.len(), 1, "one range: {r:?}");
    assert!(r[0].1 - r[0].0 > 0.5, "some time: {r:?}");
    r[0]
}

#[test]
fn an_append_buffers_the_clip_and_reports_update_then_updateend() {
    let (ms, id) = source();
    assert!(ms.buffered(id).is_empty());
    ms.append(id, &clip()).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Update(id), MseEvent::UpdateEnd(id)]
    );
    assert!(!ms.updating(id));
    let r = ranges(&ms, id);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].0.abs() < 0.1, "from the start: {r:?}");
}

#[test]
fn appends_in_pieces_buffer_the_same() {
    let all = whole();
    for piece in [1000, 4096, 77] {
        let (ms, id) = source();
        for c in clip().chunks(piece) {
            ms.append(id, c).unwrap();
        }
        assert_eq!(ranges(&ms, id), [all], "pieces of {piece}");
    }
}

#[test]
fn the_timestamp_offset_moves_the_frames() {
    let all = whole();
    let (ms, id) = source();
    ms.set_timestamp_offset(id, 10.0);
    ms.append(id, &clip()).unwrap();
    assert_eq!(ranges(&ms, id), [(all.0 + 10.0, all.1 + 10.0)]);
}

#[test]
fn the_append_window_drops_frames_outside_it() {
    let all = whole();
    let (ms, id) = source();
    let mid = (all.0 + all.1) / 2.0;
    ms.set_append_window(id, 0.0, mid);
    ms.append(id, &clip()).unwrap();
    let r = ranges(&ms, id);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        r[0].1 <= mid + 1e-6 && r[0].1 > all.0,
        "ends by {mid}: {r:?}"
    );
}

#[test]
fn removal_cuts_to_the_next_random_access_point_and_to_the_end() {
    let all = whole();
    let (ms, id) = source();
    ms.append(id, &clip()).unwrap();
    ms.take_events();
    // (from the end: the frames from `start` on go)
    let start = all.0 + (all.1 - all.0) * 0.5;
    ms.remove(id, start, f64::INFINITY).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Update(id), MseEvent::UpdateEnd(id)]
    );
    let r = ranges(&ms, id);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].1 <= start + 1e-6 && r[0].1 > all.0, "{r:?}");
    ms.remove(id, 0.0, f64::INFINITY).unwrap();
    assert!(ms.buffered(id).is_empty());
}

#[test]
fn an_overlapping_append_replaces_rather_than_adds() {
    let all = whole();
    let (ms, id) = source();
    ms.append(id, &clip()).unwrap();
    let frames = |ms: &MseSource| {
        ms.state().buffers[0]
            .track_buffers
            .iter()
            .map(|t| t.frames.len())
            .sum::<usize>()
    };
    let once = frames(&ms);
    ms.append(id, &clip()).unwrap();
    assert_eq!(
        frames(&ms),
        once,
        "the same frames again replace themselves"
    );
    assert_eq!(ranges(&ms, id), [all]);
}

#[test]
fn sequence_mode_places_a_second_append_after_the_first() {
    let all = whole();
    let len = all.1 - all.0;
    let (ms, id) = source();
    ms.set_sequence_mode(id, true);
    ms.append(id, &clip()).unwrap();
    ms.append(id, &clip()).unwrap();
    let r = ranges(&ms, id);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        (r[0].1 - r[0].0 - 2.0 * len).abs() < 0.05,
        "twice as long: {r:?}"
    );
}

#[test]
fn a_media_segment_first_is_an_error() {
    let bytes = clip();
    // (the media segments without the initialization segment: from the first `moof`)
    let at = bytes.windows(4).position(|w| w == b"moof").expect("a moof") - 4;
    let (ms, id) = source();
    ms.append(id, &bytes[at..]).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Error(id), MseEvent::UpdateEnd(id)]
    );
}

/// MSE §3.5.7: an initialization segment whose track has a codec this browser does not take
/// (here the sample entry renamed `zzzz`, as WPT's `invalid-codec.mp4`) is an append error.
#[test]
fn an_initialization_segment_with_an_unknown_codec_is_an_error() {
    let mut bytes = clip();
    let stsd = bytes
        .windows(4)
        .position(|w| w == b"stsd")
        .expect("an stsd");
    let entry = stsd + bytes[stsd..].windows(4).position(|w| w == b"avc1").unwrap();
    bytes[entry..entry + 4].copy_from_slice(b"zzzz");
    let (ms, id) = source();
    ms.append(id, &bytes).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Error(id), MseEvent::UpdateEnd(id)]
    );
    assert!(ms.buffered(id).is_empty());
}

/// The segment parser loop meets bytes that are not a top-level box (a media segment copied
/// into the middle of another, as WPT's `mediasource-errors.html` corrupts one): an append error.
#[test]
fn bytes_that_are_not_a_box_are_an_error() {
    let bytes = clip();
    let moof = bytes.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let (ms, id) = source();
    ms.append(id, &bytes[..moof]).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Update(id), MseEvent::UpdateEnd(id)]
    );
    let copy = bytes[moof..].to_vec();
    let mut corrupt = copy.clone();
    let at = (corrupt.len() - 1) / 2;
    let n = corrupt.len() - at;
    corrupt[at..].copy_from_slice(&copy[..n]);
    ms.append(id, &corrupt).unwrap();
    assert_eq!(
        ms.take_events(),
        [MseEvent::Error(id), MseEvent::UpdateEnd(id)]
    );
}

#[test]
fn the_end_of_stream_sets_the_duration_and_an_append_opens_again() {
    let all = whole();
    let (ms, id) = source();
    ms.append(id, &clip()).unwrap();
    ms.end_of_stream(None);
    assert_eq!(ms.ready_state(), "ended");
    assert!((ms.duration() - all.1).abs() < 0.001, "{}", ms.duration());
    ms.append(id, &[]).unwrap();
    assert_eq!(ms.ready_state(), "open");
}

#[test]
fn webm_buffers_are_not_supported_yet() {
    let ms = MseSource::new().unwrap();
    ms.open();
    assert_eq!(
        ms.add_source_buffer("video/webm; codecs=\"vp9\""),
        Err(AddError::NotSupported)
    );
}
