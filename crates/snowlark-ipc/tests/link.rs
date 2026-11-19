// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Links between two children (`snowlark_ipc::link`), both ends in this process over a real pipe: the
//! browser's ends are loopback ones, whose "child" is this process.

use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_ipc::frame::{self, Kind};
use snowlark_ipc::{
    connect_link, loopback, BadMessage, Bulk, ClientEnd, Error, HostEnd, ProcessKind,
    ProcessPolicy, Received, Validate, INLINE_MAX,
};

const WAIT: Duration = Duration::from_secs(20);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum Msg {
    Ping(u64),
    Bytes(u32, Bulk),
}

impl Validate for Msg {
    fn validate(self, _: &ProcessPolicy) -> Result<Msg, BadMessage> {
        Ok(self)
    }
}

type Host = HostEnd<Msg, Msg>;
type Client = ClientEnd<Msg, Msg>;

fn policy() -> ProcessPolicy {
    ProcessPolicy {
        process: 1,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    }
}

/// A link with a ring of `ring` bytes each way: (host, client).
fn link(ring: u64) -> (Host, Client) {
    let (a, _a) = loopback::pair::<(), ()>(0).unwrap();
    let (b, _b) = loopback::pair::<(), ()>(0).unwrap();
    let [host, client] = a.link(&b, [ring, ring]).unwrap();
    let host = thread::spawn(move || connect_link::<Msg, Msg, _>(host, WAIT).unwrap());
    let client: Client = connect_link(client, WAIT).unwrap();
    (host.join().unwrap(), client)
}

fn next(host: &Host) -> Msg {
    match host.recv_timeout(WAIT).unwrap() {
        Received::Message(m) => host.check(m, &policy()).unwrap(),
        Received::Call(..) => panic!("a call"),
    }
}

fn bytes_of(seq: u32, len: usize) -> Vec<u8> {
    let mut x = seq as u64 * 0x9E37_79B9_7F4A_7C15 + 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn a_link_carries_messages_and_calls_both_ways() {
    let (host, client) = link(0);
    client.send(&Msg::Ping(1)).unwrap();
    assert_eq!(next(&host), Msg::Ping(1));
    host.send(&Msg::Ping(2)).unwrap();
    match client.recv_timeout(WAIT).unwrap() {
        Received::Message(m) => assert_eq!(m, Msg::Ping(2)),
        Received::Call(..) => panic!("a call"),
    }
    let answer = thread::spawn(move || match host.recv_timeout(WAIT).unwrap() {
        Received::Call(m, replier) => {
            let Msg::Ping(n) = host.check(m, &policy()).unwrap() else {
                panic!("not a ping");
            };
            replier.reply(&Msg::Ping(n + 1)).unwrap();
        }
        Received::Message(_) => panic!("not a call"),
    });
    assert_eq!(
        client.call::<Msg>(&Msg::Ping(41), WAIT).unwrap(),
        Msg::Ping(42)
    );
    answer.join().unwrap();
}

/// Sizes around every edge: inline, one byte past it, the ring's size exactly, and past it (the
/// bytes stream in after the message), many times round the ring.
#[test]
fn bulk_on_a_link_crosses_at_every_size_and_wraps_its_ring() {
    let ring = 256 << 10;
    let (host, client) = link(ring);
    let sizes = [
        0,
        1,
        INLINE_MAX,
        INLINE_MAX + 1,
        100_000,
        ring as usize,
        ring as usize + 1,
        1 << 20,
        3 << 20,
    ];
    let count = 60u32;
    let writer = thread::spawn(move || {
        for seq in 0..count {
            let bytes = bytes_of(seq, sizes[seq as usize % sizes.len()]);
            client
                .send_bulk(&bytes, |bulk| Msg::Bytes(seq, bulk), WAIT)
                .unwrap();
        }
        client
    });
    for seq in 0..count {
        let Msg::Bytes(got, bulk) = next(&host) else {
            panic!("not bytes");
        };
        assert_eq!(got, seq);
        let len = sizes[seq as usize % sizes.len()];
        assert_eq!(bulk.len(), len as u64);
        assert_eq!(matches!(bulk, Bulk::Inline(_)), len <= INLINE_MAX, "{seq}");
        assert!(
            host.open_bulk(bulk, WAIT).unwrap() == bytes_of(seq, len),
            "{seq}"
        );
    }
    writer.join().unwrap();
}

#[test]
fn a_bulk_never_opened_is_skipped() {
    let (host, client) = link(1 << 20);
    for seq in 0..3 {
        let bytes = bytes_of(seq, 100_000);
        client
            .send_bulk(&bytes, |bulk| Msg::Bytes(seq, bulk), WAIT)
            .unwrap();
    }
    let mut last = None;
    for _ in 0..3 {
        last = Some(next(&host));
    }
    let Some(Msg::Bytes(2, bulk)) = last else {
        panic!("{last:?}");
    };
    assert_eq!(host.open_bulk(bulk, WAIT).unwrap(), bytes_of(2, 100_000));
}

/// With no ring that way, large bytes are refused at the sender, and nothing is sent.
#[test]
fn a_link_without_a_ring_refuses_large_bytes() {
    let (host, client) = link(0);
    let big = vec![7; INLINE_MAX + 1];
    assert!(matches!(
        client.send_bulk(&big, |b| Msg::Bytes(0, b), WAIT),
        Err(Error::Refused)
    ));
    client
        .send_bulk(&[1, 2], |b| Msg::Bytes(1, b), WAIT)
        .unwrap();
    let Msg::Bytes(1, bulk) = next(&host) else {
        panic!("the small one did not come first");
    };
    assert_eq!(host.open_bulk(bulk, WAIT).unwrap(), [1, 2]);
}

fn closed_soon(host: &Host) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        match host.recv_timeout(Duration::from_millis(100)) {
            Err(Error::Closed) => return true,
            Err(Error::Timeout) => {}
            other => panic!("{:?}", other.is_ok()),
        }
    }
    false
}

/// Each rule a bulk can break ends the link: the host's reads end, and so do the client's.
#[test]
fn a_bulk_that_breaks_a_rule_ends_the_link() {
    for rule in 0..5 {
        let (host, client) = link(1 << 20);
        // (one good message first, so that "behind" has something to be behind)
        let good = bytes_of(0, 100_000);
        client.send_bulk(&good, |b| Msg::Bytes(0, b), WAIT).unwrap();
        let Msg::Bytes(0, bulk) = next(&host) else {
            panic!("not bytes");
        };
        assert_eq!(host.open_bulk(bulk, WAIT).unwrap(), good);
        let forged = match rule {
            0 => Bulk::Ring { pos: 0, len: 10 },
            1 => Bulk::Inline(vec![0; INLINE_MAX + 1]),
            2 => Bulk::Shared { id: 1, len: 10 },
            3 => Bulk::Given {
                handle: 4,
                size: 4096,
                len: 10,
            },
            _ => Bulk::Ring {
                pos: u64::MAX - 5,
                len: 10,
            },
        };
        client.send(&Msg::Bytes(1, forged)).unwrap();
        let Msg::Bytes(1, bulk) = next(&host) else {
            panic!("not bytes");
        };
        // (at once: a forged position is refused, not waited on until the timeout)
        let start = Instant::now();
        assert!(
            matches!(host.open_bulk(bulk, WAIT), Err(Error::BadMessage(_))),
            "{rule}"
        );
        assert!(start.elapsed() < Duration::from_secs(5), "{rule}");
        assert!(closed_soon(&host), "{rule}");
        let end = Instant::now() + Duration::from_secs(5);
        while client.send(&Msg::Ping(0)).is_ok() {
            assert!(Instant::now() < end, "{rule}: the client's end stays open");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A link carries no section business: a control frame ends it, and a control reply is never
/// taken for the answer to a call.
#[test]
fn a_control_frame_ends_a_link() {
    let (host, client) = link(0);
    client
        .write_raw(frame::encode(Kind::Control, 1, &[0]).unwrap())
        .unwrap();
    assert!(closed_soon(&host));

    let (host, client) = link(0);
    let caller = thread::spawn(move || client.call::<Msg>(&Msg::Ping(1), WAIT));
    let Received::Call(..) = host.recv_timeout(WAIT).unwrap() else {
        panic!("not a call");
    };
    let answer = snowlark_ipc::wire::encode(&Msg::Ping(9)).unwrap();
    host.write_raw(frame::encode(Kind::ControlReply, 1, &answer).unwrap())
        .unwrap();
    assert!(matches!(caller.join().unwrap(), Err(Error::Closed)));
}

/// Bytes still coming when the writer's end goes: the reader takes what came and ends at once, not
/// at its timeout.
#[test]
fn a_writer_gone_mid_stream_ends_the_read() {
    let ring = 64 << 10;
    let (host, client) = link(ring);
    let closer = client.clone();
    let writer = thread::spawn(move || {
        let bytes = bytes_of(0, 4 << 20);
        client.send_bulk(&bytes, |b| Msg::Bytes(0, b), WAIT)
    });
    let Msg::Bytes(0, bulk) = next(&host) else {
        panic!("not bytes");
    };
    // (the writer is now waiting for room: the reader has not opened the bulk)
    thread::sleep(Duration::from_millis(100));
    closer.close();
    let start = Instant::now();
    assert!(matches!(host.open_bulk(bulk, WAIT), Err(Error::Closed)));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(matches!(writer.join().unwrap(), Err(Error::Closed)));
}

/// The browser makes no ring past what one section may hold.
#[test]
fn a_ring_past_a_sections_limit_is_refused() {
    let (a, _a) = loopback::pair::<(), ()>(0).unwrap();
    let (b, _b) = loopback::pair::<(), ()>(0).unwrap();
    assert!(a.link(&b, [snowlark_ipc::MAX_SECTION, 0]).is_err());
    assert!(a.link(&b, [0, u64::MAX]).is_err());
    assert!(a.link(&b, [4096, 0]).is_ok());
}
