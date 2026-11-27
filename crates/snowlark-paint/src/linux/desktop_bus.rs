// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The desktop's session bus (D-Bus, through the system's libdbus-1): what the desktop portal
//! (file dialogs, the appearance settings) and the Secret Service (the password store's key) are
//! reached by. Calls block their thread; a request that answers later, by a signal on an object it
//! names (a Secret Service prompt's `Completed`), is waited for with [`call_and_wait`].

use std::time::{Duration, Instant};

use dbus::arg::{AppendAll, ReadAll};
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus::Path;

/// How long a plain call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection of its own to the session bus; none when there is no session bus.
pub fn session() -> Option<Connection> {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS")?;
    Connection::new_session().ok()
}

/// Whether a name is owned on the bus, or can be started by it (a service that activates on
/// demand).
pub fn has_service(conn: &Connection, name: &str) -> bool {
    let bus = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        CALL_TIMEOUT,
    );
    let owned: Option<(bool,)> = bus
        .method_call("org.freedesktop.DBus", "NameHasOwner", (name,))
        .ok();
    if owned.is_some_and(|(o,)| o) {
        return true;
    }
    let activatable: Option<(Vec<String>,)> = bus
        .method_call("org.freedesktop.DBus", "ListActivatableNames", ())
        .ok();
    activatable.is_some_and(|(names,)| names.iter().any(|n| n == name))
}

/// Listens for the signal `member` of `interface` on the object `path` from `sender`, runs `call`
/// (which makes the request that answers by it), and waits up to `timeout` for the signal's
/// arguments. The match is in place before the call, so a quick answer is not missed.
pub fn call_and_wait<A: ReadAll + Send + 'static>(
    conn: &Connection,
    sender: &str,
    path: &Path<'static>,
    interface: &str,
    member: &str,
    timeout: Duration,
    call: impl FnOnce() -> Option<()>,
) -> Option<A> {
    let rule = MatchRule::new_signal(interface.to_owned(), member.to_owned())
        .with_sender(sender.to_owned())
        .with_path(path.clone());
    let got = std::sync::Arc::new(std::sync::Mutex::new(None));
    let put = got.clone();
    let token = conn
        .add_match(rule, move |args: A, _: &Connection, _| {
            *put.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
            false
        })
        .ok()?;
    let answer = (|| {
        call()?;
        let end = Instant::now() + timeout;
        loop {
            if let Some(a) = got.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Some(a);
            }
            let left = end.checked_duration_since(Instant::now())?;
            conn.process(left.min(Duration::from_millis(200))).ok()?;
        }
    })();
    let _ = conn.remove_match(token);
    answer
}

/// Calls `method` of `interface` on `dest`'s object `path`, with the plain timeout.
pub fn call<R: ReadAll, A: AppendAll>(
    conn: &Connection,
    dest: &str,
    path: &str,
    interface: &str,
    method: &str,
    args: A,
) -> Option<R> {
    conn.with_proxy(dest, path, CALL_TIMEOUT)
        .method_call(interface, method, args)
        .ok()
}
