//! Owner-only security descriptors for the app's named kernel objects.
//!
//! A named mutex created with a NULL descriptor gets the caller's default DACL, which on
//! a shared or multi-user machine lets another account's process open (and hold, or
//! release) the object. The single-instance guards only need the creating user to reach
//! them, so they are created with a DACL that grants access to that user's SID and nobody
//! else. Everything the descriptor points at lives on the stack of
//! [`with_user_only_dacl`], which is why the attributes are lent to a closure rather than
//! returned.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, GENERIC_ALL, HANDLE, WIN32_ERROR};
use windows::Win32::Security::{
    AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL,
    ACL_REVISION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};

/// `SECURITY_DESCRIPTOR_REVISION` from winnt.h; the `windows` feature that exports it is
/// not enabled and the value has never changed.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// Runs `f` with a `SECURITY_ATTRIBUTES` whose DACL grants access to the current user's
/// SID and nobody else. `None` when the descriptor could not be built (the token could not
/// be read, say); callers fall back to the default descriptor in that case rather than
/// failing the operation.
pub(crate) unsafe fn with_user_only_dacl<T>(
    f: impl FnOnce(&SECURITY_ATTRIBUTES) -> Option<T>,
) -> Option<T> {
    let user = current_token_user()?;
    // SAFETY: `user` holds a TOKEN_USER followed by the SID bytes its `Sid` points at, in
    // an 8-byte-aligned buffer that outlives every use below.
    let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
    let sid_len = GetLengthSid(sid) as usize;
    if sid_len == 0 {
        return None;
    }
    // One ACCESS_ALLOWED_ACE: its `SidStart` u32 is replaced by the SID itself. ACL sizes
    // are DWORD multiples, and a `Vec<u32>` gives the alignment the ACL needs.
    let ace_len = core::mem::size_of::<ACCESS_ALLOWED_ACE>() - core::mem::size_of::<u32>();
    let acl_len = (core::mem::size_of::<ACL>() + ace_len + sid_len + 3) & !3;
    let mut acl_buf = vec![0u32; acl_len / 4];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    InitializeAcl(acl, acl_len as u32, ACL_REVISION).ok()?;
    AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL.0, sid).ok()?;
    let mut sd = SECURITY_DESCRIPTOR::default();
    let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut SECURITY_DESCRIPTOR as *mut c_void);
    InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION).ok()?;
    SetSecurityDescriptorDacl(psd, true, Some(acl as *const ACL), false).ok()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    f(&sa)
}

/// The `TOKEN_USER` block of this process's token, in an 8-byte-aligned buffer.
unsafe fn current_token_user() -> Option<Vec<u64>> {
    let mut token = HANDLE::default();
    OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    let filled = GetTokenInformation(
        token,
        TokenUser,
        Some(buf.as_mut_ptr() as *mut c_void),
        (buf.len() * 8) as u32,
        &mut needed,
    )
    .is_ok();
    let _ = CloseHandle(token);
    filled.then_some(buf)
}

/// `CreateMutexW` with an owner-only DACL, falling back to the default descriptor when
/// one cannot be built. Returns the creation result together with the `GetLastError`
/// value read immediately after the call, because the single-instance guards need
/// `ERROR_ALREADY_EXISTS` and nothing may run between the two calls that could clobber
/// it (dropping the descriptor's buffers, for one).
pub(crate) unsafe fn create_mutex_user_only(
    initial_owner: bool,
    name: PCWSTR,
) -> (windows::core::Result<HANDLE>, WIN32_ERROR) {
    let created = with_user_only_dacl(|sa| {
        let sa: *const SECURITY_ATTRIBUTES = sa;
        let r = CreateMutexW(Some(sa), initial_owner, name);
        Some((r, GetLastError()))
    });
    match created {
        Some(pair) => pair,
        None => {
            let r = CreateMutexW(None, initial_owner, name);
            (r, GetLastError())
        }
    }
}
