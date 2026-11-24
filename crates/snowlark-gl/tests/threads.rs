// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Services on several threads at once: every page thread of a process has its own, and they all
//! reach the process's one ANGLE display and its Direct3D 11 device.

use std::sync::mpsc;
use std::time::Duration;

use snowlark_gl::{Attributes, Backend, Command as C, Query as Q, Reply, Service};

const COLOR: u32 = 0x4000;
const RGBA: u32 = 0x1908;
const UNSIGNED_BYTE: u32 = 0x1401;

/// Clears, presents and reads back on `threads` threads at once, each in its own color, and checks
/// every pixel each one gets; within `limit`, or the test fails instead of hanging the binary.
fn race(warp: bool, threads: u32, rounds: u32, limit: Duration) {
    let (done, finished) = mpsc::channel();
    for t in 0..threads {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut s =
                Service::new(warp).expect("ANGLE (dev.ps1 copies it beside the test binary)");
            let a = Attributes {
                antialias: false,
                preserve_drawing_buffer: t % 2 == 0,
                ..Attributes::default()
            };
            let color = [(t * 40 % 256) as u8, 255 - (t * 25 % 256) as u8, 128, 255];
            let f = color.map(|c| c as f32 / 255.0);
            let mut wrong = None;
            for r in 0..rounds {
                // a context made and destroyed on every round, and one kept
                for id in [1, 2 + r] {
                    if id != 1 || r == 0 {
                        s.create_context(id, &a, 8 + t, 4).unwrap();
                    }
                    s.submit(vec![(id, C::ClearColor(f)), (id, C::Clear(COLOR))]);
                    let rb = s.present(id).unwrap();
                    let shown = &rb.rgba[..4];
                    let read = match s.query(id, &Q::ReadPixels(1, 1, 1, 1, RGBA, UNSIGNED_BYTE, 4))
                    {
                        Reply::Bytes(b) => b,
                        other => panic!("readPixels: {other:?}"),
                    };
                    let kept = if a.preserve_drawing_buffer {
                        color
                    } else {
                        [0; 4]
                    };
                    if shown != color || read != kept {
                        wrong.get_or_insert((t, r, shown.to_vec(), read));
                    }
                    if id != 1 {
                        s.destroy(id);
                    }
                }
            }
            done.send(wrong).unwrap();
        });
    }
    drop(done);
    let deadline = std::time::Instant::now() + limit;
    for _ in 0..threads {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match finished.recv_timeout(left) {
            Ok(None) => {}
            Ok(Some((t, r, shown, read))) => {
                panic!("thread {t} round {r}: presented {shown:?}, read {read:?}")
            }
            Err(_) => panic!("services on {threads} threads did not finish in {limit:?}"),
        }
    }
}

#[test]
fn services_on_several_threads_at_once() {
    race(true, 8, 40, Duration::from_secs(60));
}

#[test]
fn services_on_several_threads_at_once_on_the_graphics_card() {
    // (the card's driver, where a page's contexts normally run; WARP where there is none)
    race(false, 8, 40, Duration::from_secs(60));
}
