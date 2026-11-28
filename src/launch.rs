//! Starting programs the way GIO's `g_app_info_launch` does, without GIO.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::desktop::AppEntry;

/// Resolves a program like `g_find_program_for_path`: a path containing a
/// slash is checked directly, anything else is searched in `$PATH`.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let is_exec = |p: &Path| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if program.contains('/') {
        let p = PathBuf::from(program);
        return is_exec(&p).then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| is_exec(p))
}

/// Splits a command line like `g_shell_parse_argv`: quotes and backslashes
/// as in sh, `#` comments, no expansion. None on bad quoting or no words.
pub fn shell_parse_argv(s: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    args.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '#' if !in_word => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('$' | '`' | '"' | '\\') => cur.push(c),
                            '\n' => {}
                            c => {
                                cur.push('\\');
                                cur.push(c);
                            }
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => cur.push(c),
                    None => cur.push('\\'),
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        args.push(cur);
    }
    (!args.is_empty()).then_some(args)
}

/// Quotes a string for the shell like `g_shell_quote`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Expands an `Exec=` value with no files or URIs, as GIO does: %f %F %u
/// %U and the deprecated codes vanish, %i becomes `--icon <Icon>`, %c the
/// name, %k the desktop file, %% a percent sign.
pub fn expand_exec(exec: &str, entry: &AppEntry) -> Option<Vec<String>> {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('i') => {
                if let Some(icon) = &entry.icon_value {
                    out.push_str("--icon ");
                    out.push_str(&shell_quote(icon));
                }
            }
            Some('c') => out.push_str(&shell_quote(&entry.name)),
            Some('k') => out.push_str(&shell_quote(&entry.desktop_path)),
            _ => {}
        }
    }
    shell_parse_argv(&out)
}

/// Terminals to wrap `Terminal=true` apps in: GIO's list, then a few
/// common ones it does not know.
const TERMINALS: &[(&str, &[&str])] = &[
    ("xdg-terminal-exec", &[]),
    ("kgx", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("mate-terminal", &["-x"]),
    ("xfce4-terminal", &["-x"]),
    ("tilix", &["-e"]),
    ("konsole", &["-e"]),
    ("nxterm", &["-e"]),
    ("color-xterm", &["-e"]),
    ("rxvt", &["-e"]),
    ("dtterm", &["-e"]),
    ("xterm", &["-e"]),
    ("kitty", &[]),
    ("foot", &[]),
    ("alacritty", &["-e"]),
    ("ghostty", &["-e"]),
    ("wezterm", &["start", "--"]),
];

fn terminal_prefix() -> Option<Vec<String>> {
    TERMINALS.iter().find_map(|(name, args)| {
        find_program(name).map(|p| {
            let mut v = vec![p.to_string_lossy().into_owned()];
            v.extend(args.iter().map(|a| a.to_string()));
            v
        })
    })
}

/// Launches a desktop entry.
pub fn launch_entry(entry: &AppEntry) -> io::Result<()> {
    let exec = entry.exec.as_deref().filter(|e| !e.is_empty()).ok_or_else(|| io::Error::other("no Exec line"))?;
    let mut argv = expand_exec(exec, entry).ok_or_else(|| io::Error::other("cannot parse Exec line"))?;
    if entry.terminal {
        let mut t = terminal_prefix().ok_or_else(|| io::Error::other("no terminal found"))?;
        t.append(&mut argv);
        argv = t;
    }
    let env = [("GIO_LAUNCHED_DESKTOP_FILE".to_string(), entry.desktop_path.clone())];
    spawn_detached(&argv, entry.working_dir.as_deref(), &env, false)
}

/// Starts a program fully detached (new session, reparented to init) and
/// reports whether it could be executed. With `quiet`, stdout and stderr go
/// to /dev/null.
pub fn spawn_detached(argv: &[String], cwd: Option<&str>, env: &[(String, String)], quiet: bool) -> io::Result<()> {
    let cstr = |s: &str| CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    if argv.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let args: Vec<CString> = argv.iter().map(|a| cstr(a)).collect::<io::Result<_>>()?;
    let mut arg_ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    arg_ptrs.push(std::ptr::null());

    let mut envs: Vec<CString> = Vec::new();
    for (k, v) in std::env::vars_os() {
        if env.iter().any(|(ek, _)| ek.as_bytes() == k.as_bytes()) {
            continue;
        }
        let mut kv = k.as_bytes().to_vec();
        kv.push(b'=');
        kv.extend_from_slice(v.as_bytes());
        if let Ok(c) = CString::new(kv) {
            envs.push(c);
        }
    }
    for (k, v) in env {
        envs.push(cstr(&format!("{k}={v}"))?);
    }
    let mut env_ptrs: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    let cwd = cwd.map(cstr).transpose()?;
    let devnull = cstr("/dev/null")?;

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (rd, wr) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = io::Error::last_os_error();
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
        return Err(err);
    }
    if pid == 0 {
        // Child: only async-signal-safe calls from here on.
        unsafe {
            libc::close(rd);
            libc::setsid();
            let grandchild = libc::fork();
            if grandchild != 0 {
                if grandchild < 0 {
                    let e = *libc::__errno_location();
                    libc::write(wr, &e as *const i32 as *const libc::c_void, 4);
                }
                libc::_exit(0);
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            if quiet {
                let fd = libc::open(devnull.as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    if fd > 2 {
                        libc::close(fd);
                    }
                }
            }
            if let Some(dir) = &cwd {
                if libc::chdir(dir.as_ptr()) != 0 {
                    let e = *libc::__errno_location();
                    libc::write(wr, &e as *const i32 as *const libc::c_void, 4);
                    libc::_exit(127);
                }
            }
            libc::execvpe(arg_ptrs[0], arg_ptrs.as_ptr(), env_ptrs.as_ptr());
            let e = *libc::__errno_location();
            libc::write(wr, &e as *const i32 as *const libc::c_void, 4);
            libc::_exit(127);
        }
    }

    unsafe {
        libc::close(wr);
        let mut status = 0;
        while libc::waitpid(pid, &mut status, 0) < 0 && *libc::__errno_location() == libc::EINTR {}
        let mut errno: i32 = 0;
        let n = loop {
            let n = libc::read(rd, &mut errno as *mut i32 as *mut libc::c_void, 4);
            if n < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            break n;
        };
        libc::close(rd);
        if n == 4 {
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> AppEntry {
        AppEntry {
            name: "It's".into(),
            name_lower: "it's".into(),
            search_text: String::new(),
            icon: None,
            icon_value: Some("foo".into()),
            desktop_id: "x.desktop".into(),
            desktop_path: "/a b/x.desktop".into(),
            categories: String::new(),
            exec: None,
            terminal: false,
            working_dir: None,
        }
    }

    #[test]
    fn parses_like_g_shell_parse_argv() {
        assert_eq!(shell_parse_argv("a 'b c' \"d \\\"e\\\"\" f\\ g").unwrap(), vec!["a", "b c", "d \"e\"", "f g"]);
        assert_eq!(shell_parse_argv("a # comment").unwrap(), vec!["a"]);
        assert!(shell_parse_argv("'open").is_none());
        assert!(shell_parse_argv("   ").is_none());
    }

    #[test]
    fn expands_field_codes() {
        let e = entry();
        assert_eq!(
            expand_exec("/usr/bin/flatpak run app @@u %U @@", &e).unwrap(),
            vec!["/usr/bin/flatpak", "run", "app", "@@u", "@@"]
        );
        assert_eq!(expand_exec("prog %i %c %k 100%%", &e).unwrap(), vec!["prog", "--icon", "foo", "It's", "/a b/x.desktop", "100%"]);
    }

    #[test]
    fn spawn_reports_missing_programs() {
        assert!(spawn_detached(&["/nonexistent/program".into()], None, &[], true).is_err());
        assert!(spawn_detached(&["true".into()], None, &[], true).is_ok());
    }
}
