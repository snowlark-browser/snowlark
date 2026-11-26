// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The operating system's part on Linux, so far: the build id. The process boundary itself (Unix
//! domain sockets passing descriptors, memfd sections, launch through a fork server, crash dumps)
//! is stage 4 of docs/linux.md; until it comes, only the loopback runs here.

use crate::wire::BuildId;

/// This binary's identity: its executable file's size and modification time. Both ends being one
/// file is what makes this enough.
pub fn build_id() -> BuildId {
    let mut id = [0u8; 16];
    if let Ok(meta) = std::env::current_exe().and_then(std::fs::metadata) {
        id[0..8].copy_from_slice(&meta.len().to_le_bytes());
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u32);
        id[8..12].copy_from_slice(&modified.to_le_bytes());
    }
    id[12..16].copy_from_slice(&crate::wire::MAGIC.to_le_bytes());
    BuildId(id)
}

/// The total size in bytes of the file system that holds `path`: not known here yet.
pub fn volume_size(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The machine's physical memory in bytes, from `/proc/meminfo`.
pub fn physical_memory() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_build_id_is_stable() {
        assert_eq!(super::build_id(), super::build_id());
    }

    #[test]
    fn the_machine_has_physical_memory() {
        let bytes = super::physical_memory().unwrap();
        assert!(bytes > 1 << 30, "{bytes}");
    }
}
