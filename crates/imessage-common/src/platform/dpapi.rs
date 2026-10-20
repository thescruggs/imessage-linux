//! Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`), current-user
//! scope: only the same Windows account (on the same machine, or via its
//! roaming credentials) can unwrap the blob. Used for secrets that Linux
//! keeps in a mode-0600 file or the Secret Service.

use std::io;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

fn blob(data: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let len = u32::try_from(data.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "DPAPI input too large"))?;
    Ok(CRYPT_INTEGER_BLOB { cbData: len, pbData: data.as_ptr().cast_mut() })
}

/// Copy out and free a blob DPAPI allocated.
///
/// SAFETY: `out` must come from a successful CryptProtectData /
/// CryptUnprotectData call (LocalAlloc'd, `cbData` bytes).
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: per the caller's contract.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        v
    }
}

/// Wrap `data` for the current user. Never shows UI.
pub fn protect(data: &[u8], description: &str) -> io::Result<Vec<u8>> {
    let input = blob(data)?;
    let desc: Vec<u16> = description.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: `input` points at `data` (only read), `desc` is NUL-terminated,
    // optional pointers are null; on success `out` is ours to free.
    let ok = unsafe {
        CryptProtectData(&input, desc.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful call.
    Ok(unsafe { take(out) })
}

/// Unwrap a blob from [`protect`]. Fails for another user's (or a corrupt)
/// blob. The caller should zeroize the result if it is secret.
pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data)?;
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: as in `protect`; no description is requested back.
    let ok = unsafe {
        CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful call.
    Ok(unsafe { take(out) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let secret = [0x5au8; 32];
        let wrapped = protect(&secret, "imessage-common test").unwrap();
        assert_ne!(&wrapped[..], &secret[..]);
        assert_eq!(unprotect(&wrapped).unwrap(), secret);
        let mut bad = wrapped.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(unprotect(&bad).is_err(), "tampered blob");
        assert!(unprotect(b"not a dpapi blob").is_err());
    }
}
