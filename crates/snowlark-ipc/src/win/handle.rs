// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handles between processes. Only the browser duplicates; a child holds no handle to the browser.
//! It duplicates into a child, and out of the GPU process only ([`Peer::take_out`]): there the
//! value a child chose names a handle in that child's own table, which it can only give away, and
//! never one of the browser's.

use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS,
    HANDLE,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::error::Error;
use crate::section::Section;
use crate::win::launch::Process;

pub(crate) fn raw(h: &impl AsRawHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

/// Takes ownership of a handle the system just returned.
pub(crate) fn owned(h: HANDLE) -> OwnedHandle {
    // SAFETY: every caller passes a valid handle it owns and does not use again.
    unsafe { OwnedHandle::from_raw_handle(h.0 as RawHandle) }
}

/// A handle value in the child's own table, on its way there inside a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteHandle(u64);

impl RemoteHandle {
    pub(crate) fn from_value(value: u64) -> RemoteHandle {
        RemoteHandle(value)
    }

    /// Owns the handle this value names in this process. Only a child's end offers this
    /// ([`crate::ChildEnd::take_handle`]): a browser that did would be closing a value a child chose.
    pub(crate) fn take(self) -> OwnedHandle {
        owned(HANDLE(self.0 as usize as *mut core::ffi::c_void))
    }

    /// [`RemoteHandle::take`], in the form every platform has (a descriptor table can say no).
    pub(crate) fn take_checked(self) -> Result<OwnedHandle, Error> {
        Ok(self.take())
    }

    /// A handle of this process's, left in its table for the browser to take out
    /// ([`crate::ChildEnd::give_handle`]).
    pub(crate) fn give(handle: OwnedHandle) -> RemoteHandle {
        use std::os::windows::io::IntoRawHandle;
        RemoteHandle(handle.into_raw_handle() as usize as u64)
    }
}

/// Who is at the other end of a connection.
#[derive(Clone)]
pub(crate) enum Peer {
    /// A child of this process (the browser's end).
    Child(Arc<Process>),
    /// The browser (a child's end).
    Browser,
    /// This same process (the loopback).
    Local,
    /// The other end of a link between two children: no handle to it is held.
    Link,
}

/// `SECTION_QUERY | SECTION_MAP_WRITE | SECTION_MAP_READ`: not `SECTION_EXTEND_SIZE`.
const SECTION_FOR_CHILD: u32 = 0x0001 | 0x0002 | 0x0004;

impl Peer {
    pub(crate) fn describe(&self) -> String {
        match self {
            Peer::Child(p) => format!("process {}", p.pid()),
            Peer::Browser => "the browser".into(),
            Peer::Local => "the loopback".into(),
            Peer::Link => "the linked process".into(),
        }
    }

    pub(crate) fn kill(&self) {
        if let Peer::Child(p) = self {
            p.kill();
        }
    }

    fn target(&self) -> Result<HANDLE, Error> {
        match self {
            Peer::Child(p) => Ok(raw(p.handle())),
            // SAFETY: a pseudo handle, valid in any process.
            Peer::Local => Ok(unsafe { GetCurrentProcess() }),
            Peer::Browser | Peer::Link => Err(Error::Refused),
        }
    }

    fn put(
        &self,
        source: HANDLE,
        access: u32,
        options: DUPLICATE_HANDLE_OPTIONS,
    ) -> Result<u64, Error> {
        let target = self.target()?;
        let mut out = HANDLE::default();
        // SAFETY: `source` is open in this process, `target` is a process handle with
        // PROCESS_DUP_HANDLE, and `out` is a local.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                target,
                &mut out,
                access,
                false,
                options,
            )?;
        }
        Ok(out.0 as usize as u64)
    }

    /// Moves the handle `value` names in the child's table into this process's: it is closed
    /// there. A value that names nothing there fails.
    pub(crate) fn take_out(&self, value: RemoteHandle) -> Result<OwnedHandle, Error> {
        let source = match self {
            Peer::Child(p) => raw(p.handle()),
            // SAFETY: a pseudo handle, valid in any process.
            Peer::Local => unsafe { GetCurrentProcess() },
            Peer::Browser | Peer::Link => return Err(Error::Refused),
        };
        let mut out = HANDLE::default();
        // SAFETY: `source` is a process handle with PROCESS_DUP_HANDLE (the one handles go into
        // the child by), the value is looked up in that process's table, where the system checks
        // it, and `out` is a local, a new handle of this process's on success.
        unsafe {
            DuplicateHandle(
                source,
                HANDLE(value.0 as usize as *mut core::ffi::c_void),
                GetCurrentProcess(),
                &mut out,
                0,
                false,
                DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
            )?;
        }
        Ok(owned(out))
    }

    pub(crate) fn duplicate(&self, handle: &impl AsHandle) -> Result<RemoteHandle, Error> {
        let source = HANDLE(handle.as_handle().as_raw_handle());
        Ok(RemoteHandle(self.put(source, 0, DUPLICATE_SAME_ACCESS)?))
    }

    /// A section for the child: it can map it, but neither extend it nor do anything else with it.
    pub(crate) fn duplicate_section(&self, section: &Section) -> Result<u64, Error> {
        self.put(
            raw(section.handle()),
            SECTION_FOR_CHILD,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    }
}
