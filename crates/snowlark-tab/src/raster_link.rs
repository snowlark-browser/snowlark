// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The raster seam as messages: what the encode thread (the renderer's) and the raster thread (the
//! browser's) say to each other, in the format a process boundary will carry
//! (`docs/process-isolation.md`, "One frame" and M4's first step).
//!
//! Each message is a head, which goes through the wire format (`snowlark_ipc::wire`, at most a pipe
//! frame), and bytes beside it: a job's lists, an image's pixels, a web font's data. Across
//! processes those bytes will be in a shared section; in one process the vector itself moves. Every
//! head is decoded and checked against its bytes by a `join` that trusts neither, so what reaches
//! the raster thread from another process is no more trusted than a display list is.
//!
//! In one process the threads hand each other values, as before. With `SNOWLARK_IPC=loopback`, [`cross`]
//! puts every message through the format on the sender's thread first (as `snowlark_ipc::loopback`
//! does for the tab's commands and events), and a message refused on the way is dropped as a raster
//! side in another process would drop it, after `[ipc] loopback:` says why.
//!
//! The compositor's recycled frames do not cross: the compositor and the raster thread are both the
//! browser's (`TiledFrame` holds the device's tiles).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snowlark_ipc::Error;
use snowlark_paint::{Refused, Resource, ResourceHead};

use crate::raster_thread::{JobHead, RasterJob};

/// A message of the raster seam: a head for the wire and bytes beside it.
pub(crate) trait Message: Sized {
    type Head: Serialize + DeserializeOwned;
    /// The head and the bytes that travel beside it (empty when it has none).
    fn split(self) -> (Self::Head, Vec<u8>);
    /// The message `head` and `bytes` make, if they are a well-formed one.
    fn join(head: Self::Head, bytes: Vec<u8>) -> Result<Self, Refused>;
}

/// Encode side to raster side, in order: what the tables need, then the job that needs it.
#[derive(Debug, PartialEq)]
pub(crate) enum ToRaster {
    Resource(Resource),
    Job(Box<RasterJob>),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ToRasterHead {
    Resource(ResourceHead),
    Job(JobHead),
}

impl Message for ToRaster {
    type Head = ToRasterHead;

    fn split(self) -> (ToRasterHead, Vec<u8>) {
        match self {
            ToRaster::Resource(r) => {
                let (head, bytes) = r.split();
                (ToRasterHead::Resource(head), bytes)
            }
            ToRaster::Job(job) => {
                let RasterJob { head, lists } = *job;
                (ToRasterHead::Job(head), lists)
            }
        }
    }

    fn join(head: ToRasterHead, bytes: Vec<u8>) -> Result<ToRaster, Refused> {
        match head {
            ToRasterHead::Resource(head) => Resource::join(head, bytes).map(ToRaster::Resource),
            ToRasterHead::Job(head) => {
                RasterJob::join(head, bytes).map(|j| ToRaster::Job(j.into()))
            }
        }
    }
}

impl ToRaster {
    /// [`Message::join`], with why it was refused as words.
    pub(crate) fn join_head(head: ToRasterHead, bytes: Vec<u8>) -> Result<ToRaster, String> {
        ToRaster::join(head, bytes).map_err(|e| e.to_string())
    }
}

/// Raster side to encode side: the raster thread has decoded a job's lists, so the buffer they
/// came in is free to fill again (across processes, the mailbox slot it was in). The bytes beside it
/// are that buffer, whatever it holds.
#[derive(Debug, PartialEq)]
pub(crate) struct Taken(pub Vec<u8>);

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum FromRasterHead {
    Taken,
}

impl Message for Taken {
    type Head = FromRasterHead;

    fn split(self) -> (FromRasterHead, Vec<u8>) {
        (FromRasterHead::Taken, self.0)
    }

    fn join(head: FromRasterHead, bytes: Vec<u8>) -> Result<Taken, Refused> {
        match head {
            FromRasterHead::Taken => Ok(Taken(bytes)),
        }
    }
}

/// `message` as the wire carries it: the encoded head, and the bytes beside it.
pub(crate) fn encode<M: Message>(message: M) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let (head, bytes) = message.split();
    Ok((snowlark_ipc::wire::encode(&head)?, bytes))
}

/// The message an encoded head and the bytes beside it make, decoded and checked.
pub(crate) fn decode<M: Message>(head: &[u8], bytes: Vec<u8>) -> Result<M, Error> {
    let head = snowlark_ipc::wire::decode(head)?;
    M::join(head, bytes).map_err(|e| Error::BadMessage(e.to_string()))
}

/// `message` as the far end of a pipe gets it: its head framed, cut out of the stream, decoded,
/// and joined to its bytes again.
pub(crate) fn pass<M: Message>(message: M) -> Result<M, Error> {
    use snowlark_ipc::frame::{self, FrameReader, Kind};
    let (head, bytes) = encode(message)?;
    let mut reader = FrameReader::new();
    reader.push(&frame::encode(Kind::Message, 0, &head)?);
    let frame = reader.next_frame()?.ok_or(Error::BadFrame("no frame"))?;
    decode(&frame.payload, bytes)
}

/// How many messages [`cross`] has put through the format (for tests that must know it ran).
#[cfg(test)]
pub(crate) static CROSSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `message` as it is, or with `SNOWLARK_IPC=loopback` through [`pass`]: `None` when it is refused
/// on the way.
pub(crate) fn cross<M: Message>(message: M) -> Option<M> {
    if !snowlark_ipc::loopback::enabled() {
        return Some(message);
    }
    match pass(message) {
        Ok(message) => {
            #[cfg(test)]
            CROSSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(message)
        }
        Err(e) => {
            eprintln!(
                "[ipc] loopback: {} does not cross: {e}",
                std::any::type_name::<M>()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use snowlark_paint::{
        Color, DisplayList, DisplayListBuilder, FontFile, FontId, FontSource, ImageId, LayerKind,
        LayerState, Point, Rect, Size, StateOp, StickyConstraint, Viewport, MAX_LIST_BYTES,
    };

    use snowlark_paint::layer_animation::{
        AnimatedValues, Direction, Easing, LayerAnimation, StepPosition, TransformFn,
    };
    use snowlark_paint::Ticks;

    use super::*;
    use crate::driver::{ColorScheme, ScrollbarColors};
    use crate::raster_thread::FrameMeta;

    fn list(n: usize) -> Vec<u8> {
        let mut b = DisplayListBuilder::new();
        for i in 0..n {
            b.fill_rect(Rect::new(i as f32, 0.0, 4.0, 4.0), Color::rgb(10, 20, 30));
        }
        let mut bytes = Vec::new();
        b.build().encode(&mut bytes);
        bytes
    }

    fn sticky() -> LayerKind {
        LayerKind::Sticky(StickyConstraint {
            rect: Rect::new(0.0, 10.0, 100.0, 20.0),
            container: Rect::new(0.0, 0.0, 100.0, 900.0),
            insets: [Some(0.0), None, None, Some(-4.5)],
        })
    }

    /// A compositor animation of the composited layer below: a turn a second, forever.
    fn spinner() -> LayerAnimation {
        LayerAnimation {
            element: u64::MAX - 3,
            origin: Ticks::now(),
            start: 1.5,
            playback_rate: 1.25,
            delay: 0.25,
            duration: 1.0,
            iterations: f64::INFINITY,
            iteration_start: 0.5,
            direction: Direction::Alternate,
            easing: Easing::Steps(4, StepPosition::JumpStart),
            epsilon: Some(1e-6),
            fill_backwards: false,
            fill_forwards: true,
            offsets: vec![0.0, 0.5, 1.0],
            easings: vec![
                Easing::CubicBezier([0.25, 0.1, 0.25, 1.0]),
                Easing::Steps(3, StepPosition::JumpBoth),
                Easing::Piecewise(vec![[0.0, 0.0], [0.5, 0.75], [1.0, 1.0]]),
            ],
            values: AnimatedValues::Transform {
                lists: vec![
                    vec![TransformFn::Translate(0.0, -2.0), TransformFn::Rotate(0.0)],
                    vec![],
                    vec![TransformFn::Translate(4.0, 0.0), TransformFn::Rotate(6.25)],
                ],
                pre: [1.0, 0.0, 0.0, 1.0, -16.0, -16.0],
                post: [1.0, 0.0, 0.0, 1.0, 16.0, 16.0],
            },
        }
    }

    fn fade() -> LayerAnimation {
        LayerAnimation {
            values: AnimatedValues::Opacity(vec![0.0, 1.0, 0.25]),
            direction: Direction::Normal,
            iterations: 2.5,
            ..spinner()
        }
    }

    /// A job of a base list and two layers, as the encode thread makes one.
    fn job() -> RasterJob {
        let (base, fixed, stuck, turned) = (list(3), list(1), list(2), list(1));
        let head = JobHead {
            base_len: base.len(),
            layers: vec![
                (Some(LayerKind::Fixed), LayerState::default(), fixed.len()),
                (Some(sticky()), LayerState::default(), stuck.len()),
                (
                    Some(LayerKind::Composited(u64::MAX - 3)),
                    LayerState(vec![
                        StateOp::Clip(Rect::new(0.0, 0.0, 50.0, 60.0)),
                        StateOp::Transform([0.0, 1.0, -1.0, 0.0, 32.5, 0.0]),
                        StateOp::ClipRounded(Rect::new(0.0, 0.0, 9.0, 9.0), [4.5; 8]),
                        StateOp::Opacity(0.25),
                        StateOp::Animated(u64::MAX - 3),
                    ]),
                    turned.len(),
                ),
            ],
            view: Viewport {
                width: 1200,
                height: 2400,
                scale: 1.5,
                background: Color::WHITE,
            },
            viewport: Size::new(800.0, 600.0),
            base_at: (0, -300),
            meta: FrameMeta {
                origin: Point::new(0.0, 125.5),
                scale: 1.5,
                content: Size::new(800.0, 4000.0),
                scrollbar: 17.0,
                color_scheme: ColorScheme::Dark,
                scrollbar_colors: Some(ScrollbarColors {
                    thumb: Color::rgb(1, 2, 3),
                    track: Color::rgb(4, 5, 6),
                }),
                background: Color::rgb(30, 30, 30),
                generation: 12,
                number: 345,
                restore: Some(Point::new(0.0, 900.0)),
                scroll: Some(crate::comp::PageScroll {
                    to: Point::new(0.0, 1200.0),
                    serial: 7,
                    by: Some(Point::new(0.0, 40.0)),
                }),
            },
            band: (-200.0, 1400.0),
            pinned: true,
            encode_ms: 0.25,
            animations: vec![spinner(), fade()],
        };
        RasterJob {
            head,
            lists: [base, fixed, stuck, turned].concat(),
        }
    }

    fn resources() -> Vec<Resource> {
        let font = |file| FontSource {
            file,
            index: 0,
            simulations: 2,
            axes: vec![(u32::from_be_bytes(*b"wdth"), 87.5)],
        };
        let path = r"C:\Windows\Fonts\segoeui.ttf".encode_utf16().collect();
        vec![
            Resource::FontAdd {
                id: FontId(0),
                source: font(FontFile::System {
                    path,
                    last_write: 7,
                }),
            },
            Resource::FontAdd {
                id: FontId(4),
                source: font(FontFile::Data(vec![0, 1, 0, 0, 5])),
            },
            Resource::ImageAdd {
                id: ImageId(2),
                frame: 1,
                version: 99,
                width: 2,
                height: 2,
                pixels: (0..16).collect(),
            },
            Resource::ImageRemove(ImageId(2)),
            Resource::ImagesClear,
        ]
    }

    /// One message of each kind there is: the `match` has no wildcard, so a new kind does not
    /// compile until it has a sample here.
    fn samples() -> Vec<ToRaster> {
        let mut out: Vec<ToRaster> = resources().into_iter().map(ToRaster::Resource).collect();
        out.push(ToRaster::Job(Box::new(job())));
        let mut sampled = [false; 6];
        for m in &out {
            let kind = match m {
                ToRaster::Resource(Resource::FontAdd { source, .. }) => match source.file {
                    FontFile::System { .. } => 0,
                    FontFile::Data(_) => 1,
                },
                ToRaster::Resource(Resource::ImageAdd { .. }) => 2,
                ToRaster::Resource(Resource::ImageRemove(_)) => 3,
                ToRaster::Resource(Resource::ImagesClear) => 4,
                ToRaster::Job(_) => 5,
            };
            sampled[kind] = true;
        }
        assert!(
            sampled.iter().all(|s| *s),
            "a kind with no sample: {sampled:?}"
        );
        out
    }

    fn clone(m: &ToRaster) -> ToRaster {
        match m {
            ToRaster::Resource(r) => ToRaster::Resource(r.clone()),
            ToRaster::Job(j) => ToRaster::Job(Box::new(RasterJob {
                head: j.head.clone(),
                lists: j.lists.clone(),
            })),
        }
    }

    /// A deterministic stream of numbers.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 24
        }
    }

    #[test]
    fn every_message_crosses_the_wire_and_comes_back() {
        for m in samples() {
            assert_eq!(pass(clone(&m)).unwrap(), m);
            let (head, bytes) = encode(clone(&m)).unwrap();
            assert_eq!(decode::<ToRaster>(&head, bytes).unwrap(), m);
            // (a message is a pipe frame at most, whatever its bytes beside it are; the head
            // grows with the frame's compositor animations, which `fit_budget` holds to a part of
            // a frame)
            assert!(head.len() < 1200, "{} bytes", head.len());
        }
        let buffer = Taken(vec![7; 5000]);
        let (head, bytes) = encode(Taken(buffer.0.clone())).unwrap();
        assert_eq!(head.len(), 1);
        assert_eq!(decode::<Taken>(&head, bytes).unwrap(), buffer);
        assert_eq!(pass(Taken(buffer.0.clone())).unwrap(), buffer);
    }

    /// `fit_budget` counts an animation at no less than what it takes on the wire.
    #[test]
    fn an_animation_takes_no_more_than_its_wire_bound() {
        let mut big = spinner();
        if let AnimatedValues::Transform { lists, .. } = &mut big.values {
            for l in lists.iter_mut() {
                *l = vec![TransformFn::Rotate(1.0); snowlark_paint::layer_animation::MAX_LIST];
            }
        }
        big.easings[0] = Easing::Piecewise(vec![[0.5, 0.5]; 200]);
        for a in [spinner(), fade(), big] {
            let bytes = snowlark_ipc::wire::encode(&a).unwrap().len();
            assert!(bytes <= a.wire_bytes(), "{bytes} > {}", a.wire_bytes());
        }
    }

    #[test]
    fn a_cut_short_or_trailing_head_is_refused() {
        for m in samples() {
            let (head, bytes) = encode(m).unwrap();
            for len in 0..head.len() {
                assert!(
                    decode::<ToRaster>(&head[..len], bytes.clone()).is_err(),
                    "{len} of {}",
                    head.len()
                );
            }
            let mut long = head.clone();
            long.push(0);
            assert!(decode::<ToRaster>(&long, bytes).is_err());
        }
        assert!(decode::<Taken>(&[], Vec::new()).is_err());
        assert!(decode::<Taken>(&[1], Vec::new()).is_err());
    }

    /// What a raster side takes after decoding: what it would have to draw with.
    fn takes(m: &ToRaster) {
        match m {
            ToRaster::Resource(Resource::ImageAdd {
                width,
                height,
                pixels,
                ..
            }) => assert_eq!(pixels.len() as u64, *width as u64 * *height as u64 * 4),
            ToRaster::Resource(Resource::FontAdd { source, .. }) => {
                assert!(source.axes.iter().all(|(_, v)| v.is_finite()));
                if let FontFile::System { path, .. } = &source.file {
                    assert!(!path.is_empty() && !path.contains(&0));
                }
            }
            ToRaster::Resource(_) => {}
            ToRaster::Job(job) => {
                // Its lists split as the head says, with no panic.
                let mut lists = Vec::new();
                let _ = crate::raster_thread::decode(job, &mut lists);
                assert!(job.head.view.scale.is_finite() && job.head.view.scale > 0.0);
            }
        }
    }

    /// Corrupted heads and bytes beside them: decoding never panics, and anything it takes is a
    /// message a raster side can draw with, which encodes again to itself.
    #[test]
    fn corrupted_messages_are_refused_or_well_formed() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let (mut taken, mut refused) = (0, 0);
        for m in samples() {
            let (head, bytes) = encode(m).unwrap();
            for _ in 0..2000 {
                let mut bad = head.clone();
                let mut beside = bytes.clone();
                for _ in 0..1 + rng.next() % 3 {
                    let at = rng.next() as usize % bad.len();
                    match rng.next() % 4 {
                        0 => bad[at] = rng.next() as u8,
                        1 => bad[at] ^= 1 << (rng.next() % 8),
                        2 => bad[at] = bad[at].wrapping_add(1),
                        _ => beside
                            .truncate(beside.len().saturating_sub(1 + rng.next() as usize % 4)),
                    }
                }
                match decode::<ToRaster>(&bad, beside) {
                    Ok(m) => {
                        takes(&m);
                        let again = clone(&m);
                        assert_eq!(pass(again).unwrap(), m);
                        taken += 1;
                    }
                    Err(_) => refused += 1,
                }
            }
        }
        // (both outcomes were reached, so the test exercised the checks and the decoding)
        assert!(
            taken > 100 && refused > 100,
            "{taken} taken, {refused} refused"
        );
    }

    fn joined(change: impl FnOnce(&mut JobHead, &mut Vec<u8>)) -> Result<RasterJob, &'static str> {
        let RasterJob {
            mut head,
            mut lists,
        } = job();
        change(&mut head, &mut lists);
        RasterJob::join(head, lists).map_err(|e| e.0)
    }

    #[test]
    fn corrupt_jobs_are_refused() {
        let lengths = Some("lists not as long as the job says");
        assert!(joined(|_, _| ()).is_ok());
        assert_eq!(joined(|h, _| h.base_len += 1).err(), lengths);
        assert_eq!(joined(|_, l| l.push(0)).err(), lengths);
        assert_eq!(joined(|h, _| h.layers[1].2 = usize::MAX).err(), lengths);
        assert_eq!(joined(|h, _| h.base_len = usize::MAX).err(), lengths);
        // (an empty layer is a list that does not decode: the raster thread refuses it then)
        assert!(joined(|h, _| h.layers.push((None, LayerState::default(), 0))).is_ok());
        let bad_number: [fn(&mut JobHead); 9] = [
            |h| h.view.scale = 0.0,
            |h| h.view.scale = f32::NAN,
            |h| h.viewport.width = -1.0,
            |h| h.band.1 = f32::INFINITY,
            |h| h.meta.origin.x = f32::NAN,
            |h| h.meta.scale = f32::NEG_INFINITY,
            |h| h.meta.content.height = f32::NAN,
            |h| h.meta.restore = Some(Point::new(0.0, f32::NAN)),
            |h| h.encode_ms = f64::NAN,
        ];
        for (i, change) in bad_number.iter().enumerate() {
            assert!(
                matches!(
                    joined(|h, _| change(h)),
                    Err("a number a frame cannot have")
                ),
                "{i}"
            );
        }
        assert!(matches!(
            joined(|h, _| h.view.height = 512 * 2000),
            Err("a frame larger than a raster side takes")
        ));
        assert!(matches!(
            joined(|h, l| {
                l.resize(MAX_LIST_BYTES + 1, 0);
                h.base_len = l.len() - h.layers.iter().map(|(_, _, n)| n).sum::<usize>();
            }),
            Err("a frame larger than a raster side takes")
        ));
        assert!(matches!(
            joined(
                |h, _| h.layers[1].0 = Some(LayerKind::Sticky(StickyConstraint {
                    rect: Rect::new(f32::NAN, 0.0, 1.0, 1.0),
                    container: Rect::ZERO,
                    insets: [None; 4],
                }))
            ),
            Err("a layer's kind")
        ));
        // A composited layer's state with a number it cannot be drawn through.
        assert!(joined(|h, _| {
            h.layers[1].0 = Some(LayerKind::Composited(9));
            h.layers[1].1 = LayerState(vec![StateOp::Transform([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])]);
        })
        .is_ok());
        for bad in [
            StateOp::Transform([1.0, 0.0, 0.0, f32::NAN, 0.0, 0.0]),
            StateOp::Opacity(2.0),
            StateOp::Clip(Rect::new(0.0, f32::INFINITY, 1.0, 1.0)),
            StateOp::ClipRounded(Rect::new(0.0, 0.0, 1.0, 1.0), [-1.0; 8]),
            StateOp::Filter(
                snowlark_paint::ColorFilters::of(&[snowlark_paint::FilterOp::Grayscale(2.0)])
                    .unwrap(),
            ),
        ] {
            assert_eq!(
                joined(|h, _| h.layers[1].1 = LayerState(vec![bad])).err(),
                Some("a layer's state")
            );
        }
        // An animation the compositor could not sample.
        let bad_animation: [fn(&mut LayerAnimation); 8] = [
            |a| a.duration = f64::INFINITY,
            |a| a.playback_rate = 0.0,
            // (backwards over infinite iterations)
            |a| a.playback_rate = -1.0,
            |a| a.iteration_start = f64::NAN,
            |a| a.easing = Easing::CubicBezier([0.5, 0.0, 1.5, 1.0]),
            |a| a.offsets[1] = 2.0,
            |a| a.easings[1] = Easing::Steps(0, StepPosition::JumpEnd),
            |a| {
                if let AnimatedValues::Transform { lists, .. } = &mut a.values {
                    lists[0][1] = TransformFn::Scale(1.0, f32::NAN);
                }
            },
        ];
        for (i, change) in bad_animation.iter().enumerate() {
            assert_eq!(
                joined(|h, _| change(&mut h.animations[0])).err(),
                Some("an animation a compositor cannot sample"),
                "{i}"
            );
        }
        // A job that passed is one its lists decode from.
        let ok = joined(|_, _| ()).unwrap();
        let mut lists = vec![DisplayList::new()];
        crate::raster_thread::decode(&ok, &mut lists).unwrap();
        assert_eq!(lists.len(), 4);
        assert_eq!(lists[2].len(), 2);
    }
}
