// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An `IMFByteStream` over a [`MediaResource`]: Media Foundation's source reads the resource
//! snowlark-net fetches (with its cookies, policies and range requests) instead of fetching on its
//! own. Asynchronous reads (`BeginRead`) are served by a reader thread of the stream's own, which
//! may wait for the network without holding up Media Foundation's work queues.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, Ref, Result, BOOL};
use windows::Storage::Streams::IRandomAccessStream;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, S_OK};
use windows::Win32::Media::MediaFoundation::*;

use crate::resource::MediaResource;

/// A read waiting for the reader thread.
struct Job {
    /// The caller's buffer (valid until EndRead, per IMFByteStream's contract).
    buffer: usize,
    len: u32,
    pos: u64,
    result: IMFAsyncResult,
}

// SAFETY: the buffer address is only written by the reader thread while Media Foundation waits for
// the read (the contract of BeginRead/EndRead); the async result is a free-threaded MF object.
unsafe impl Send for Job {}

struct Shared {
    resource: MediaResource,
    pos: Mutex<u64>,
    /// Bytes read by finished asynchronous reads, by their async result's address.
    done: Mutex<HashMap<usize, u32>>,
}

#[implement(IMFByteStream)]
pub struct ByteStream {
    shared: Arc<Shared>,
    reader: Mutex<Sender<Job>>,
}

fn read_full(
    resource: &MediaResource,
    pos: u64,
    buf: &mut [u8],
) -> std::result::Result<usize, String> {
    // (one read returns what is contiguous in the cache; fill the buffer up to the end)
    let mut n = 0;
    while n < buf.len() {
        let got = resource.read_at(pos + n as u64, &mut buf[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    Ok(n)
}

impl ByteStream {
    /// The stream, and the same as a WinRT `IRandomAccessStream` (what `MediaSource.CreateFromStream`
    /// takes).
    pub fn create(resource: MediaResource) -> Result<IRandomAccessStream> {
        super::ensure_com();
        let shared = Arc::new(Shared {
            resource,
            pos: Mutex::new(0),
            done: Mutex::new(HashMap::new()),
        });
        let (tx, rx) = channel::<Job>();
        let s = shared.clone();
        std::thread::Builder::new()
            .name("snowlark-media-read".into())
            .spawn(move || {
                super::ensure_com();
                while let Ok(job) = rx.recv() {
                    // SAFETY: Media Foundation keeps the buffer of a pending BeginRead alive and
                    // untouched until EndRead, which comes after the callback below.
                    let buf = unsafe {
                        std::slice::from_raw_parts_mut(job.buffer as *mut u8, job.len as usize)
                    };
                    let status = match read_full(&s.resource, job.pos, buf) {
                        Ok(n) => {
                            s.done
                                .lock()
                                .unwrap()
                                .insert(job.result.as_raw() as usize, n as u32);
                            let mut p = s.pos.lock().unwrap();
                            *p = job.pos + n as u64;
                            S_OK
                        }
                        Err(_) => E_FAIL,
                    };
                    // SAFETY: completing an async result Media Foundation made for this read.
                    unsafe {
                        let _ = job.result.SetStatus(status);
                        let _ = MFInvokeCallback(&job.result);
                    }
                }
            })
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        let stream: IMFByteStream = ByteStream {
            shared,
            reader: Mutex::new(tx),
        }
        .into();
        let mut out: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: MFCreateStreamOnMFByteStreamEx writes an owned IRandomAccessStream pointer for the
        // IID asked, which from_raw takes over.
        unsafe {
            MFCreateStreamOnMFByteStreamEx(&stream, &IRandomAccessStream::IID, &mut out)?;
            Ok(IRandomAccessStream::from_raw(out))
        }
    }
}

impl IMFByteStream_Impl for ByteStream_Impl {
    fn GetCapabilities(&self) -> Result<u32> {
        let mut caps = MFBYTESTREAM_IS_READABLE | MFBYTESTREAM_IS_REMOTE;
        if self.shared.resource.length().is_some() {
            caps |= MFBYTESTREAM_IS_SEEKABLE;
        }
        if !matches!(
            self.shared.resource.head(),
            crate::resource::Head::Ok { ranges: true, .. }
        ) {
            caps |= MFBYTESTREAM_HAS_SLOW_SEEK;
        }
        Ok(caps)
    }

    fn GetLength(&self) -> Result<u64> {
        // (-1: unknown)
        Ok(self.shared.resource.length().unwrap_or(u64::MAX))
    }

    fn SetLength(&self, _length: u64) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetCurrentPosition(&self) -> Result<u64> {
        Ok(*self.shared.pos.lock().unwrap())
    }

    fn SetCurrentPosition(&self, position: u64) -> Result<()> {
        *self.shared.pos.lock().unwrap() = position;
        Ok(())
    }

    fn IsEndOfStream(&self) -> Result<BOOL> {
        let pos = *self.shared.pos.lock().unwrap();
        Ok(self
            .shared
            .resource
            .length()
            .is_some_and(|l| pos >= l)
            .into())
    }

    // (IMFByteStream's contract: Media Foundation passes a writable buffer and an out pointer; the
    // trait the bindings generate is safe, so the pointers are checked and used here)
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn Read(&self, pb: *mut u8, cb: u32, pcbread: *mut u32) -> Result<()> {
        if pb.is_null() || pcbread.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let pos = *self.shared.pos.lock().unwrap();
        // SAFETY: the caller hands a writable buffer of `cb` bytes and an out pointer (checked
        // non-null above).
        let buf = unsafe { std::slice::from_raw_parts_mut(pb, cb as usize) };
        let n = read_full(&self.shared.resource, pos, buf)
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        *self.shared.pos.lock().unwrap() = pos + n as u64;
        // SAFETY: as above.
        unsafe { *pcbread = n as u32 };
        Ok(())
    }

    fn BeginRead(
        &self,
        pb: *mut u8,
        cb: u32,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<windows::core::IUnknown>,
    ) -> Result<()> {
        if pb.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let callback = callback.ok()?;
        // SAFETY: an async result for the caller's callback and state, completed by the reader.
        let result = unsafe { MFCreateAsyncResult(None, callback, state.as_ref())? };
        let pos = *self.shared.pos.lock().unwrap();
        self.reader
            .lock()
            .unwrap()
            .send(Job {
                buffer: pb as usize,
                len: cb,
                pos,
                result,
            })
            .map_err(|_| windows::core::Error::from(E_FAIL))
    }

    fn EndRead(&self, result: Ref<IMFAsyncResult>) -> Result<u32> {
        let result = result.ok()?;
        let n = self
            .shared
            .done
            .lock()
            .unwrap()
            .remove(&(result.as_raw() as usize));
        // SAFETY: reading the status of a completed async result.
        unsafe { result.GetStatus() }?;
        n.ok_or_else(|| E_FAIL.into())
    }

    fn Write(&self, _pb: *const u8, _cb: u32) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn BeginWrite(
        &self,
        _pb: *const u8,
        _cb: u32,
        _callback: Ref<IMFAsyncCallback>,
        _state: Ref<windows::core::IUnknown>,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndWrite(&self, _result: Ref<IMFAsyncResult>) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn Seek(&self, origin: MFBYTESTREAM_SEEK_ORIGIN, offset: i64, _flags: u32) -> Result<u64> {
        let mut pos = self.shared.pos.lock().unwrap();
        let base = if origin == msoCurrent { *pos as i64 } else { 0 };
        let new = base
            .checked_add(offset)
            .filter(|p| *p >= 0)
            .ok_or(E_INVALIDARG)?;
        *pos = new as u64;
        Ok(*pos)
    }

    fn Flush(&self) -> Result<()> {
        Ok(())
    }

    fn Close(&self) -> Result<()> {
        Ok(())
    }
}
