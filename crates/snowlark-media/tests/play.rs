// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Playback end to end: the fixture clip served over HTTP with range requests, fetched through
//! snowlark-net, decoded by Media Foundation into GPU textures (Windows only: docs/linux.md stage 7).

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use snowlark_media::resource::{Head, MediaResource};
use snowlark_media::{Player, PlayerEvent, PlayerOptions, Source};
use snowlark_net::{NetConfig, NetThread, Request, Url};
use snowlark_paint::video::VideoStore;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pattern.mp4")
}

/// Writes the fixtures when `SNOWLARK_WRITE_FIXTURE=1` (once, by hand: the clips are checked in).
#[test]
fn write_fixture() {
    if std::env::var("SNOWLARK_WRITE_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    let path = fixture();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    snowlark_media::win::encode::write_test_clip(&path, 320, 180, 4, 25, true).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > 1000);
    // Video only, fragmented: for Media Source Extensions.
    let frag = path.with_file_name("pattern-frag.mp4");
    snowlark_media::win::encode::write_fragmented_test_clip(&frag, 160, 90, 4, 25, false).unwrap();
    assert!(std::fs::metadata(&frag).unwrap().len() > 1000);
}

/// An HTTP/1.1 server for one body that answers `Range: bytes=N-` with 206. Returns the port and
/// a count of the ranged requests.
fn serve(body: Vec<u8>, content_type: &'static str, ranges: bool) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ranged = Arc::new(AtomicUsize::new(0));
    let r2 = ranged.clone();
    let body = Arc::new(body);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let body = body.clone();
            let ranged = r2.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut start: Option<usize> = None;
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 {
                            return;
                        }
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        if let Some(v) = h
                            .strip_prefix("Range: bytes=")
                            .or(h.strip_prefix("range: bytes="))
                        {
                            start = v.trim_end_matches('-').parse().ok();
                        }
                    }
                    let head = match start.filter(|_| ranges) {
                        Some(s) => {
                            ranged.fetch_add(1, Ordering::SeqCst);
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Range: bytes {s}-{}/{}\r\nAccept-Ranges: bytes\r\n\r\n",
                                body.len() - s,
                                body.len() - 1,
                                body.len()
                            )
                        }
                        None => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        ),
                    };
                    let s = start.filter(|_| ranges).unwrap_or(0);
                    if out.write_all(head.as_bytes()).is_err() || out.write_all(&body[s..]).is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, ranged)
}

struct Wake(Mutex<bool>, Condvar);

impl Wake {
    fn new() -> Arc<Wake> {
        Arc::new(Wake(Mutex::new(false), Condvar::new()))
    }
    fn wait(&self, ms: u64) {
        let g = self.0.lock().unwrap();
        let (mut g, _) = self.1.wait_timeout(g, Duration::from_millis(ms)).unwrap();
        *g = false;
    }
}

fn resource(net: &NetThread, url: &str, wake: Arc<Wake>) -> MediaResource {
    let url = Url::parse(url).unwrap();
    MediaResource::open(
        net.client(),
        Arc::new(move || Request::new(url.clone())),
        Arc::new(move || {
            *wake.0.lock().unwrap() = true;
            wake.1.notify_all();
        }),
        None,
    )
}

#[test]
fn resources_read_with_range_requests() {
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let (port, ranged) = serve(body.clone(), "application/octet-stream", true);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let r = resource(&net, &format!("http://127.0.0.1:{port}/a"), Wake::new());
    match r.wait_head(Duration::from_secs(10)) {
        Head::Ok {
            length,
            ranges,
            status,
            ..
        } => {
            assert_eq!((length, ranges, status), (Some(3_000_000), true, 206));
        }
        h => panic!("{h:?}"),
    }
    let mut buf = [0u8; 16];
    assert_eq!(r.read_at(0, &mut buf).unwrap(), 16);
    assert_eq!(&buf[..], &body[..16]);
    // A read far ahead starts a second range request there.
    let n = r.read_at(2_900_000, &mut buf).unwrap();
    assert!(n > 0);
    assert_eq!(&buf[..n], &body[2_900_000..2_900_000 + n]);
    assert!(ranged.load(Ordering::SeqCst) >= 2);
    assert_eq!(r.read_at(3_000_000, &mut buf).unwrap(), 0);
}

#[test]
fn resources_from_servers_without_ranges_read_in_order() {
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 7) as u8).collect();
    let (port, _) = serve(body.clone(), "video/mp4", false);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let r = resource(&net, &format!("http://127.0.0.1:{port}/a"), Wake::new());
    let mut buf = vec![0u8; 1000];
    let n = r.read_at(150_000, &mut buf).unwrap();
    assert_eq!(&buf[..n], &body[150_000..150_000 + n]);
    assert!(matches!(r.head(), Head::Ok { ranges: false, .. }));
}

#[test]
fn errors_are_not_media() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Some(Ok(mut s)) = listener.incoming().next() {
            let mut b = [0u8; 1024];
            let _ = std::io::Read::read(&mut s, &mut b);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        }
    });
    let r = resource(&net, &format!("http://127.0.0.1:{port}/a"), Wake::new());
    assert!(matches!(
        r.wait_head(Duration::from_secs(10)),
        Head::Failed { network: false, .. }
    ));
}

#[test]
fn plays_the_fixture_into_gpu_frames() {
    let Ok(gpu) = snowlark_paint::Gpu::new() else {
        return;
    };
    let body = std::fs::read(fixture()).expect("the fixture clip");
    let (port, _) = serve(body, "video/mp4", true);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let wake = Wake::new();
    let videos = VideoStore::new(Some(gpu.clone()));
    let w2 = wake.clone();
    let player = Player::open(
        PlayerOptions {
            videos: Some(videos.clone()),
            wake: Arc::new(move || {
                *w2.0.lock().unwrap() = true;
                w2.1.notify_all();
            }),
            volume: 1.0,
            muted: true,
            rate: 1.0,
            audio: None,
        },
        Source::Resource {
            resource: resource(
                &net,
                &format!("http://127.0.0.1:{port}/clip.mp4"),
                wake.clone(),
            ),
            hint: None,
        },
    );
    let mut events = Vec::new();
    let until = |events: &mut Vec<PlayerEvent>, f: &dyn Fn(&PlayerEvent) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let new = player.take_events();
            let hit = new.iter().any(f);
            events.extend(new);
            if hit {
                return true;
            }
            wake.wait(50);
        }
        false
    };
    assert!(
        until(&mut events, &|e| matches!(e, PlayerEvent::Opened { .. })),
        "{events:?}"
    );
    let opened = events.iter().find_map(|e| match e {
        PlayerEvent::Opened {
            duration,
            width,
            height,
        } => Some((*duration, *width, *height)),
        _ => None,
    });
    let (duration, w, h) = opened.unwrap();
    assert_eq!((w, h), (320, 180));
    assert!((duration - 4.0).abs() < 0.2, "{duration}");
    // The first frame comes with the metadata (HTML's "loadeddata").
    assert!(
        events.contains(&PlayerEvent::Frame) || until(&mut events, &|e| *e == PlayerEvent::Frame),
        "{events:?}"
    );
    player.play();
    assert!(
        until(&mut events, &|e| *e == PlayerEvent::Playing),
        "{events:?}"
    );
    player.pause();
    player.seek(1.5);
    assert!(
        until(&mut events, &|e| *e == PlayerEvent::Seeked),
        "{events:?}"
    );
    // The frame of second 1 (green) shows once the copy is old enough.
    let slot = videos.get(player.video().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut green = false;
    while Instant::now() < deadline && !green {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(Some(f)) = slot.read_front(&gpu) {
            let p = f.pixel(10, 10);
            green = p.g > 150 && p.r < 80 && p.b < 80;
        }
    }
    assert!(green, "the frame at 1.5 s is the green second");
    player.play();
    assert!(
        until(&mut events, &|e| *e == PlayerEvent::Ended),
        "{events:?}"
    );
    player.close();
}

#[test]
fn seeks_while_paused_show_the_frame() {
    let Ok(gpu) = snowlark_paint::Gpu::new() else {
        return;
    };
    let body = std::fs::read(fixture()).expect("the fixture clip");
    let (port, _) = serve(body, "video/mp4", false);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let wake = Wake::new();
    let videos = VideoStore::new(Some(gpu.clone()));
    let w2 = wake.clone();
    let player = Player::open(
        PlayerOptions {
            videos: Some(videos.clone()),
            wake: Arc::new(move || {
                *w2.0.lock().unwrap() = true;
                w2.1.notify_all();
            }),
            volume: 1.0,
            muted: true,
            rate: 1.0,
            audio: None,
        },
        Source::Resource {
            resource: resource(
                &net,
                &format!("http://127.0.0.1:{port}/clip.mp4"),
                wake.clone(),
            ),
            hint: None,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|e| matches!(e, PlayerEvent::Opened { .. }))
    {
        events.extend(player.take_events());
        wake.wait(50);
    }
    // Played and paused at once (an autoplaying video paused by a script), then a seek while
    // paused: the frame there shows.
    player.play();
    player.pause();
    player.seek(2.5);
    let slot = videos.get(player.video().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut color = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(Some(f)) = slot.read_front(&gpu) {
            let p = f.pixel(10, 10);
            color = Some((p.r, p.g, p.b));
            // second 2: blue-ish (BGR 0xE0, 0x40, 0x20)
            if p.b > 150 && p.r < 80 {
                break;
            }
        }
    }
    let (r, _, b) = color.expect("a frame");
    assert!(b > 150 && r < 80, "{color:?} {:?}", player.take_events());
}

/// Which second of the pattern clip a pixel is from (its background colour), if any.
fn pattern_second(p: snowlark_paint::geom::Color) -> Option<u32> {
    (0..4).find(|&s| {
        let (b, g, r) = snowlark_media::win::encode::pattern_color(s);
        let near = |a: u8, e: u8| (a as i32 - e as i32).abs() < 48;
        near(p.b, b) && near(p.g, g) && near(p.r, r)
    })
}

/// A script that sets `currentTime` on every animation frame (a scroll-scrubbed video) seeks
/// again before the last seek finished. HTML §4.8.11.9: a new seek aborts the one running, so
/// only the last one ends in `seeked`, and the element shows the frame at the last target, never
/// one at a position given up. The player runs one seek at a time, keeps only the newest asked
/// for while one runs, and reports the seek done once the frame there is copied.
#[test]
fn a_burst_of_seeks_ends_once_at_the_last_frame() {
    let Ok(gpu) = snowlark_paint::Gpu::new() else {
        return;
    };
    let body = std::fs::read(fixture()).expect("the fixture clip");
    let (port, _) = serve(body, "video/mp4", false);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let wake = Wake::new();
    let videos = VideoStore::new(Some(gpu.clone()));
    let w2 = wake.clone();
    let player = Player::open(
        PlayerOptions {
            videos: Some(videos.clone()),
            wake: Arc::new(move || {
                *w2.0.lock().unwrap() = true;
                w2.1.notify_all();
            }),
            volume: 1.0,
            muted: true,
            rate: 1.0,
            audio: None,
        },
        Source::Resource {
            resource: resource(
                &net,
                &format!("http://127.0.0.1:{port}/clip.mp4"),
                wake.clone(),
            ),
            hint: None,
        },
    );
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !events.contains(&PlayerEvent::Frame) {
        events.extend(player.take_events());
        wake.wait(50);
    }
    assert!(events.contains(&PlayerEvent::Frame), "{events:?}");
    let slot = videos.get(player.video().unwrap()).unwrap();
    // The first frame (second 0) shows.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && slot
            .read_front(&gpu)
            .ok()
            .flatten()
            .map(|f| pattern_second(f.pixel(10, 10)))
            != Some(Some(0))
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Seeks one after the other, each before the last finished: seconds 1, 2, then 3.
    for t in [1.5, 2.5, 3.5] {
        player.seek(t);
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut seeked = 0;
    let mut seen = Vec::new();
    let mut last_seeked = None;
    // The second shown a little after the seek was reported done: the frame copied by then shows
    // once it is a copy's time old.
    let mut shown_after = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for e in player.take_events() {
            if e == PlayerEvent::Seeked {
                seeked += 1;
                last_seeked = Some(Instant::now());
            }
        }
        if let Ok(Some(f)) = slot.read_front(&gpu) {
            let s = pattern_second(f.pixel(10, 10));
            if seen.last() != Some(&s) {
                seen.push(s);
            }
            if shown_after.is_none()
                && last_seeked.is_some_and(|t| t.elapsed() > Duration::from_millis(100))
            {
                shown_after = Some(s);
            }
        }
        if last_seeked.is_some_and(|t| t.elapsed() > Duration::from_millis(800)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        seeked, 1,
        "one seeked for the burst; seconds shown {seen:?}"
    );
    assert_eq!(
        shown_after,
        Some(Some(3)),
        "the frame at the target was copied before the seek was reported done: {seen:?}"
    );
    assert_eq!(
        seen.last(),
        Some(&Some(3)),
        "the last target's frame shows: {seen:?}"
    );
    assert!(
        !seen.contains(&Some(1)) && !seen.contains(&Some(2)),
        "no frame of a seek given up shows: {seen:?}"
    );
}

/// Plays `body` (served over HTTP) in a tab's audio group that is muted (the tests make no sound;
/// the tab's mute is below what counts as audible).
fn play_in_tab(
    body: Vec<u8>,
) -> (
    Player,
    Arc<snowlark_media::AudioGroup>,
    Arc<Wake>,
    NetThread,
) {
    let (port, _) = serve(body, "video/mp4", true);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let wake = Wake::new();
    let group = snowlark_media::AudioGroup::new();
    group.set_muted(true);
    let w2 = wake.clone();
    let player = Player::open(
        PlayerOptions {
            videos: None,
            wake: Arc::new(move || {
                *w2.0.lock().unwrap() = true;
                w2.1.notify_all();
            }),
            volume: 1.0,
            muted: false,
            rate: 1.0,
            audio: Some(group.clone()),
        },
        Source::Resource {
            resource: resource(
                &net,
                &format!("http://127.0.0.1:{port}/clip.mp4"),
                wake.clone(),
            ),
            hint: None,
        },
    );
    (player, group, wake, net)
}

fn wait_until(wake: &Wake, secs: u64, f: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        wake.wait(20);
    }
    f()
}

#[test]
fn audible_follows_playback_and_the_elements_volume() {
    let body = std::fs::read(fixture()).expect("the fixture clip");
    let (player, group, wake, _net) = play_in_tab(body);
    let heard: Arc<Mutex<Vec<bool>>> = Arc::default();
    let h = heard.clone();
    group.set_notify(Some(Box::new(move |a| h.lock().unwrap().push(a))));
    assert!(wait_until(&wake, 20, || player
        .take_events()
        .iter()
        .any(|e| matches!(e, PlayerEvent::Opened { .. }))));
    assert!(!group.audible(), "not before playing");
    player.play();
    assert!(
        wait_until(&wake, 5, || group.audible()),
        "playing with sound"
    );
    // The element muted, then at volume zero: not audible.
    player.set_volume(1.0, true);
    assert!(wait_until(&wake, 2, || !group.audible()), "element muted");
    player.set_volume(1.0, false);
    assert!(wait_until(&wake, 2, || group.audible()));
    player.set_volume(0.0, false);
    assert!(wait_until(&wake, 2, || !group.audible()), "volume 0");
    player.set_volume(0.5, false);
    assert!(wait_until(&wake, 2, || group.audible()));
    // The tab's mute does not change it (the tab shows the muted speaker).
    group.set_muted(false);
    group.set_muted(true);
    assert!(group.audible());
    player.pause();
    assert!(wait_until(&wake, 2, || !group.audible()), "paused");
    player.play();
    assert!(wait_until(&wake, 2, || group.audible()));
    // Closing the player stops it counting.
    player.close();
    drop(player);
    assert!(wait_until(&wake, 2, || !group.audible()), "closed");
    let heard = heard.lock().unwrap().clone();
    assert!(heard.len() >= 8, "{heard:?}");
    assert!(heard.windows(2).all(|w| w[0] != w[1]), "{heard:?}");
}

#[test]
fn video_without_sound_is_not_audible() {
    let dir = std::env::temp_dir().join(format!("snowlark-media-silent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("silent.mp4");
    snowlark_media::win::encode::write_test_clip(&path, 160, 90, 3, 25, false).unwrap();
    let body = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let (player, group, wake, _net) = play_in_tab(body);
    assert!(wait_until(&wake, 20, || player
        .take_events()
        .iter()
        .any(|e| matches!(e, PlayerEvent::Opened { .. }))));
    player.play();
    let playing = || player.status().playing;
    assert!(wait_until(&wake, 5, playing), "it plays");
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        assert!(!group.audible(), "a video without an audio track");
        wake.wait(20);
    }
    player.close();
}
