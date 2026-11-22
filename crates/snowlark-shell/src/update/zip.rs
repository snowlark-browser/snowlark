// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading a release zip (APPNOTE.TXT, the ZIP file format): its central directory, and each file
//! stored or deflated, its CRC-32 checked.
//!
//! A release is a few files in one folder, so this reads only that: files directly in the zip or
//! all in one top-level folder (which is taken off), no deeper folders, no ZIP64, no encryption.
//! Anything else is refused, and so is a name that could leave the folder it is unpacked into.

use std::io::Read;

/// The largest file a release may unpack to.
pub const MAX_FILE: u64 = 1 << 30;

fn u16_at(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| "the zip is cut short".to_owned())
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| "the zip is cut short".to_owned())
}

struct Entry {
    name: String,
    method: u16,
    flags: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    local: usize,
}

/// The files of the zip `bytes`, by name (the one top-level folder taken off) with their contents.
pub fn files(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    // The end of central directory record: the last one in the final 64 KiB + 22 bytes.
    let from = bytes.len().saturating_sub(22 + 0xFFFF);
    let end = (from..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&i| u32_at(bytes, i) == Ok(0x0605_4b50))
        .ok_or("not a zip")?;
    let count = u16_at(bytes, end + 10)? as usize;
    let mut at = u32_at(bytes, end + 16)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(bytes, at)? != 0x0201_4b50 {
            return Err("a broken central directory".into());
        }
        let name_len = u16_at(bytes, at + 28)? as usize;
        let extra_len = u16_at(bytes, at + 30)? as usize;
        let comment_len = u16_at(bytes, at + 32)? as usize;
        let name = bytes
            .get(at + 46..at + 46 + name_len)
            .ok_or("the zip is cut short")?;
        entries.push(Entry {
            name: String::from_utf8(name.to_vec()).map_err(|_| "a name that is not UTF-8")?,
            flags: u16_at(bytes, at + 8)?,
            method: u16_at(bytes, at + 10)?,
            crc: u32_at(bytes, at + 16)?,
            compressed: u64::from(u32_at(bytes, at + 20)?),
            size: u64::from(u32_at(bytes, at + 24)?),
            local: u32_at(bytes, at + 42)? as usize,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    // (folders are only the one at the top)
    let files: Vec<&Entry> = entries.iter().filter(|e| !e.name.ends_with('/')).collect();
    let top = files
        .first()
        .and_then(|e| e.name.split_once('/'))
        .map(|(t, _)| format!("{t}/"));
    let top = top.filter(|t| files.iter().all(|e| e.name.starts_with(t.as_str())));
    if top
        .as_deref()
        .is_some_and(|t| !plain_name(&t[..t.len() - 1]))
    {
        return Err("the zip's folder is not a plain name".into());
    }
    let mut out = Vec::with_capacity(files.len());
    for e in files {
        let name = match &top {
            Some(t) => &e.name[t.len()..],
            None => e.name.as_str(),
        };
        if !plain_name(name) {
            return Err(format!("{:?} is not a plain file name", e.name));
        }
        if e.flags & 1 != 0 {
            return Err(format!("{name} is encrypted"));
        }
        if e.size > MAX_FILE || e.compressed > MAX_FILE {
            return Err(format!("{name} is too large"));
        }
        if u32_at(bytes, e.local)? != 0x0403_4b50 {
            return Err(format!("{name}: a broken local header"));
        }
        let start = e.local
            + 30
            + u16_at(bytes, e.local + 26)? as usize
            + u16_at(bytes, e.local + 28)? as usize;
        let data = bytes
            .get(start..start + e.compressed as usize)
            .ok_or("the zip is cut short")?;
        let content = match e.method {
            0 => data.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(e.size as usize);
                flate2::read::DeflateDecoder::new(data)
                    .take(e.size + 1)
                    .read_to_end(&mut v)
                    .map_err(|err| format!("{name}: {err}"))?;
                v
            }
            m => return Err(format!("{name}: compression method {m}")),
        };
        if content.len() as u64 != e.size {
            return Err(format!("{name} is not the size the zip says"));
        }
        let mut crc = flate2::Crc::new();
        crc.update(&content);
        if crc.sum() != e.crc {
            return Err(format!("{name} does not match its CRC-32"));
        }
        if out
            .iter()
            .any(|(n, _): &(String, Vec<u8>)| n.eq_ignore_ascii_case(name))
        {
            return Err(format!("{name} is in the zip twice"));
        }
        out.push((name.to_owned(), content));
    }
    Ok(out)
}

/// A file name to unpack into a folder: one part, nothing Windows reads as a path, a device or a
/// stream.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && !name.ends_with(' ')
        && name
            .chars()
            .all(|c| !c.is_control() && !"/\\:*?\"<>|".contains(c))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `files` (name, contents), deflated, as the release script's archiver writes one.
    pub(crate) fn make(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content) in files {
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(content).unwrap();
            let data = enc.finish().unwrap();
            let mut crc = flate2::Crc::new();
            crc.update(content);
            let local = out.len() as u32;
            let header = |sig: u32, central: bool| {
                let mut h = Vec::new();
                h.extend(sig.to_le_bytes());
                if central {
                    h.extend(20u16.to_le_bytes());
                }
                h.extend(20u16.to_le_bytes());
                h.extend(0u16.to_le_bytes());
                h.extend(8u16.to_le_bytes());
                h.extend(0u32.to_le_bytes());
                h.extend(crc.sum().to_le_bytes());
                h.extend((data.len() as u32).to_le_bytes());
                h.extend((content.len() as u32).to_le_bytes());
                h.extend((name.len() as u16).to_le_bytes());
                h.extend(0u16.to_le_bytes());
                if central {
                    // (comment length, disk, internal and external attributes)
                    h.extend([0u8; 6]);
                    h.extend(0u32.to_le_bytes());
                    h.extend(local.to_le_bytes());
                }
                h.extend(name.as_bytes());
                h
            };
            out.extend(header(0x0403_4b50, false));
            out.extend(&data);
            central.extend(header(0x0201_4b50, true));
        }
        let at = out.len() as u32;
        out.extend(&central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn a_release_zip_unpacks_without_its_top_folder() {
        let zip = make(&[
            ("snowlark-preview-x/snowlark.exe", b"MZ program"),
            ("snowlark-preview-x/README.txt", b"read me"),
        ]);
        let f = files(&zip).unwrap();
        assert_eq!(f[0], ("snowlark.exe".to_owned(), b"MZ program".to_vec()));
        assert_eq!(f[1], ("README.txt".to_owned(), b"read me".to_vec()));
        // Files at the top level stay as they are.
        let flat = files(&make(&[("a.txt", b"a")])).unwrap();
        assert_eq!(flat[0].0, "a.txt");
    }

    #[test]
    fn a_name_that_leaves_the_folder_or_goes_deeper_is_refused() {
        for bad in ["top/../x.exe", "top/sub/x.exe", "top/a:b", "top/c\\d"] {
            let zip = make(&[(bad, b"x"), ("top/ok.txt", b"y")]);
            assert!(files(&zip).is_err(), "{bad}");
        }
        assert!(files(&make(&[("../x.exe", b"x")])).is_err());
    }

    #[test]
    fn a_changed_or_cut_zip_is_refused() {
        let zip = make(&[("top/snowlark.exe", b"MZ program bytes")]);
        // (a byte of the deflated data changed: the CRC or the inflate fails)
        let mut bad = zip.clone();
        bad[30 + "top/snowlark.exe".len() + 2] ^= 0xFF;
        assert!(files(&bad).is_err());
        assert!(files(&zip[..zip.len() - 30]).is_err());
        assert!(files(b"").is_err());
        assert!(files(b"PK not really").is_err());
    }
}
