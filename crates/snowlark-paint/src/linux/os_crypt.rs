// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Data only the signed-in user can read back, on Linux: the desktop's Secret Service (D-Bus) is
//! the counterpart of Windows' Data Protection API and is not used yet, so nothing can be
//! protected and the password store keeps no key (it saves no passwords) rather than writing one
//! any program of the user could read.

/// `data` protected for the current user: not available yet.
pub fn protect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

/// What [`protect`] made, read back: not available yet.
pub fn unprotect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}
