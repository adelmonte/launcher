mod app;
mod desktop;
mod icon;
mod lockfile;

use gtk::glib;

fn main() {
    // Fast path: if a daemon is already running, signal it and exit immediately.
    if lockfile::fast_path() {
        std::process::exit(0);
    }

    let start_hidden = std::env::args().any(|a| a == "--daemon");

    // Set before gtk::init so the X11/Wayland window identity (WM_CLASS / app_id)
    // is stable for window-manager rules.
    glib::set_prgname(Some("launcher"));

    if gtk::init().is_err() {
        eprintln!("Failed to initialize GTK");
        std::process::exit(1);
    }

    lockfile::write_lock();

    let launcher = app::App::new(start_hidden);

    {
        let launcher = launcher.clone();
        glib::unix_signal_add_local(libc::SIGUSR1, move || {
            let launcher = launcher.clone();
            glib::idle_add_local_once(move || launcher.toggle_visibility());
            glib::ControlFlow::Continue
        });
    }
    glib::unix_signal_add_local(libc::SIGTERM, || {
        lockfile::cleanup_lock();
        gtk::main_quit();
        glib::ControlFlow::Break
    });
    glib::unix_signal_add_local(libc::SIGINT, || {
        lockfile::cleanup_lock();
        gtk::main_quit();
        glib::ControlFlow::Break
    });

    gtk::main();
    lockfile::cleanup_lock();
}
