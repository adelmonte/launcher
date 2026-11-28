use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// An inotify instance watching directories for changed `.desktop` files.
pub struct DirWatch {
    fd: OwnedFd,
}

impl DirWatch {
    pub fn new(dirs: &[impl AsRef<Path>]) -> io::Result<DirWatch> {
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_ATTRIB;
        for dir in dirs {
            let dir = dir.as_ref();
            if !dir.exists() {
                continue;
            }
            if let Ok(c) = CString::new(dir.as_os_str().as_bytes()) {
                unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) };
            }
        }
        Ok(DirWatch { fd })
    }

    /// Drains pending events; true if any concerned a `.desktop` file.
    pub fn read_desktop_changes(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut changed = false;
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            let header = std::mem::size_of::<libc::inotify_event>();
            let mut off = 0;
            while off + header <= n {
                let ev = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::inotify_event) };
                let name_len = ev.len as usize;
                let name = &buf[off + header..(off + header + name_len).min(n)];
                let name = name.split(|b| *b == 0).next().unwrap_or_default();
                if name.ends_with(b".desktop") {
                    changed = true;
                }
                off += header + name_len;
            }
        }
        changed
    }
}

impl AsFd for DirWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
