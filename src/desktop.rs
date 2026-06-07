use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use gtk::gio;
use gtk::gio::prelude::*;

#[derive(Clone)]
pub struct AppEntry {
    pub name: String,
    pub name_lower: String,
    pub search_text: String,
    pub description: String,
    pub icon: Option<gio::Icon>,
    pub desktop_id: String,
    pub desktop_path: String,
    pub app_info: gio::DesktopAppInfo,
    pub keywords: String,
    pub generic_name: String,
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
    let h = home();
    vec![
        h.join(".local/share/applications"),
        h.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ]
}

struct Slot {
    priority: i32,
    entry: Option<AppEntry>,
}

pub fn load_applications() -> Vec<AppEntry> {
    let mut by_key: HashMap<String, Slot> = HashMap::new();

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

            let app_info = match gio::DesktopAppInfo::from_filename(&file_path) {
                Some(ai) => ai,
                None => {
                    by_key.insert(id_key, Slot { priority, entry: None });
                    continue;
                }
            };

            if app_info.is_nodisplay() || app_info.is_hidden() {
                // Mark as hidden so lower-priority versions don't show up.
                by_key.insert(id_key, Slot { priority, entry: None });
                continue;
            }

            let name = app_info.name().to_string();
            if name.is_empty() {
                continue;
            }

            let description = app_info
                .description()
                .map(|g| g.to_string())
                .unwrap_or_default();
            let icon = app_info.icon();
            let keywords = app_info
                .keywords()
                .iter()
                .map(|g| g.as_str().to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            let generic_name = app_info
                .generic_name()
                .map(|g| g.as_str().to_lowercase())
                .unwrap_or_default();

            let name_lower = name.to_lowercase();
            let search_text =
                format!("{} {} {}", description.to_lowercase(), keywords, generic_name);

            let entry = AppEntry {
                name,
                name_lower,
                search_text,
                description,
                icon,
                desktop_id: desktop_id.clone(),
                desktop_path: file_path.to_string_lossy().to_string(),
                app_info,
                keywords,
                generic_name,
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
        _ => return None,
    })
}

/// Map app indices into category buckets. BTreeMap keeps the categories sorted
/// (matching `sorted(self.categories.keys())` in the Python).
pub fn organize_by_category(apps: &[AppEntry]) -> BTreeMap<String, Vec<usize>> {
    let mut categories: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, app) in apps.iter().enumerate() {
        let cats = app.app_info.categories();
        let cats = match cats {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => {
                categories.entry("Other".to_string()).or_default().push(i);
                continue;
            }
        };

        let mut categorized = false;
        for cat in cats.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Some(target) = map_category(cat) {
                categories.entry(target.to_string()).or_default().push(i);
                categorized = true;
                break;
            }
        }

        if !categorized {
            categories.entry("Other".to_string()).or_default().push(i);
        }
    }

    categories
}
