use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use gui_toolkit::Icon;

use crate::launch::{find_program, shell_parse_argv};

#[derive(Clone)]
pub struct AppEntry {
    pub name: String,
    pub name_lower: String,
    pub search_text: String,
    pub icon: Option<Icon>,
    /// The raw `Icon=` value, for `%i`.
    pub icon_value: Option<String>,
    pub desktop_id: String,
    pub desktop_path: String,
    pub categories: String,
    pub exec: Option<String>,
    pub terminal: bool,
    pub working_dir: Option<String>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// (path, priority) — higher priority wins on duplicate desktop ids.
fn search_paths() -> Vec<(PathBuf, i32)> {
    let h = home();
    vec![
        (h.join(".local/share/applications"), 4),
        (h.join(".local/share/flatpak/exports/share/applications"), 3),
        (PathBuf::from("/usr/share/applications"), 2),
        (PathBuf::from("/var/lib/flatpak/exports/share/applications"), 1),
    ]
}

pub fn watch_dirs() -> Vec<PathBuf> {
    search_paths().into_iter().map(|(p, _)| p).collect()
}

struct Slot {
    priority: i32,
    entry: Option<AppEntry>,
}

pub fn load_applications() -> Vec<AppEntry> {
    let mut by_key: HashMap<String, Slot> = HashMap::new();
    let languages = language_names();

    for (path, priority) in search_paths() {
        if !path.exists() {
            continue;
        }
        let dir = match std::fs::read_dir(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        for entry in dir.flatten() {
            let file_path = entry.path();
            let fname = match file_path.file_name().and_then(|s| s.to_str()) {
                Some(f) => f.to_string(),
                None => continue,
            };
            if !fname.ends_with(".desktop") {
                continue;
            }

            let desktop_id = fname;
            let id_key = desktop_id.to_lowercase();

            // Already have an equal-or-higher priority entry for this id.
            if let Some(slot) = by_key.get(&id_key) {
                if slot.priority >= priority {
                    continue;
                }
            }

            let info = match DesktopInfo::load(&file_path, &languages) {
                Some(info) => info,
                None => {
                    by_key.insert(id_key, Slot { priority, entry: None });
                    continue;
                }
            };

            if info.no_display || info.hidden {
                // Mark as hidden so lower-priority versions don't show up.
                by_key.insert(id_key, Slot { priority, entry: None });
                continue;
            }

            let name = info.name.clone();
            if name.is_empty() {
                continue;
            }

            let keywords = info.keywords.iter().map(|k| k.to_lowercase()).collect::<Vec<_>>().join(" ");
            let generic_name = info.generic_name.as_deref().unwrap_or_default().to_lowercase();
            let description = info.comment.as_deref().unwrap_or_default();

            let name_lower = name.to_lowercase();
            let search_text = format!("{} {} {}", description.to_lowercase(), keywords, generic_name);

            let entry = AppEntry {
                name,
                name_lower,
                search_text,
                icon: info.icon(),
                icon_value: info.icon.clone(),
                desktop_id: desktop_id.clone(),
                desktop_path: file_path.to_string_lossy().to_string(),
                categories: info.categories.unwrap_or_default(),
                exec: info.exec,
                terminal: info.terminal,
                working_dir: info.path,
            };

            by_key.insert(id_key, Slot { priority, entry: Some(entry) });
        }
    }

    let mut apps: Vec<AppEntry> = by_key.into_values().filter_map(|s| s.entry).collect();
    apps.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));
    apps
}

fn map_category(cat: &str) -> Option<&'static str> {
    Some(match cat {
        "AudioVideo" | "Audio" | "Video" => "Multimedia",
        "Development" => "Development",
        "Education" => "Education",
        "Game" => "Games",
        "Graphics" => "Graphics",
        "Network" => "Internet",
        "Office" => "Office",
        "Science" => "Science",
        "Settings" => "Settings",
        "System" => "System Tools",
        "Utility" => "Accessories",
        "HamRadio" => "Multimedia",
        _ => return None,
    })
}

/// Firefox PWAs (firefoxpwa) don't set useful categories.
fn is_web_app(desktop_id: &str) -> bool {
    desktop_id.starts_with("FFPWA-")
}

/// Map app indices into category buckets. BTreeMap keeps the categories sorted.
pub fn organize_by_category(apps: &[AppEntry]) -> BTreeMap<String, Vec<usize>> {
    let mut categories: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, app) in apps.iter().enumerate() {
        let target = app
            .categories
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .find_map(map_category)
            .or_else(|| is_web_app(&app.desktop_id).then_some("Internet"))
            .unwrap_or("Other");

        categories.entry(target.to_string()).or_default().push(i);
    }

    categories
}

/// The `[Desktop Entry]` fields the launcher uses, read the way GIO's
/// `g_desktop_app_info_new_from_filename` reads them.
pub struct DesktopInfo {
    pub name: String,
    pub comment: Option<String>,
    pub generic_name: Option<String>,
    pub keywords: Vec<String>,
    pub icon: Option<String>,
    pub categories: Option<String>,
    pub exec: Option<String>,
    pub path: Option<String>,
    pub terminal: bool,
    pub hidden: bool,
    pub no_display: bool,
}

impl DesktopInfo {
    /// None when GIO would reject the file: unreadable, not starting with a
    /// `[Desktop Entry]` group, not `Type=Application`, or naming a
    /// `TryExec`/`Exec` program that is not installed.
    pub fn load(path: &Path, languages: &[String]) -> Option<DesktopInfo> {
        let src = std::fs::read_to_string(path).ok()?;
        let group = parse_desktop_group(&src)?;
        let get = |key: &str| group.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let string = |key: &str| get(key).and_then(unescape);
        let locale_string = |key: &str| {
            for lang in languages {
                if let Some(v) = get(&format!("{key}[{lang}]")).and_then(unescape) {
                    return Some(v);
                }
            }
            string(key)
        };
        let locale_list = |key: &str| {
            for lang in languages {
                if let Some(v) = get(&format!("{key}[{lang}]")) {
                    return split_list(v);
                }
            }
            get(key).map(split_list).unwrap_or_default()
        };
        let boolean = |key: &str| matches!(get(key).map(str::trim), Some("true" | "1"));

        if string("Type").as_deref() != Some("Application") {
            return None;
        }
        if let Some(try_exec) = string("TryExec").filter(|t| !t.is_empty()) {
            find_program(&try_exec)?;
        }
        let exec = string("Exec");
        if let Some(e) = exec.as_deref().filter(|e| !e.is_empty()) {
            let argv = shell_parse_argv(e)?;
            find_program(argv.first()?)?;
        }

        Some(DesktopInfo {
            name: locale_string("Name").unwrap_or_else(|| "Unnamed".to_string()),
            comment: locale_string("Comment"),
            generic_name: locale_string("GenericName"),
            keywords: locale_list("Keywords"),
            icon: locale_string("Icon").filter(|i| !i.is_empty()),
            categories: string("Categories"),
            exec,
            path: string("Path").filter(|p| !p.is_empty()),
            terminal: boolean("Terminal"),
            hidden: boolean("Hidden"),
            no_display: boolean("NoDisplay"),
        })
    }

    /// The icon as GIO resolves it: an absolute path is a file, anything
    /// else a theme name with any image extension dropped.
    pub fn icon(&self) -> Option<Icon> {
        icon_from_value(self.icon.as_deref()?)
    }
}

pub fn icon_from_value(value: &str) -> Option<Icon> {
    if value.is_empty() {
        return None;
    }
    if value.starts_with('/') {
        return Some(Icon::Path(PathBuf::from(value)));
    }
    let name = [".png", ".xpm", ".svg"]
        .iter()
        .find_map(|ext| value.strip_suffix(ext))
        .unwrap_or(value);
    Some(Icon::Exact(name.to_string()))
}

/// The key/value pairs of the first group, if it is `[Desktop Entry]`.
fn parse_desktop_group(src: &str) -> Option<Vec<(String, String)>> {
    let mut in_group = false;
    let mut seen_group = false;
    let mut out = Vec::new();
    for raw in src.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let name = line.trim_end().strip_prefix('[')?.strip_suffix(']')?;
            if !seen_group {
                if name != "Desktop Entry" {
                    return None;
                }
                seen_group = true;
                in_group = true;
            } else {
                // A repeated [Desktop Entry] group merges into the first.
                in_group = name == "Desktop Entry";
            }
            continue;
        }
        let (k, v) = line.split_once('=')?;
        if !seen_group {
            return None;
        }
        if in_group {
            out.push((k.trim_end().to_string(), v.trim_start().to_string()));
        }
    }
    seen_group.then_some(out)
}

/// Key-file string unescaping: `\s \n \t \r \\`.
fn unescape(v: &str) -> Option<String> {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            // GLib rejects the whole value on an unknown escape.
            _ => return None,
        }
    }
    Some(out)
}

/// A `;`-separated key-file list (empty if GLib would reject it).
fn split_list(v: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => cur.push(';'),
                Some('s') => cur.push(' '),
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some('r') => cur.push('\r'),
                Some('\\') => cur.push('\\'),
                _ => return Vec::new(),
            },
            ';' => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        items.push(cur);
    }
    items
}

/// Locale names to try for localized keys, most specific first, like
/// `g_get_language_names()`: "en_US.UTF-8" → en_US.UTF-8, en_US, en.UTF-8, en.
pub fn language_names() -> Vec<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut locales: Vec<String> = Vec::new();
    let primary = var("LC_ALL").or_else(|| var("LC_MESSAGES")).or_else(|| var("LANG"));
    if let Some(list) = var("LANGUAGE").filter(|_| primary.as_deref() != Some("C")) {
        locales.extend(list.split(':').filter(|s| !s.is_empty()).map(str::to_string));
    }
    if let Some(p) = primary {
        locales.push(p);
    }
    let mut out: Vec<String> = Vec::new();
    for loc in locales {
        for v in locale_variants(&loc) {
            if v != "C" && v != "POSIX" && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

fn locale_variants(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((r, m)) => (r, Some(m)),
        None => (locale, None),
    };
    let (rest, encoding) = match rest.split_once('.') {
        Some((r, e)) => (r, Some(e)),
        None => (rest, None),
    };
    let (lang, territory) = match rest.split_once('_') {
        Some((l, t)) => (l, Some(t)),
        None => (rest, None),
    };
    let mut out = Vec::new();
    for t in [territory, None] {
        for e in [encoding, None] {
            for m in [modifier, None] {
                let mut s = lang.to_string();
                if let Some(t) = t {
                    s.push('_');
                    s.push_str(t);
                }
                if let Some(e) = e {
                    s.push('.');
                    s.push_str(e);
                }
                if let Some(m) = m {
                    s.push('@');
                    s.push_str(m);
                }
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_variants_match_glib_order() {
        assert_eq!(locale_variants("en_US.UTF-8"), vec!["en_US.UTF-8", "en_US", "en.UTF-8", "en"]);
        assert_eq!(locale_variants("de"), vec!["de"]);
    }

    #[test]
    fn rejects_files_gio_rejects() {
        assert!(parse_desktop_group("[Other]\nA=b\n[Desktop Entry]\nType=Application\n").is_none());
        assert!(parse_desktop_group("garbage line\n").is_none());
        let g = parse_desktop_group("# c\n[Desktop Entry]\nName = Foo\n[Desktop Action x]\nName=Bar\n").unwrap();
        assert_eq!(g, vec![("Name".to_string(), "Foo".to_string())]);
    }

    #[test]
    fn lists_and_escapes() {
        assert_eq!(split_list("a;b\\;c;;"), vec!["a", "b;c", ""]);
        assert_eq!(unescape("a\\sb\\\\c").unwrap(), "a b\\c");
    }

    #[test]
    fn icons_like_gio() {
        assert_eq!(icon_from_value("firefox.png"), Some(Icon::Exact("firefox".into())));
        assert_eq!(icon_from_value("/x/y.png"), Some(Icon::Path("/x/y.png".into())));
    }
}
