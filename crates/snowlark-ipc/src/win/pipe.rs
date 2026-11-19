// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control pipe: one duplex overlapped byte-mode named pipe per child. The browser creates both
//! ends under a random 128-bit name, first instance only, remote clients refused, with a DACL for
//! this user alone. The child never opens anything by name: it inherits the client end.

use std::os::windows::io::OwnedHandle;
use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::Error;
use crate::win::handle::owned;

const BUFFER: u32 = 64 << 10;

pub(crate) fn random_16() -> Result<[u8; 16], Error> {
    let mut bytes = [0u8; 16];
    // SAFETY: fills the local buffer; the system-preferred generator needs no algorithm handle.
    unsafe {
        BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG).ok()?;
    }
    Ok(bytes)
}

/// This user's SID as a string.
fn user_sid() -> Result<String, Error> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a local out value and is taken into an owner at once.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
    let token = owned(token);
    let mut size = 0;
    let handle = crate::win::handle::raw(&token);
    // SAFETY: the first call only asks for the size (and reports an error for doing so). The second
    // fills a buffer of that size, 8-aligned for the TOKEN_USER at its start, whose SID pointer
    // stays inside it. The string the system allocates is copied, then freed.
    unsafe {
        let _ = GetTokenInformation(handle, TokenUser, None, 0, &mut size);
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        GetTokenInformation(
            handle,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
        let sid = text.to_string().map_err(|_| Error::Refused);
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

/// `D:P(A;;GA;;;<user>)`: protected, and all access for this user only.
fn descriptor() -> Result<&'static HSTRING, Error> {
    static SDDL: OnceLock<HSTRING> = OnceLock::new();
    if let Some(s) = SDDL.get() {
        return Ok(s);
    }
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;{})", user_sid()?));
    Ok(SDDL.get_or_init(|| sddl))
}

/// The two ends of a new pipe: (server, client). Neither is inheritable yet.
pub(crate) fn pair() -> Result<(OwnedHandle, OwnedHandle), Error> {
    let id = random_16()?;
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let name = HSTRING::from(format!(r"\\.\pipe\snowlark-ipc-{hex}"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a valid string; the descriptor the system allocates is freed below, after the
    // only call that reads it.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(descriptor()?.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: valid name and attributes; the result is checked before it is owned.
    let server = unsafe {
        CreateNamedPipeW(
            &name,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            BUFFER,
            BUFFER,
            0,
            Some(&attributes),
        )
    };
    // SAFETY: allocated by the conversion above, and nothing refers to it any more.
    let _ = unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    if server.is_invalid() {
        return Err(windows::core::Error::from_thread().into());
    }
    let server = owned(server);
    // SAFETY: opens the pipe just made. Anonymous: the server end can never impersonate this client.
    let client = unsafe {
        CreateFileW(
            &name,
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
            None,
        )?
    };
    Ok((server, owned(client)))
}
