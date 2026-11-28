// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the operating system does, and nearly all of this crate's `unsafe`.

pub mod cpu;
pub mod crash_dump;
pub(crate) mod exit;
pub mod handle;
pub(crate) mod io;
pub mod launch;
pub(crate) mod pipe;
pub mod processes;
pub mod signing;

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::wire::BuildId;

/// This binary's identity, from its own PE header in memory: the link's time stamp, image size and
/// checksum. Both ends being one file is what makes this cheap, with no file to hash.
pub fn build_id() -> BuildId {
    let mut id = [0u8; 16];
    // SAFETY: the module handle of the process's executable is the address of its mapped PE image,
    // which starts with a DOS header whose field at 0x3C is the offset of the NT headers. Their
    // file header has the time stamp at +8 and the optional header (at +24) has the image size at
    // +56 and checksum at +64. All of it is mapped and read-only for the life of the process.
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return BuildId(id);
        };
        let base = module.0 as *const u8;
        let nt = base.add((base.add(0x3C) as *const u32).read_unaligned() as usize);
        let field = |offset: usize| (nt.add(offset) as *const u32).read_unaligned();
        id[0..4].copy_from_slice(&field(8).to_le_bytes());
        id[4..8].copy_from_slice(&field(24 + 56).to_le_bytes());
        id[8..12].copy_from_slice(&field(24 + 64).to_le_bytes());
    }
    id[12..16].copy_from_slice(&crate::wire::MAGIC.to_le_bytes());
    BuildId(id)
}

/// The total size in bytes of the volume that holds `path` (an existing file or folder), as the
/// caller's account sees it (a disk quota on the volume counts).
pub fn volume_size(path: &std::path::Path) -> Option<u64> {
    let path = HSTRING::from(path.as_os_str());
    let mut total = 0u64;
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call, and `total` is a
    // valid u64 the call writes; the two other outputs are not asked for.
    unsafe { GetDiskFreeSpaceExW(&path, None, Some(&mut total), None) }.ok()?;
    Some(total)
}

/// The machine's physical memory in bytes.
pub fn physical_memory() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set, which the call fills.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status.ullTotalPhys)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_machine_has_physical_memory() {
        let bytes = super::physical_memory().unwrap();
        assert!(bytes > 1 << 30, "{bytes}");
    }

    #[test]
    fn the_volume_of_a_folder_has_a_size() {
        let size = super::volume_size(&std::env::temp_dir()).unwrap();
        assert!(size > 1 << 30, "{size}");
        let missing = std::path::Path::new(r"Q:\no\such\volume");
        assert_eq!(super::volume_size(missing), None);
    }
}
