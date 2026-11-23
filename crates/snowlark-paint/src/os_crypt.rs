// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Data only the signed-in Windows user can read back: the Data Protection API at user scope. The
//! password store wraps its key with it (`docs/password-manager.md`, "Storage on Windows").

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Copies a blob the system allocated for the caller and frees it.
///
/// # Safety
/// `out` is a blob a successful protect or unprotect call filled.
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: (the caller's) the system wrote `cbData` bytes at `pbData`, freed with LocalFree.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        v
    }
}

/// `data` encrypted so that only the current Windows user on this machine can decrypt it.
pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    let input = blob(data);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `data`, alive for the call, which only reads it; `out` receives a
    // blob the system allocated, copied and freed by `take` only when the call succeeded.
    unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
        Some(take(out))
    }
}

/// Decrypts what [`protect`] made; `None` when this user cannot (another user's or machine's data,
/// or damaged bytes).
pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let input = blob(data);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
        Some(take(out))
    }
}
