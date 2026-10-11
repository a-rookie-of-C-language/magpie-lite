//! Windows files containing fresh credentials use a protected, owner-only DACL.
//! Existing agent files use ReplaceFileW so their original ACL is not discarded.
use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this is the owned process-token handle, closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Allocation(*mut c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: conversion APIs allocate this pointer with LocalAlloc.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn user_sid() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: valid process pseudo-handle and writable output pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut length = 0;
    // SAFETY: the first call only obtains the required allocation size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    // usize provides the required alignment for TOKEN_USER and its SID pointer.
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: buffer is aligned and at least length bytes long.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful TokenUser query initialized the TOKEN_USER structure.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    // SAFETY: SID points into the live buffer; conversion initializes sid.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let allocation = Allocation(sid.cast());
    let mut size = 0;
    // SAFETY: ConvertSidToStringSidW returns a null-terminated wide string.
    unsafe {
        while *sid.add(size) != 0 {
            size += 1;
        }
    }
    // SAFETY: size was obtained from that string's terminator.
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, size) })
        .map_err(|_| io::Error::other("invalid owner SID"));
    drop(allocation);
    result
}
pub fn make_private(path: &Path, directory: bool) -> io::Result<()> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    let sid = SID
        .get_or_init(|| user_sid().ok())
        .as_ref()
        .ok_or_else(|| io::Error::other("cannot identify file owner"))?;
    let inherit = if directory { "OICI" } else { "" };
    let sddl = wide(OsStr::new(&format!("D:P(A;{inherit};FA;;;{sid})")));
    let mut descriptor = null_mut();
    // SAFETY: SDDL is null-terminated; descriptor receives the allocated result.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let allocation = Allocation(descriptor);
    let path = wide(path.as_os_str());
    // SAFETY: path and descriptor remain live throughout this call.
    let result = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    drop(allocation);
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn publish(temp: tempfile::TempPath, target: &Path) -> io::Result<()> {
    if !target.try_exists()? {
        // Do not clobber a file created by an external writer since our check.
        return temp.persist_noclobber(target).map_err(|e| e.error);
    }
    let target = wide(target.as_os_str());
    let replacement = wide(temp.as_os_str());
    // SAFETY: filenames are null-terminated and no optional pointers are used.
    // Do not ignore ACL-merge errors: the journal stays pending on any failure.
    if unsafe {
        ReplaceFileW(
            target.as_ptr(),
            replacement.as_ptr(),
            null(),
            0,
            null(),
            null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // ReplaceFileW moved the temporary file; TempPath cleanup is a no-op now.
    Ok(())
}

#[cfg(test)]
pub(crate) fn dacl_sddl(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::GetFileSecurityW;
    let path = wide(path.as_os_str());
    let mut length = 0;
    // SAFETY: query buffer size without reading a security descriptor.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            null_mut(),
            0,
            &mut length,
        );
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: aligned buffer has enough room for the descriptor.
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut string = null_mut();
    let mut chars = 0;
    // SAFETY: a valid descriptor was returned above; output is locally allocated.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            buffer.as_ptr() as *mut c_void,
            1,
            DACL_SECURITY_INFORMATION,
            &mut string,
            &mut chars,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let allocation = Allocation(string.cast());
    // SAFETY: chars includes the terminating nul for the conversion result.
    let result = String::from_utf16(unsafe {
        std::slice::from_raw_parts(string, chars.saturating_sub(1) as usize)
    })
    .map_err(|_| io::Error::other("invalid ACL string"));
    drop(allocation);
    result
}
