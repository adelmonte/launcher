mod app;
mod desktop;
mod launch;
mod lockfile;
mod watch;

use std::io::Read;
use std::os::unix::net::UnixStream;

use gui_toolkit::calloop::generic::Generic;
use gui_toolkit::calloop::{Interest, Mode, PostAction};

/// A self-pipe that becomes readable when one of `signals` arrives.
fn signal_pipe(signals: &[i32]) -> Option<UnixStream> {
    let (read, write) = UnixStream::pair().ok()?;
    read.set_nonblocking(true).ok()?;
    for &sig in signals {
        signal_hook::low_level::pipe::register(sig, write.try_clone().ok()?).ok()?;
    }
    Some(read)
}

fn drain(stream: &mut UnixStream) {
    let mut buf = [0u8; 64];
    while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
}

fn main() {
    // Fast path: if a daemon is already running, signal it and exit immediately.
    if lockfile::fast_path() {
        std::process::exit(0);
    }

    let start_hidden = std::env::args().any(|a| a == "--daemon");
    // Use our own activation token for our first window, and don't hand it
    // down to the apps we launch.
    let activation_token = std::env::var("XDG_ACTIVATION_TOKEN").ok().filter(|_| !start_hidden);
    std::env::remove_var("XDG_ACTIVATION_TOKEN");
    std::env::remove_var("DESKTOP_STARTUP_ID");

    lockfile::write_lock();

    let result = gui_toolkit::run(|cx| {
        if let Some(pipe) = signal_pipe(&[libc::SIGUSR1]) {
            cx.insert_source(Generic::new(pipe, Interest::READ, Mode::Level), |_, pipe, app: &mut app::App, cx| {
                // SAFETY: the stream is only read, never closed here.
                drain(unsafe { pipe.get_mut() });
                app.toggle_visibility(cx);
                Ok(PostAction::Continue)
            });
        }
        if let Some(pipe) = signal_pipe(&[libc::SIGTERM, libc::SIGINT]) {
            cx.insert_source(Generic::new(pipe, Interest::READ, Mode::Level), |_, _, _, cx| {
                cx.quit();
                Ok(PostAction::Continue)
            });
        }
        if let Ok(watch) = watch::DirWatch::new(&desktop::watch_dirs()) {
            cx.insert_source(Generic::new(watch, Interest::READ, Mode::Level), |_, watch, app: &mut app::App, cx| {
                if watch.read_desktop_changes() {
                    app.on_desktop_dir_changed(cx);
                }
                Ok(PostAction::Continue)
            });
        }
        app::App::new(cx, start_hidden, activation_token)
    });

    lockfile::cleanup_lock();
    if let Err(e) = result {
        eprintln!("launcher: {e}");
        std::process::exit(1);
    }
}
