// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The same connection code with no pipe under it.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use snowlark_ipc::loopback::{self, pair};
use snowlark_ipc::{BadMessage, Error, ProcessPolicy, Received, Validate};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
enum Message {
    Text(String),
    Number(u64),
}

impl Validate for Message {
    fn validate(self, _: &ProcessPolicy) -> Result<Message, BadMessage> {
        match self {
            Message::Number(n) if n > 100 => Err(BadMessage("too big".into())),
            m => Ok(m),
        }
    }
}

#[test]
fn messages_arrive_in_order_through_the_wire_format() {
    let (browser, child) = pair::<Message, Message>(0).unwrap();
    for n in 0..100 {
        browser.send(&Message::Number(n)).unwrap();
        child.send(&Message::Number(n)).unwrap();
    }
    for n in 0..100 {
        let Received::Message(m) = child.recv_timeout(WAIT).unwrap() else {
            panic!();
        };
        assert_eq!(m, Message::Number(n));
        let Received::Message(m) = browser.recv_timeout(WAIT).unwrap() else {
            panic!();
        };
        assert_eq!(browser.check(m).unwrap(), Message::Number(n));
    }
}

#[test]
fn calls_are_answered() {
    let (browser, child) = pair::<Message, Message>(0).unwrap();
    let server = std::thread::spawn(move || {
        while let Ok(Received::Call(Message::Number(n), replier)) = child.recv() {
            replier.reply(&Message::Number(n + 1)).unwrap();
        }
    });
    let r = browser.call::<Message>(&Message::Number(1), WAIT).unwrap();
    assert_eq!(browser.check(r).unwrap(), Message::Number(2));
    browser.close();
    server.join().unwrap();
}

#[test]
fn sections_work_in_one_process() {
    let (browser, child) = pair::<Message, Message>(1 << 20).unwrap();
    let section = child.alloc_shared(4096, WAIT).unwrap();
    assert!(section.write(0, b"shared"));
    let seen = browser.section(section.id()).unwrap();
    assert_eq!(seen.read_once(0, 6).unwrap(), b"shared");
    assert_eq!(browser.shared_bytes(), 4096);
    assert!(matches!(
        child.alloc_shared(1 << 21, WAIT),
        Err(Error::Refused)
    ));
    drop(section);
    assert_eq!(browser.shared_bytes(), 0);
}

#[test]
fn a_childs_bulk_is_passed_to_another_child_unread() {
    let (browser, child) = pair::<Message, Message>(1 << 20).unwrap();
    let (other, other_child) = pair::<Message, Message>(0).unwrap();
    let bytes: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    let bulk = child.bulk(bytes.clone(), WAIT).unwrap();
    assert!(matches!(bulk, snowlark_ipc::Bulk::Shared { .. }));
    assert!(browser.shared_bytes() as usize >= bytes.len());
    let passed = browser.pass_bulk(bulk, &other).unwrap();
    assert!(matches!(passed, snowlark_ipc::Bulk::Given { .. }));
    // (out of the first child's quota, and the second reads what the first wrote)
    assert_eq!(browser.shared_bytes(), 0);
    assert_eq!(other_child.open_bulk(passed).unwrap(), bytes);
    // (a section the child does not hold, and a handle from a child, break the rules)
    let forged = snowlark_ipc::Bulk::Shared { id: 99, len: 1 };
    assert!(matches!(
        browser.pass_bulk(forged, &other),
        Err(Error::BadMessage(_))
    ));
    let small = browser
        .pass_bulk(snowlark_ipc::Bulk::Inline(vec![1, 2]), &other)
        .unwrap();
    assert_eq!(small, snowlark_ipc::Bulk::Inline(vec![1, 2]));
}

#[test]
fn a_failed_check_closes_the_loopback() {
    let (browser, child) = pair::<Message, Message>(0).unwrap();
    child.send(&Message::Number(101)).unwrap();
    let Received::Message(m) = browser.recv_timeout(WAIT).unwrap() else {
        panic!();
    };
    assert!(browser.check(m).is_err());
    assert!(matches!(
        child.send(&Message::Number(1)),
        Err(Error::Closed)
    ));
}

#[test]
fn pass_gives_back_the_same_value() {
    let m = Message::Text("héllo".into());
    assert_eq!(loopback::pass(&m).unwrap(), m);
    assert!(loopback::largest() >= 6);
}

#[test]
fn pass_refuses_what_a_pipe_would() {
    let m = Message::Text("x".repeat(2 << 20));
    assert!(matches!(loopback::pass(&m), Err(Error::TooBig(_))));
}

#[test]
fn the_channel_behaves_as_mpsc() {
    let (tx, rx) = snowlark_ipc::channel();
    let tx2 = tx.clone();
    tx.send(Message::Number(1)).unwrap();
    tx2.send(Message::Number(2)).unwrap();
    assert_eq!(rx.recv().unwrap(), Message::Number(1));
    assert_eq!(rx.recv().unwrap(), Message::Number(2));
    drop((tx, tx2));
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(1)),
        Err(RecvTimeoutError::Disconnected)
    );
}
