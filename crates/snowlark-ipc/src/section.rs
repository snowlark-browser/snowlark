// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared memory: pagefile-backed sections, mapped on both sides. The privileged side creates every
//! one, so a child's handle is never trusted for its type or size, no section is backed by a file
//! that could be truncated under a reader, and what a child holds is counted exactly.
//!
//! Shared memory is read once: a child can rewrite a section while the browser looks at it, so
//! [`Section::read_once`] copies to private memory, and only that copy is ever parsed.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd as OwnedHandle};
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows::Win32::System::Memory::*;

use crate::error::Error;
use crate::os::handle::RemoteHandle;
#[cfg(windows)]
use crate::win::handle::{owned, raw};

/// The most one section may hold.
pub const MAX_SECTION: u64 = 1 << 30;

/// A mapped section.
pub struct Section {
    handle: OwnedHandle,
    view: *mut u8,
    size: usize,
}

// SAFETY: the view is plain memory valid for the life of the Section; every access goes through
// methods that copy in or out with bounds checked, which any thread may do. Concurrent access
// gives wrong bytes, which is why readers copy before parsing, never undefined behaviour in safe code:
// no reference to the mapped memory is ever handed out.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Section {
    /// A new section of `size` bytes (the browser's side).
    #[cfg(windows)]
    pub fn create(size: usize) -> Result<Section, Error> {
        if size == 0 || size as u64 > MAX_SECTION {
            return Err(Error::Refused);
        }
        // SAFETY: no file, so the pagefile backs it; committed, so the size is charged now and a
        // write can never fault later. The handle is owned at once.
        let handle = owned(unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE | SEC_COMMIT,
                (size as u64 >> 32) as u32,
                size as u32,
                None,
            )?
        });
        Section::map(handle, size)
    }

    /// Maps a section the browser made for this process.
    pub(crate) fn from_remote(handle: RemoteHandle, size: u64) -> Result<Section, Error> {
        Section::map(handle.take_checked()?, size as usize)
    }

    #[cfg(windows)]
    fn map(handle: OwnedHandle, size: usize) -> Result<Section, Error> {
        // SAFETY: an open section handle. The mapping fails if the section is smaller than `size`.
        let view =
            unsafe { MapViewOfFile(raw(&handle), FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size) };
        if view.Value.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        Ok(Section {
            handle,
            view: view.Value.cast(),
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn handle(&self) -> &OwnedHandle {
        &self.handle
    }

    fn in_bounds(&self, offset: usize, len: usize) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size)
    }

    /// Copies `len` bytes at `offset` to private memory: the only bytes it is safe to parse.
    pub fn read_once(&self, offset: usize, len: usize) -> Option<Vec<u8>> {
        if !self.in_bounds(offset, len) {
            return None;
        }
        let mut out = vec![0; len];
        // SAFETY: the range is inside the view, and `out` is a separate allocation of `len` bytes.
        // Another process may write the view meanwhile: the copy then holds wrong bytes, nothing
        // worse, and nothing has read the view twice.
        unsafe { std::ptr::copy_nonoverlapping(self.view.add(offset), out.as_mut_ptr(), len) };
        Some(out)
    }

    /// Writes `bytes` at `offset`. False if they don't fit.
    pub fn write(&self, offset: usize, bytes: &[u8]) -> bool {
        if !self.in_bounds(offset, bytes.len()) {
            return false;
        }
        // SAFETY: the range is inside the view, and `bytes` is not in it: no reference to the view
        // exists to borrow from.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.view.add(offset), bytes.len())
        };
        true
    }

    /// The `u64` at `offset`, read atomically: a cell both processes keep a value in (the other
    /// process's value is its word). `offset` must be a multiple of 8 and is never also used with
    /// [`Section::read_once`] or [`Section::write`]. `None` out of bounds or unaligned.
    pub fn load_u64(&self, offset: usize) -> Option<u64> {
        self.cell(offset, |c| c.load(Ordering::Acquire))
    }

    /// Stores `value` atomically in the cell at `offset` ([`Section::load_u64`]). False if there is
    /// no such cell.
    pub fn store_u64(&self, offset: usize, value: u64) -> bool {
        self.cell(offset, |c| c.store(value, Ordering::Release))
            .is_some()
    }

    /// Raises the cell at `offset` to `value` if it is lower, atomically. False if there is no such
    /// cell.
    pub fn fetch_max_u64(&self, offset: usize, value: u64) -> bool {
        self.cell(offset, |c| c.fetch_max(value, Ordering::AcqRel))
            .is_some()
    }

    /// Runs `f` on the cell at `offset`; the reference never leaves this call.
    fn cell<R>(&self, offset: usize, f: impl FnOnce(&AtomicU64) -> R) -> Option<R> {
        if !offset.is_multiple_of(8) || !self.in_bounds(offset, 8) {
            return None;
        }
        // SAFETY: the 8 bytes are inside the view, which is valid for the life of `self` and page
        // aligned, so `offset` being a multiple of 8 aligns them for a u64. Every access to a cell is
        // atomic (other threads' and the other process's alike), and the reference does not outlive
        // this call.
        let cell = unsafe { AtomicU64::from_ptr(self.view.add(offset).cast()) };
        Some(f(cell))
    }
}

#[cfg(windows)]
impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view this Section mapped; no reference to it exists.
        let _ = unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            })
        };
    }
}

#[cfg(unix)]
fn os_check(r: libc::c_int) -> Result<libc::c_int, Error> {
    if r < 0 {
        Err(Error::Os(std::io::Error::last_os_error()))
    } else {
        Ok(r)
    }
}

#[cfg(unix)]
impl Section {
    /// A new section of `size` bytes (the browser's side): an anonymous memory file sealed at its
    /// size, so that no process holding it can shrink it under a reader (whose access past the end
    /// would fault) or grow it.
    pub fn create(size: usize) -> Result<Section, Error> {
        if size == 0 || size as u64 > MAX_SECTION {
            return Err(Error::Refused);
        }
        // SAFETY: a static NUL-terminated name; the descriptor is owned at once.
        let fd = os_check(unsafe {
            libc::memfd_create(
                c"snowlark-ipc".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        })?;
        // SAFETY: the new descriptor, ours alone.
        let handle = unsafe { OwnedHandle::from_raw_fd(fd) };
        // SAFETY: sizes and seals our own new memory file.
        unsafe {
            os_check(libc::ftruncate(handle.as_raw_fd(), size as libc::off_t))?;
            os_check(libc::fcntl(
                handle.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
            ))?;
        }
        Section::map(handle, size)
    }

    /// Maps `size` bytes of a memory file. One that is smaller, or not sealed against shrinking,
    /// is refused: a view past a file's end faults.
    fn map(handle: OwnedHandle, size: usize) -> Result<Section, Error> {
        if size == 0 || size as u64 > MAX_SECTION {
            return Err(Error::Refused);
        }
        let fd = handle.as_raw_fd();
        // SAFETY: `stat` is plain data the call fills, for an open descriptor of ours.
        let stat = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            os_check(libc::fstat(fd, &mut stat))?;
            stat
        };
        // SAFETY: reads the seals of an open descriptor.
        let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        if (stat.st_size as u64) < size as u64 || seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
            return Err(Error::Refused);
        }
        // SAFETY: a shared mapping of `size` bytes of a file at least that large that cannot
        // shrink; the view is unmapped when the Section drops.
        let view = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if view == libc::MAP_FAILED {
            return Err(Error::Os(std::io::Error::last_os_error()));
        }
        Ok(Section {
            handle,
            view: view.cast(),
            size,
        })
    }
}

#[cfg(unix)]
impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view this Section mapped, of its size; no reference to it exists.
        unsafe { libc::munmap(self.view.cast(), self.size) };
    }
}

/// The sections made for one child, counted against its quota.
pub(crate) struct Broker {
    quota: u64,
    used: u64,
    next: u64,
    sections: HashMap<u64, Arc<Section>>,
}

impl Broker {
    pub(crate) fn new(quota: u64) -> Broker {
        Broker {
            quota,
            used: 0,
            next: 1,
            sections: HashMap::new(),
        }
    }

    /// A new section, or `None` for one the quota or the system refuses. A size is the child's word,
    /// so everything about it is checked here.
    pub(crate) fn alloc(&mut self, size: u64) -> Option<(u64, Arc<Section>)> {
        if size == 0 || size > MAX_SECTION || size > self.quota.saturating_sub(self.used) {
            return None;
        }
        let section = Arc::new(Section::create(size as usize).ok()?);
        let id = self.next;
        self.next += 1;
        self.used += size;
        self.sections.insert(id, section.clone());
        Some((id, section))
    }

    /// False if the child holds no such section.
    pub(crate) fn free(&mut self, id: u64) -> bool {
        self.take(id).is_some()
    }

    /// Removes the section `id` from the child's quota and returns it, or `None` if it holds none.
    pub(crate) fn take(&mut self, id: u64) -> Option<Arc<Section>> {
        let section = self.sections.remove(&id)?;
        self.used -= section.size() as u64;
        Some(section)
    }

    pub(crate) fn get(&self, id: u64) -> Option<Arc<Section>> {
        self.sections.get(&id).cloned()
    }

    pub(crate) fn used(&self) -> u64 {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_written_read_back() {
        let s = Section::create(8192).unwrap();
        assert!(s.write(100, b"hello"));
        assert_eq!(s.read_once(100, 5).unwrap(), b"hello");
    }

    #[test]
    fn out_of_range_is_refused() {
        let s = Section::create(4096).unwrap();
        assert!(s.read_once(4090, 7).is_none());
        assert!(s.read_once(usize::MAX, 2).is_none());
        assert!(!s.write(4095, b"ab"));
        assert!(s.read_once(4096, 0).is_some());
    }

    #[test]
    fn cells_are_aligned_u64s_in_bounds() {
        let s = Section::create(16).unwrap();
        assert!(s.store_u64(8, 5));
        assert!(s.fetch_max_u64(8, 3) && s.load_u64(8) == Some(5));
        assert!(s.fetch_max_u64(8, 9) && s.load_u64(8) == Some(9));
        assert_eq!(s.load_u64(0), Some(0));
        assert_eq!(s.load_u64(4), None);
        assert_eq!(s.load_u64(16), None);
        assert!(!s.store_u64(usize::MAX - 7, 1));
    }

    #[test]
    fn sizes_are_checked() {
        assert!(Section::create(0).is_err());
        assert!(Section::create(MAX_SECTION as usize + 1).is_err());
    }

    #[test]
    fn the_quota_is_exact() {
        let mut b = Broker::new(10_000);
        let (a, _) = b.alloc(6_000).unwrap();
        assert!(b.alloc(5_000).is_none());
        assert!(b.alloc(0).is_none());
        assert!(b.alloc(u64::MAX).is_none());
        assert_eq!(b.used(), 6_000);
        assert!(b.free(a));
        assert!(!b.free(a));
        assert_eq!(b.used(), 0);
        assert!(b.alloc(10_000).is_some());
    }
}
