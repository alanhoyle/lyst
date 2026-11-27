//! User and group names for Unix file ownership, looked up once per id.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;

thread_local! {
    static USERS: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
    static GROUPS: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
}

/// `owner:group` as `chown` takes it, with numbers for ids that have no name.
pub fn owner_group(uid: u32, gid: u32) -> String {
    format!("{}:{}", user(uid), group(gid))
}

pub fn user(uid: u32) -> String {
    USERS.with(|c| c.borrow_mut().entry(uid).or_insert_with(|| lookup_user(uid).unwrap_or_else(|| uid.to_string())).clone())
}

pub fn group(gid: u32) -> String {
    GROUPS.with(|c| c.borrow_mut().entry(gid).or_insert_with(|| lookup_group(gid).unwrap_or_else(|| gid.to_string())).clone())
}

/// Calls a `get*id_r` function, growing the buffer while it says ERANGE.
fn with_buffer(mut call: impl FnMut(&mut [libc::c_char]) -> libc::c_int) -> bool {
    let mut buf = vec![0; 1024];
    loop {
        match call(&mut buf) {
            0 => return true,
            libc::ERANGE if buf.len() < 1 << 20 => buf.resize(buf.len() * 2, 0),
            _ => return false,
        }
    }
}

fn lookup_user(uid: u32) -> Option<String> {
    // SAFETY: passwd is plain data; getpwuid_r fills it with pointers into
    // `buf`, which are read before `buf` is dropped.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let mut name = None;
    with_buffer(|buf| {
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == 0 && !found.is_null() && !pwd.pw_name.is_null() {
            name = Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned());
        }
        rc
    });
    name
}

fn lookup_group(gid: u32) -> Option<String> {
    // SAFETY: as for lookup_user.
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::group = std::ptr::null_mut();
    let mut name = None;
    with_buffer(|buf| {
        let rc = unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == 0 && !found.is_null() && !grp.gr_name.is_null() {
            name = Some(unsafe { CStr::from_ptr(grp.gr_name) }.to_string_lossy().into_owned());
        }
        rc
    });
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers() {
        assert_eq!(user(0), "root");
        // Group 0 is "wheel" on macOS and BSD, "root" on Linux.
        assert!(["root", "wheel"].contains(&group(0).as_str()), "{}", group(0));
        // An id nobody has comes back as the number.
        assert_eq!(user(4_000_000_123), "4000000123");
        assert_eq!(owner_group(4_000_000_123, 4_000_000_124), "4000000123:4000000124");
    }
}
