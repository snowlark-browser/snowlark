// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Code signing: whether an executable carries a valid Authenticode signature, and who signed it.
//!
//! [`signer_of`] asks `WinVerifyTrust` (the Authenticode policy, `WINTRUST_ACTION_GENERIC_VERIFY_V2`)
//! to check the file's embedded signature: the file's hash matches what was signed, and the
//! signing certificate chains to a root the machine trusts and was valid when the signature's
//! timestamp says it was made. Revocation is not checked (nothing goes on the network, and a check
//! that cannot be made is not a failure). The answer names the signing certificate by its subject
//! and issuer, as DER-encoded names, so two files are "by the same signer" when both match:
//! a renewed certificate from the same authority for the same subject still does.

use std::path::Path;

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};

/// Who signed a file: the signing certificate's subject and issuer (DER-encoded X.500 names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    pub subject: Vec<u8>,
    pub issuer: Vec<u8>,
}

impl Signer {
    /// Whether `other` was signed by the same signer: the same subject, certified by the same
    /// issuer.
    pub fn same_as(&self, other: &Signer) -> bool {
        self.subject == other.subject && self.issuer == other.issuer
    }
}

/// The signer of `path`'s embedded Authenticode signature, when the signature is valid; the reason
/// otherwise (no signature, a file changed since it was signed, an untrusted or expired
/// certificate).
pub fn signer_of(path: &Path) -> Result<Signer, String> {
    let wide = HSTRING::from(path.as_os_str());
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` is a WINTRUST_DATA with its size set, naming the file through `file`, whose
    // path is the NUL-terminated `wide`; all three outlive the call. With no window and WTD_UI_NONE
    // the call shows nothing. It opens state (`hWVTStateData`) that is closed below in every case.
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    let signer = if status == 0 {
        // SAFETY: after a successful verify, `hWVTStateData` is the provider's state, which stays
        // valid until the close below. The signer and its chain are the provider's own structures;
        // each pointer is checked before it is read, and the names are copied out of the
        // certificate before the state is closed.
        unsafe { read_signer(data.hWVTStateData) }
    } else {
        Err(format!(
            "not validly signed (WinVerifyTrust {:#010x})",
            status as u32
        ))
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: closes the state the verify opened (a no-op when it opened none), with the same
    // `data`, `file` and `wide`, still alive.
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    signer
}

/// The first signer's certificate names from a verify's state.
///
/// # Safety
///
/// `state` is the `hWVTStateData` of a successful `WinVerifyTrust` verify that is not closed yet.
unsafe fn read_signer(state: HANDLE) -> Result<Signer, String> {
    // SAFETY: as the function's contract says; every pointer is checked for null before use.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return Err("no provider data".into());
        }
        let sgnr = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
        if sgnr.is_null() || (*sgnr).csCertChain == 0 || (*sgnr).pasCertChain.is_null() {
            return Err("no signer".into());
        }
        let cert = (*(*sgnr).pasCertChain).pCert;
        if cert.is_null() || (*cert).pCertInfo.is_null() {
            return Err("no signing certificate".into());
        }
        let info = &*(*cert).pCertInfo;
        let blob = |b: &windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB| {
            if b.pbData.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(b.pbData, b.cbData as usize).to_vec()
            }
        };
        let signer = Signer {
            subject: blob(&info.Subject),
            issuer: blob(&info.Issuer),
        };
        if signer.subject.is_empty() {
            return Err("the signing certificate has no subject".into());
        }
        Ok(signer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsigned_executable_has_no_signer() {
        // (a test binary is never signed)
        let me = std::env::current_exe().unwrap();
        assert!(signer_of(&me).is_err());
        assert!(signer_of(Path::new(r"Q:\no\such\file.exe")).is_err());
    }

    #[test]
    fn the_same_signer_is_the_same_subject_and_issuer() {
        let a = Signer {
            subject: vec![1, 2],
            issuer: vec![3],
        };
        assert!(a.same_as(&a.clone()));
        let other_subject = Signer {
            subject: vec![1, 9],
            ..a.clone()
        };
        assert!(!a.same_as(&other_subject));
        let other_issuer = Signer {
            issuer: vec![4],
            ..a.clone()
        };
        assert!(!a.same_as(&other_issuer));
    }

    /// With `SNOWLARK_TEST_SIGNED_EXE` naming an executable with a valid signature and
    /// `SNOWLARK_TEST_OTHER_SIGNED_EXE` one by another signer: the first verifies, the second is
    /// another signer, and a copy of the first with one byte changed is refused.
    #[test]
    #[ignore = "needs signed executables named in the environment"]
    fn a_signed_executable_names_its_signer_and_a_changed_one_is_refused() {
        let signed =
            std::env::var_os("SNOWLARK_TEST_SIGNED_EXE").expect("SNOWLARK_TEST_SIGNED_EXE");
        let other = std::env::var_os("SNOWLARK_TEST_OTHER_SIGNED_EXE")
            .expect("SNOWLARK_TEST_OTHER_SIGNED_EXE");
        let a = signer_of(Path::new(&signed)).unwrap();
        assert!(a.same_as(&signer_of(Path::new(&signed)).unwrap()));
        let b = signer_of(Path::new(&other)).unwrap();
        assert!(!a.same_as(&b));
        let dir = std::env::temp_dir().join(format!("snowlark-signing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("changed.exe");
        let mut bytes = std::fs::read(&signed).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x55;
        std::fs::write(&copy, &bytes).unwrap();
        let changed = signer_of(&copy);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(changed.is_err(), "{changed:?}");
    }
}
