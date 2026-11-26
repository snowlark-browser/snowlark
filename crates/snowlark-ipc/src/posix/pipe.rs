// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control channel: a connected pair of Unix stream sockets per child, with no name anywhere in
//! the file system or the abstract namespace, so nothing but the two ends can ever reach it. The
//! browser makes both; the child inherits its end at launch, or gets it as a descriptor.

use std::os::fd::{FromRawFd, OwnedFd};

use crate::error::Error;

/// The two ends of a new channel: (server, client). Both close on exec and do not block.
pub(crate) fn pair() -> Result<(OwnedFd, OwnedFd), Error> {
    let mut fds = [0; 2];
    // SAFETY: `fds` is an array of two ints the call fills on success.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            fds.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(Error::Os(std::io::Error::last_os_error()));
    }
    // SAFETY: both are new descriptors of ours, owned from here on.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
