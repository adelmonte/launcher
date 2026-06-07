use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub const LOCK_FILE: &str = "/tmp/launcher.lock";

/// Fast path: if a daemon is already running, signal it and report that we
/// should exit. Mirrors the pre-GTK lock check in the original launcher.
/// Returns `true` if an existing live daemon was signalled (caller exits 0).
pub fn fast_path() -> bool {
    let path = Path::new(LOCK_FILE);
    if !path.exists() {
        return false;
    }

    let first_line = fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()));

    match first_line.and_then(|l| l.parse::<i32>().ok()) {
        Some(pid) => {
            if pid != std::process::id() as i32 {
                let alive = unsafe { libc::kill(pid, 0) == 0 };
                if alive {
                    unsafe {
                        libc::kill(pid, libc::SIGUSR1);
                    }
                    return true;
                }
                // dead -> stale lock
                let _ = fs::remove_file(path);
            }
            false
        }
        None => {
            // parse / read error -> remove the stale lock
            let _ = fs::remove_file(path);
            false
        }
    }
}

pub fn write_lock() {
    if let Ok(mut f) = fs::File::create(LOCK_FILE) {
        let _ = write!(f, "{}", std::process::id());
    }
}

pub fn signal_waybar(visible: bool) {
    if let Ok(mut f) = fs::File::create(LOCK_FILE) {
        let _ = write!(
            f,
            "{}\n{}",
            std::process::id(),
            if visible { "visible" } else { "hidden" }
        );
    }
    let _ = Command::new("pkill")
        .arg("-RTMIN+8")
        .arg("waybar")
        .stderr(Stdio::null())
        .status();
}

pub fn cleanup_lock() {
    let _ = fs::remove_file(LOCK_FILE);
    // Signal waybar that launcher is inactive
    let _ = Command::new("pkill")
        .arg("-RTMIN+8")
        .arg("waybar")
        .stderr(Stdio::null())
        .status();
}
