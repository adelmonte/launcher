use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gui_toolkit::{
    ease_out_cubic, Color, CursorIcon, Cx, DrawCx, Event, Icon, KeyEvent, KeyResponse, Keysym, PopupOptions,
    ScrollEvent, Scroller, Shadow, TextInput, TextStyle, TimerId, ToplevelOptions, WindowId, BTN_LEFT, BTN_RIGHT,
};

use crate::desktop::{self, AppEntry};
use crate::launch::{launch_entry, spawn_detached};
use crate::lockfile;

const DEFAULT_SIZE: (u32, u32) = (250, 400);

// Geometry, in logical pixels, matching the GTK 3 layout it replaces.
const ROW_H: f32 = 38.0;
const MARGIN: f32 = 5.0;
const SPACING: f32 = 10.0;
const APP_ICON: f32 = 32.0;
const CATEGORY_ICON: f32 = 24.0;
const SMALL_ICON: f32 = 16.0;
/// Stars and chevrons, drawn inside a SMALL_ICON box.
const GLYPH_ICON: f32 = 11.0;
const STAR_BUTTON: (f32, f32) = (34.0, 26.0);
const NAV_H: f32 = 35.0;
const SEPARATOR_H: f32 = 5.0;

const TRANSITION: Duration = Duration::from_millis(250);
const TRANSITION_LOCK: Duration = Duration::from_millis(260);

const LABEL_SHADOWS: [Shadow; 2] = [
    Shadow { dx: 0.0, dy: 1.0, blur: 2.0, color: Color::rgba8(0, 0, 0, 230) },
    Shadow { dx: 0.0, dy: 1.0, blur: 4.0, color: Color::rgba8(0, 0, 0, 153) },
];
const SEPARATOR: Color = Color::rgba8(255, 255, 255, 51);

#[derive(Clone)]
enum View {
    Favorites,
    Categories,
    Category(String),
}

#[derive(Clone)]
enum Row {
    App { idx: usize, favorite: bool, draggable: bool },
    Category { name: String, icon: &'static str },
}

#[derive(Clone, Copy, PartialEq)]
enum NavAction {
    ShowCategories,
    GoBack,
}

struct NavButton {
    forward: bool,
    label: &'static str,
    icon: &'static str,
    action: NavAction,
}

#[derive(Default)]
struct List {
    rows: Vec<Row>,
    selected: Option<usize>,
    scroll: Scroller,
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Forward,
    Back,
}

struct Slide {
    from: usize,
    direction: Direction,
    start: Instant,
}

enum MenuAction {
    Launch(usize),
    OpenLocation(String),
}

struct Menu {
    win: WindowId,
    items: Vec<Option<(String, MenuAction)>>,
    hover: Option<usize>,
}

struct Layout {
    w: f32,
    list_h: f32,
    sep1_y: f32,
    nav_y: f32,
    sep2_y: f32,
    entry: (f32, f32, f32, f32),
}

fn favorites_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".config")
        .join("launcher-favorites.json")
}

fn load_favorites() -> Vec<String> {
    let path = favorites_path();
    if path.exists() {
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&s) {
                return v;
            }
        }
        return Vec::new();
    }
    Vec::new()
}

fn category_icon(name: &str) -> &'static str {
    match name {
        "Multimedia" => "applications-multimedia",
        "Development" => "applications-development",
        "Education" => "applications-science",
        "Games" => "applications-games",
        "Graphics" => "applications-graphics",
        "Internet" => "applications-internet",
        "Office" => "applications-office",
        "Science" => "applications-science",
        "Settings" => "preferences-system",
        "System Tools" => "applications-system",
        "Accessories" => "applications-accessories",
        "Other" => "applications-other",
        _ => "folder",
    }
}

pub struct App {
    win: Option<WindowId>,
    activation_token: Option<String>,
    size: (f32, f32),

    lists: [List; 2],
    current: usize,
    slide: Option<Slide>,
    nav: NavButton,
    entry: TextInput,
    menu: Option<Menu>,

    all_apps: Vec<AppEntry>,
    by_id: HashMap<String, usize>,
    categories: BTreeMap<String, Vec<usize>>,
    favorites: Vec<String>,
    view_stack: Vec<View>,
    /// Selected row index of each view below the top, restored on go_back.
    selection_stack: Vec<usize>,

    dragging: bool,
    drag_row: Option<usize>,
    drag_moved: bool,
    /// Row a primary press landed on; releasing over it activates it.
    pressed_row: Option<usize>,
    pressed_star: Option<usize>,
    keyboard_mode: bool,
    last_pointer: (f64, f64),
    pointer: Option<(f32, f32)>,
    activate_blocked: bool,
    armed_key: Option<Keysym>,
    menu_open: bool,
    is_animating: bool,
    visible: bool,
    focus_out_timeout: Option<TimerId>,
    reload_pending: bool,
}

impl App {
    pub fn new(cx: &mut Cx<App>, start_hidden: bool, activation_token: Option<String>) -> App {
        let mut app = App::build(activation_token);
        if !start_hidden {
            app.open_window(cx);
            app.visible = true;
            app.blur_entry();
            app.publish_state();
        }
        app
    }

    fn build(activation_token: Option<String>) -> App {
        let all_apps = desktop::load_applications();
        let categories = desktop::organize_by_category(&all_apps);
        let mut app = App {
            win: None,
            activation_token,
            size: (DEFAULT_SIZE.0 as f32, DEFAULT_SIZE.1 as f32),
            lists: [List::default(), List::default()],
            current: 0,
            slide: None,
            nav: NavButton { forward: true, label: "All Applications", icon: "folder", action: NavAction::ShowCategories },
            entry: TextInput::new("🔎 Search"),
            menu: None,
            all_apps,
            by_id: HashMap::new(),
            categories,
            favorites: load_favorites(),
            view_stack: Vec::new(),
            selection_stack: Vec::new(),
            dragging: false,
            drag_row: None,
            drag_moved: false,
            pressed_row: None,
            pressed_star: None,
            keyboard_mode: true,
            last_pointer: (f64::NAN, f64::NAN),
            pointer: None,
            activate_blocked: false,
            armed_key: None,
            menu_open: false,
            is_animating: false,
            visible: false,
            focus_out_timeout: None,
            reload_pending: false,
        };
        app.rebuild_by_id();
        app.show_favorites_view();
        app
    }

    fn open_window(&mut self, cx: &mut Cx<App>) {
        if self.win.is_some() {
            return;
        }
        let mut opts = ToplevelOptions::new("Applications", "launcher", DEFAULT_SIZE);
        opts.transparent = true;
        opts.decorations = gui_toolkit::Decorations::None;
        opts.activation_token = self.activation_token.take();
        self.win = Some(cx.open_toplevel(opts));
    }

    fn redraw(&self, cx: &mut Cx<App>) {
        if let Some(w) = self.win {
            cx.redraw(w);
        }
    }

    fn layout(&self, entry_h: f32) -> Layout {
        let (w, h) = self.size;
        let search_y = h - (entry_h + 2.0 * MARGIN);
        let sep2_y = search_y - SEPARATOR_H;
        let nav_y = sep2_y - NAV_H;
        let sep1_y = nav_y - SEPARATOR_H;
        Layout {
            w,
            list_h: sep1_y.max(0.0),
            sep1_y,
            nav_y,
            sep2_y,
            entry: (MARGIN, search_y + MARGIN, (w - 2.0 * MARGIN).max(0.0), entry_h),
        }
    }

    fn layout_cx(&self, cx: &Cx<App>) -> Layout {
        self.layout(cx.theme().entry.height)
    }

    fn max_scroll(&self, list: usize, list_h: f32) -> f32 {
        (self.lists[list].rows.len() as f32 * ROW_H - list_h).max(0.0)
    }

    pub fn toggle_visibility(&mut self, cx: &mut Cx<App>) {
        if self.visible {
            self.hide_launcher(cx);
        } else {
            self.show_launcher(cx);
        }
    }

    fn show_launcher(&mut self, cx: &mut Cx<App>) {
        self.favorites = load_favorites();

        // Reset to favorites view
        self.view_stack.clear();
        self.selection_stack.clear();
        self.set_search_text_silently("");
        self.keyboard_mode = true;
        self.slide = None;
        self.show_favorites_view();

        // Block activation until stale Wayland key events pass.
        self.activate_blocked = true;
        self.armed_key = None;

        self.open_window(cx);
        self.visible = true;
        self.blur_entry();
        self.publish_state();
        self.redraw(cx);

        cx.timeout(Duration::from_millis(150), |app, _| app.activate_blocked = false);
    }

    fn hide_launcher(&mut self, cx: &mut Cx<App>) {
        if let Some(id) = self.focus_out_timeout.take() {
            cx.cancel(id);
        }
        self.close_menu(cx);
        if let Some(w) = self.win.take() {
            cx.close(w);
        }
        self.dragging = false;
        self.drag_row = None;
        self.pressed_row = None;
        self.pressed_star = None;
        self.pointer = None;
        self.visible = false;
        self.publish_state();
    }

    fn publish_state(&self) {
        lockfile::write_state(self.visible);
    }

    fn set_search_text_silently(&mut self, text: &str) {
        self.entry.set_text(text);
    }

    // --- selection ---

    fn select_index(&mut self, list: usize, index: usize, list_h: f32) {
        let count = self.lists[list].rows.len();
        if count == 0 {
            return;
        }
        let i = index.min(count - 1);
        let max = self.max_scroll(list, list_h);
        let l = &mut self.lists[list];
        l.selected = Some(i);
        // Scroll the minimum distance to bring it into view.
        l.scroll.reveal(i as f32 * ROW_H, (i + 1) as f32 * ROW_H, list_h, max);
    }

    fn select_first(&mut self, list: usize) {
        let l = &mut self.lists[list];
        l.selected = if l.rows.is_empty() { None } else { Some(0) };
        l.scroll.set(0.0, 0.0);
    }

    fn move_selection(&mut self, delta: isize, list_h: f32) {
        let list = self.current;
        let count = self.lists[list].rows.len() as isize;
        if count == 0 {
            return;
        }
        let next = match self.lists[list].selected {
            Some(i) => (i as isize + delta).clamp(0, count - 1),
            None if delta > 0 => 0,
            None => count - 1,
        };
        self.select_index(list, next as usize, list_h);
        self.blur_entry_if_empty();
    }

    /// Move the highlight under the mouse, without scrolling.
    fn hover_select(&mut self, row: usize) {
        let l = &mut self.lists[self.current];
        if row < l.rows.len() && l.selected != Some(row) {
            l.selected = Some(row);
            self.blur_entry_if_empty();
        }
    }

    /// Records a pointer position, returning `false` if the pointer has not
    /// actually moved, so stray synthesized events cannot steal the highlight.
    fn note_pointer(&mut self, pos: (f64, f64)) -> bool {
        if self.last_pointer == pos {
            return false;
        }
        self.last_pointer = pos;
        self.keyboard_mode = false;
        true
    }

    fn focus_entry(&mut self) {
        if !self.entry.is_focused() {
            self.entry.focus();
            self.entry.move_to_end();
        }
    }

    /// Unfocused, the empty entry shows its placeholder; the first keypress
    /// hands focus back via `on_key_press`, so nothing is lost by waiting.
    fn blur_entry(&mut self) {
        self.entry.blur();
    }

    /// Moving the selection by key or pointer with nothing typed means the user
    /// is browsing rather than searching, so hand the placeholder back — this
    /// is what restores it after a query is backspaced away.
    fn blur_entry_if_empty(&mut self) {
        if self.entry.text().is_empty() {
            self.blur_entry();
        }
    }

    // --- navigation / views ---

    fn render_view(&mut self, view: &View) -> Vec<Row> {
        match view {
            View::Favorites => self.populate_favorites(),
            View::Categories => self.populate_categories(),
            View::Category(name) => self.populate_category(name),
        }
    }

    fn repopulate(&mut self, list: usize, rows: Vec<Row>) {
        self.lists[list].rows = rows;
        self.select_first(list);
    }

    fn show_favorites_view(&mut self) {
        self.view_stack = vec![View::Favorites];
        self.selection_stack.clear();
        let rows = self.populate_favorites();
        self.repopulate(self.current, rows);
    }

    fn push_view(&mut self, cx: &mut Cx<App>, view: View) {
        if self.is_animating {
            return;
        }
        let selected = self.lists[self.current].selected.unwrap_or(0);
        self.selection_stack.push(selected);
        self.view_stack.push(view.clone());
        self.animate_transition(cx, Direction::Forward, &view);
    }

    fn go_back(&mut self, cx: &mut Cx<App>) {
        if self.is_animating {
            return;
        }
        if self.view_stack.len() <= 1 {
            return;
        }
        self.view_stack.pop();
        let Some(prev) = self.view_stack.last().cloned() else { return };
        let selected = self.selection_stack.pop().unwrap_or(0);
        self.animate_transition(cx, Direction::Back, &prev);
        if selected > 0 {
            let list_h = self.layout_cx(cx).list_h;
            self.select_index(self.current, selected, list_h);
        }
    }

    /// Rebuild the view currently on top of the stack, in place.
    fn restore_current_view(&mut self) {
        let current = match self.view_stack.last().cloned() {
            Some(v) => v,
            None => {
                self.show_favorites_view();
                return;
            }
        };
        let rows = self.render_view(&current);
        self.repopulate(self.current, rows);
    }

    fn populate_favorites(&mut self) -> Vec<Row> {
        let rows = self
            .favorites
            .iter()
            .filter_map(|id| self.by_id.get(id).copied())
            .map(|idx| Row::App { idx, favorite: true, draggable: true })
            .collect();
        self.nav = NavButton { forward: true, label: "All Applications", icon: "folder", action: NavAction::ShowCategories };
        rows
    }

    fn populate_categories(&mut self) -> Vec<Row> {
        let mut rows = vec![Row::Category { name: "All Applications".into(), icon: "applications-other" }];
        for (cat, apps) in &self.categories {
            if !apps.is_empty() {
                rows.push(Row::Category { name: cat.clone(), icon: category_icon(cat) });
            }
        }
        self.nav = NavButton { forward: false, label: "Back", icon: "go-previous", action: NavAction::GoBack };
        rows
    }

    fn apps_for_category(&self, name: &str) -> Vec<usize> {
        if name == "All Applications" {
            (0..self.all_apps.len()).collect()
        } else {
            self.categories.get(name).cloned().unwrap_or_default()
        }
    }

    fn favorite_ids(&self) -> HashSet<String> {
        self.favorites.iter().cloned().collect()
    }

    fn populate_category(&mut self, name: &str) -> Vec<Row> {
        let mut idxs = self.apps_for_category(name);
        idxs.sort_by(|&a, &b| self.all_apps[a].name_lower.cmp(&self.all_apps[b].name_lower));
        let favs = self.favorite_ids();
        let rows = idxs
            .into_iter()
            .map(|idx| Row::App { idx, favorite: favs.contains(&self.all_apps[idx].desktop_id), draggable: false })
            .collect();
        self.nav = NavButton { forward: false, label: "Back", icon: "go-previous", action: NavAction::GoBack };
        rows
    }

    fn show_search_results(&mut self, query: &str) {
        let favs = self.favorite_ids();
        let rows = self
            .search_apps(query)
            .into_iter()
            .take(20)
            .map(|idx| Row::App { idx, favorite: favs.contains(&self.all_apps[idx].desktop_id), draggable: false })
            .collect();
        self.repopulate(self.current, rows);
    }

    fn animate_transition(&mut self, cx: &mut Cx<App>, direction: Direction, view: &View) {
        if self.is_animating {
            return;
        }
        self.is_animating = true;

        let next = 1 - self.current;
        let rows = self.render_view(view);
        self.repopulate(next, rows);
        self.slide = Some(Slide { from: self.current, direction, start: Instant::now() });
        self.current = next;
        self.redraw(cx);

        cx.timeout(TRANSITION_LOCK, |app, _| app.is_animating = false);
    }

    fn nav_action(&mut self, cx: &mut Cx<App>, action: NavAction) {
        match action {
            NavAction::ShowCategories => self.push_view(cx, View::Categories),
            NavAction::GoBack => self.go_back(cx),
        }
    }

    // --- event handlers ---

    fn on_row_activated(&mut self, cx: &mut Cx<App>, row: usize) {
        if self.activate_blocked || self.dragging {
            return;
        }
        match self.lists[self.current].rows.get(row).cloned() {
            Some(Row::Category { name, .. }) => self.push_view(cx, View::Category(name)),
            Some(Row::App { idx, .. }) => self.launch_app(cx, idx),
            None => {}
        }
    }

    /// The row of the current list under a point in window coordinates.
    fn row_at(&self, x: f32, y: f32, l: &Layout) -> Option<usize> {
        if x < 0.0 || x >= l.w || y < 0.0 || y >= l.list_h {
            return None;
        }
        let list = &self.lists[self.current];
        let i = ((y + list.scroll.offset) / ROW_H).floor();
        (i >= 0.0 && (i as usize) < list.rows.len()).then_some(i as usize)
    }

    fn star_rect(&self, row: usize, l: &Layout) -> (f32, f32, f32, f32) {
        let y = row as f32 * ROW_H - self.lists[self.current].scroll.offset;
        let (bw, bh) = STAR_BUTTON;
        (l.w - MARGIN - bw, y + ((ROW_H - bh) / 2.0).round(), bw, bh)
    }

    fn on_star(&self, row: usize, x: f32, y: f32, l: &Layout) -> bool {
        if !matches!(self.lists[self.current].rows.get(row), Some(Row::App { .. })) {
            return false;
        }
        let (sx, sy, sw, sh) = self.star_rect(row, l);
        x >= sx && x < sx + sw && y >= sy && y < sy + sh
    }

    fn in_nav(&self, x: f32, y: f32, l: &Layout) -> bool {
        x >= 0.0 && x < l.w && y >= l.nav_y && y < l.nav_y + NAV_H
    }

    fn on_pointer_motion(&mut self, cx: &mut Cx<App>, x: f64, y: f64) {
        let l = self.layout_cx(cx);
        let (fx, fy) = (x as f32, y as f32);
        self.pointer = Some((fx, fy));
        if self.entry.pointer_motion(fx) {
            self.redraw(cx);
        }
        let cursor = if self.in_nav(fx, fy, &l) {
            CursorIcon::Pointer
        } else if self.entry.contains(fx, fy) {
            CursorIcon::Text
        } else {
            CursorIcon::Default
        };
        cx.set_cursor(cursor);

        let moved = self.note_pointer((x, y));
        if self.dragging {
            self.drag_motion_to(fy, &l);
        } else if moved {
            if let Some(row) = self.row_at(fx, fy, &l) {
                self.hover_select(row);
            }
        }
        self.redraw(cx);
    }

    fn on_pointer_enter(&mut self, cx: &mut Cx<App>, x: f64, y: f64) {
        let (fx, fy) = (x as f32, y as f32);
        self.pointer = Some((fx, fy));
        if self.dragging || self.keyboard_mode {
            self.redraw(cx);
            return;
        }
        if !self.note_pointer((x, y)) {
            return;
        }
        let l = self.layout_cx(cx);
        if let Some(row) = self.row_at(fx, fy, &l) {
            self.hover_select(row);
        }
        self.redraw(cx);
    }

    fn drag_motion_to(&mut self, y: f32, l: &Layout) {
        if !self.dragging {
            return;
        }
        let Some(drag) = self.drag_row else { return };
        if let Some(target) = self.row_at(0.0, y, l) {
            if target != drag {
                let rows = &mut self.lists[self.current].rows;
                let row = rows.remove(drag);
                rows.insert(target, row);
                self.lists[self.current].selected = Some(target);
                self.drag_row = Some(target);
                self.drag_moved = true;
            }
        }
    }

    fn on_button(&mut self, cx: &mut Cx<App>, x: f64, y: f64, button: u32, pressed: bool) {
        let l = self.layout_cx(cx);
        let (fx, fy) = (x as f32, y as f32);
        if pressed {
            self.on_button_press(cx, fx, fy, button, &l);
        } else {
            self.on_button_release(cx, fx, fy, button, &l);
        }
        self.redraw(cx);
    }

    fn on_button_press(&mut self, cx: &mut Cx<App>, x: f32, y: f32, button: u32, l: &Layout) {
        if self.entry.contains(x, y) {
            if button == BTN_LEFT {
                self.entry.focus();
                self.entry.pointer_press(x, cx.modifiers().shift);
            }
            return;
        }
        if self.in_nav(x, y, l) {
            self.nav_action(cx, self.nav.action);
            return;
        }
        let Some(row) = self.row_at(x, y, l) else { return };
        let row_kind = self.lists[self.current].rows[row].clone();
        match row_kind {
            Row::App { idx, draggable, .. } => {
                if button == BTN_LEFT && self.on_star(row, x, y, l) {
                    self.pressed_star = Some(row);
                    return;
                }
                if button == BTN_RIGHT {
                    self.show_context_menu(cx, idx, x, y);
                    return;
                }
                if button == BTN_LEFT {
                    self.keyboard_mode = false;
                    self.hover_select(row);
                    if draggable {
                        self.dragging = true;
                        self.drag_moved = false;
                        self.drag_row = Some(row);
                    }
                    self.pressed_row = Some(row);
                }
            }
            Row::Category { .. } => {
                if button == BTN_LEFT {
                    self.pressed_row = Some(row);
                }
            }
        }
    }

    fn on_button_release(&mut self, cx: &mut Cx<App>, x: f32, y: f32, button: u32, l: &Layout) {
        self.entry.pointer_release();
        if button != BTN_LEFT {
            return;
        }
        if let Some(star_row) = self.pressed_star.take() {
            if self.row_at(x, y, l) == Some(star_row) && self.on_star(star_row, x, y, l) {
                if let Some(Row::App { idx, .. }) = self.lists[self.current].rows.get(star_row).cloned() {
                    self.on_favorite_clicked(cx, idx);
                }
            }
            return;
        }
        let pressed = self.pressed_row.take();
        if self.dragging {
            self.dragging = false;
            let reordered = self.drag_moved;
            self.drag_moved = false;
            self.drag_row = None;
            if reordered {
                let order = self.lists[self.current]
                    .rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::App { idx, .. } => Some(self.all_apps[*idx].desktop_id.clone()),
                        _ => None,
                    })
                    .collect();
                self.favorites = order;
                self.save_favorites();
                // The reorder does not also launch the app.
                return;
            }
        }
        // A plain click: activate the row it started on, if still over it.
        if let Some(row) = pressed {
            if self.row_at(x, y, l) == Some(row) {
                self.lists[self.current].selected = Some(row);
                self.on_row_activated(cx, row);
            }
        }
    }

    fn on_scroll(&mut self, cx: &mut Cx<App>, ev: &ScrollEvent) {
        let l = self.layout_cx(cx);
        if (ev.y as f32) >= l.list_h {
            return;
        }
        let max = self.max_scroll(self.current, l.list_h);
        if self.lists[self.current].scroll.on_scroll(ev, l.list_h, max, Instant::now()) {
            self.redraw(cx);
        }
    }

    fn show_context_menu(&mut self, cx: &mut Cx<App>, idx: usize, x: f32, y: f32) {
        let Some(parent) = self.win else { return };
        let (name, desktop_path) = (self.all_apps[idx].name.clone(), self.all_apps[idx].desktop_path.clone());
        let items = vec![
            Some((format!("Launch {name}"), MenuAction::Launch(idx))),
            None,
            Some(("Open .desktop file location".to_string(), MenuAction::OpenLocation(desktop_path))),
        ];
        let size = menu_size(cx, &items);
        self.close_menu(cx);
        // The menu takes keyboard focus while it is up, so the launcher gets
        // a focus-out; suppress the auto-hide until it closes.
        self.menu_open = true;
        let opts = PopupOptions { parent, anchor: (x as i32, y as i32, 1, 1), size, grab: true, transparent: true };
        match cx.open_popup(opts) {
            Some(win) => self.menu = Some(Menu { win, items, hover: None }),
            None => self.menu_open = false,
        }
    }

    fn close_menu(&mut self, cx: &mut Cx<App>) {
        if let Some(menu) = self.menu.take() {
            cx.close(menu.win);
            self.on_menu_hidden(cx);
        }
    }

    fn on_menu_hidden(&mut self, cx: &mut Cx<App>) {
        self.menu_open = false;
        self.sync_entry_focus();
        // Dismissed by clicking another window: the launcher never gets focus
        // back, so start the same countdown a focus-out would.
        if let Some(w) = self.win {
            if !cx.has_keyboard_focus(w) && self.focus_out_timeout.is_none() {
                self.start_focus_out_timeout(cx);
            }
        }
        self.redraw(cx);
    }

    fn activate_menu_item(&mut self, cx: &mut Cx<App>, item: usize) {
        let Some(menu) = self.menu.take() else { return };
        let action = match menu.items.into_iter().nth(item).flatten() {
            Some((_, a)) => a,
            None => {
                self.menu = None;
                return;
            }
        };
        cx.close(menu.win);
        self.on_menu_hidden(cx);
        match action {
            MenuAction::Launch(idx) => self.launch_app(cx, idx),
            MenuAction::OpenLocation(path) => open_file_location(&path),
        }
    }

    fn on_menu_event(&mut self, cx: &mut Cx<App>, event: Event) {
        let Some(menu) = &mut self.menu else { return };
        let win = menu.win;
        match event {
            Event::PointerMotion { y, .. } | Event::PointerEnter { y, .. } => {
                let hover = menu_item_at(&menu.items, y as f32);
                if hover != menu.hover {
                    menu.hover = hover;
                    cx.redraw(win);
                }
            }
            Event::PointerLeave => {
                menu.hover = None;
                cx.redraw(win);
            }
            Event::PointerButton { y, pressed: false, .. } => {
                if let Some(i) = menu_item_at(&menu.items, y as f32) {
                    self.activate_menu_item(cx, i);
                }
            }
            Event::Key(KeyEvent { keysym, pressed: true, .. }) => match keysym {
                Keysym::Escape => self.close_menu(cx),
                Keysym::Up | Keysym::Down => {
                    let selectable: Vec<usize> = (0..menu.items.len()).filter(|i| menu.items[*i].is_some()).collect();
                    if selectable.is_empty() {
                        return;
                    }
                    let pos = menu.hover.and_then(|h| selectable.iter().position(|i| *i == h));
                    let next = match (pos, keysym == Keysym::Down) {
                        (None, true) => 0,
                        (None, false) => selectable.len() - 1,
                        (Some(p), true) => (p + 1).min(selectable.len() - 1),
                        (Some(p), false) => p.saturating_sub(1),
                    };
                    menu.hover = Some(selectable[next]);
                    cx.redraw(win);
                }
                Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                    if let Some(i) = menu.hover {
                        self.activate_menu_item(cx, i);
                    }
                }
                _ => {}
            },
            Event::CloseRequested => {
                // Dismissed by the compositor; the popup is already gone.
                self.menu = None;
                self.on_menu_hidden(cx);
            }
            _ => {}
        }
    }

    fn on_favorite_clicked(&mut self, cx: &mut Cx<App>, idx: usize) {
        let desktop_id = self.all_apps[idx].desktop_id.clone();

        if let Some(pos) = self.favorites.iter().position(|d| *d == desktop_id) {
            self.favorites.remove(pos);
        } else {
            self.favorites.push(desktop_id);
        }
        self.save_favorites();

        if !matches!(self.view_stack.last(), Some(View::Favorites) | Some(View::Category(_))) {
            return;
        }

        // Keep the user where they were instead of snapping back to the top.
        let previous = self.lists[self.current].selected;
        self.restore_current_view();
        if let Some(index) = previous {
            let list_h = self.layout_cx(cx).list_h;
            self.select_index(self.current, index, list_h);
        }
    }

    fn on_search_changed(&mut self) {
        let text = self.entry.text().to_string();
        let query = text.trim();
        if query.is_empty() {
            self.restore_current_view();
        } else {
            self.show_search_results(query);
        }
    }

    fn activate_selection(&mut self, cx: &mut Cx<App>) {
        if self.activate_blocked {
            return;
        }

        let list = &self.lists[self.current];
        if let Some(row) = list.selected.and_then(|i| list.rows.get(i)).cloned() {
            match row {
                Row::App { idx, .. } => {
                    self.launch_app(cx, idx);
                    return;
                }
                Row::Category { name, .. } => {
                    self.push_view(cx, View::Category(name));
                    return;
                }
            }
        }

        let text = self.entry.text().to_string();
        let query = text.trim();
        if query.is_empty() {
            return;
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let argv = [shell, "-c".to_string(), query.to_string()];
        if spawn_detached(&argv, None, &[], true).is_ok() {
            self.hide_launcher(cx);
        }
    }

    fn search_apps(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();

        let mut exact = Vec::new();
        let mut prefix = Vec::new();
        let mut infix = Vec::new();
        let mut other = Vec::new();

        for (i, app) in self.all_apps.iter().enumerate() {
            if app.name_lower == query {
                exact.push(i);
            } else if app.name_lower.starts_with(&query) {
                prefix.push(i);
            } else if app.name_lower.contains(&query) {
                infix.push(i);
            } else if app.search_text.contains(&query) {
                other.push(i);
            }
        }

        exact.extend(prefix);
        exact.extend(infix);
        exact.extend(other);
        exact
    }

    fn launch_app(&mut self, cx: &mut Cx<App>, idx: usize) {
        let entry = &self.all_apps[idx];
        if launch_entry(entry).is_ok() {
            self.hide_launcher(cx);
            return;
        }

        let id = entry.desktop_id.clone();
        if spawn_detached(&["gtk-launch".into(), id.clone()], None, &[], true).is_ok() {
            self.hide_launcher(cx);
            return;
        }
        if spawn_detached(&["dbus-launch".into(), "gtk-launch".into(), id], None, &[], true).is_ok() {
            self.hide_launcher(cx);
            return;
        }

        eprintln!("Failed to launch: {}", self.all_apps[idx].name);
    }

    fn save_favorites(&self) {
        let path = favorites_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.favorites) {
            let _ = fs::write(&path, json);
        }
    }

    fn rebuild_by_id(&mut self) {
        let mut map = HashMap::new();
        for (i, a) in self.all_apps.iter().enumerate() {
            map.entry(a.desktop_id.clone()).or_insert(i);
        }
        self.by_id = map;
    }

    // --- file monitoring / reload ---

    pub fn on_desktop_dir_changed(&mut self, cx: &mut Cx<App>) {
        if self.reload_pending {
            return;
        }
        self.reload_pending = true;
        cx.timeout(Duration::from_millis(500), |app, cx| app.reload_applications(cx));
    }

    fn reload_applications(&mut self, cx: &mut Cx<App>) {
        self.reload_pending = false;
        self.all_apps = desktop::load_applications();
        self.rebuild_by_id();
        self.categories = desktop::organize_by_category(&self.all_apps);
        if self.visible {
            // Category apps are always re-derived from the category name at
            // populate time, so no stored snapshot needs refreshing here.
            self.restore_current_view();
            self.redraw(cx);
        }
    }

    // --- focus / keyboard ---

    fn start_focus_out_timeout(&mut self, cx: &mut Cx<App>) {
        self.focus_out_timeout = cx.timeout(Duration::from_millis(500), |app, cx| {
            app.focus_out_timeout = None;
            app.hide_launcher(cx);
        });
    }

    fn on_focus_out(&mut self, cx: &mut Cx<App>) {
        if let Some(id) = self.focus_out_timeout.take() {
            cx.cancel(id);
        }
        if self.menu_open {
            return;
        }
        self.start_focus_out_timeout(cx);
    }

    fn on_focus_in(&mut self, cx: &mut Cx<App>) {
        if let Some(id) = self.focus_out_timeout.take() {
            cx.cancel(id);
        }
        self.sync_entry_focus();
        self.redraw(cx);
    }

    /// Mid-search the caret belongs in the entry; with an empty query the
    /// placeholder should be showing instead.
    fn sync_entry_focus(&mut self) {
        if self.entry.text().is_empty() {
            self.blur_entry();
        } else {
            self.focus_entry();
        }
    }

    /// Escape and Enter act on release, so the window is never unmapped with
    /// the key still down (which would replay it into the next show).
    fn on_key_release(&mut self, cx: &mut Cx<App>, ev: &KeyEvent) {
        if self.armed_key != Some(ev.keysym) {
            return;
        }
        self.armed_key = None;

        if ev.keysym == Keysym::Escape {
            self.hide_launcher(cx);
        } else {
            self.activate_selection(cx);
        }
    }

    fn on_key_press(&mut self, cx: &mut Cx<App>, ev: &KeyEvent) {
        let kv = ev.keysym;
        let list_h = self.layout_cx(cx).list_h;

        self.keyboard_mode = true;

        if kv == Keysym::Escape {
            if self.view_stack.len() > 1 {
                self.go_back(cx);
            } else {
                self.armed_key = Some(kv);
            }
            return;
        }

        match kv {
            Keysym::Up => return self.move_selection(-1, list_h),
            Keysym::Down => return self.move_selection(1, list_h),
            Keysym::Page_Up | Keysym::KP_Page_Up => return self.move_selection(-10, list_h),
            Keysym::Page_Down | Keysym::KP_Page_Down => return self.move_selection(10, list_h),
            Keysym::Return | Keysym::KP_Enter => {
                self.armed_key = Some(kv);
                return;
            }
            _ => {}
        }

        // With focus pinned to the entry, an empty query is what distinguishes
        // "browsing the list" from "editing a search".
        let browsing = self.entry.text().trim().is_empty();

        if browsing && (kv == Keysym::Home || kv == Keysym::End) {
            let last = self.lists[self.current].rows.len().saturating_sub(1);
            self.select_index(self.current, if kv == Keysym::Home { 0 } else { last }, list_h);
            self.blur_entry_if_empty();
            return;
        }

        if browsing && kv == Keysym::Right {
            let selected = self.lists[self.current].selected.and_then(|i| self.lists[self.current].rows.get(i)).cloned();
            if let Some(Row::Category { name, .. }) = selected {
                self.push_view(cx, View::Category(name));
            } else if matches!(self.view_stack.last(), Some(View::Favorites)) {
                self.push_view(cx, View::Categories);
            }
            return;
        }

        if browsing && kv == Keysym::Left && self.view_stack.len() > 1 {
            self.go_back(cx);
            return;
        }

        // Anything else is text, and the entry should own it.
        self.focus_entry();
        if let Some(win) = self.win {
            if self.entry.handle_key(ev, cx, win) == KeyResponse::Changed {
                self.on_search_changed();
            }
        }
    }

    // --- drawing ---

    fn draw_list(&mut self, dc: &mut DrawCx, list: usize, x: f32, l: &Layout, label: &TextStyle) {
        let theme = dc.theme;
        let hovered = self.pointer.filter(|_| list == self.current).and_then(|(px, py)| self.row_at(px, py, l));
        let scroll = self.lists[list].scroll.offset;
        let first = (scroll / ROW_H).floor().max(0.0) as usize;
        let last = (((scroll + l.list_h) / ROW_H).ceil() as usize).min(self.lists[list].rows.len());
        let w = l.w;
        for i in first..last {
            let y = i as f32 * ROW_H - scroll;
            let selected = self.lists[list].selected == Some(i);
            if selected {
                let alpha = if hovered == Some(i) { 0.7 } else { 0.6 };
                dc.fill_rect(x, y, w, ROW_H, theme.selected_bg.fade(alpha));
            }
            let symbolic = if selected { theme.selected_fg } else { theme.fg };
            match &self.lists[list].rows[i] {
                Row::App { idx, favorite, .. } => {
                    let app = &self.all_apps[*idx];
                    let icon_y = y + ((ROW_H - APP_ICON) / 2.0).round();
                    let drawn = app.icon.as_ref().is_some_and(|icon| dc.icon(icon, x + MARGIN, icon_y, APP_ICON, Some(symbolic)));
                    if !drawn {
                        dc.icon(&Icon::name("application-x-executable"), x + MARGIN, icon_y, APP_ICON, Some(symbolic));
                    }
                    let (bw, bh) = STAR_BUTTON;
                    let bx = x + w - MARGIN - bw;
                    let by = y + ((ROW_H - bh) / 2.0).round();
                    let label_x = x + MARGIN + APP_ICON + SPACING;
                    let label_w = (bx - SPACING - label_x).max(0.0);
                    dc.text_v_centered(label_x, y, ROW_H, &app.name, label, Some(label_w));
                    if list == self.current && hovered == Some(i) {
                        if let Some((px, py)) = self.pointer {
                            if px >= bx && px < bx + bw && py >= by && py < by + bh {
                                dc.fill_rounded_rect(bx, by, bw, bh, 4.0, theme.fg.fade(0.12));
                            }
                        }
                    }
                    let star = if *favorite { "starred" } else { "non-starred" };
                    dc.icon(&Icon::name(star), bx + ((bw - GLYPH_ICON) / 2.0).round(), by + ((bh - GLYPH_ICON) / 2.0).round(), GLYPH_ICON, Some(symbolic));
                }
                Row::Category { name, icon } => {
                    dc.icon(&Icon::name(*icon), x + MARGIN, y + ((ROW_H - CATEGORY_ICON) / 2.0).round(), CATEGORY_ICON, Some(symbolic));
                    let label_x = x + MARGIN + CATEGORY_ICON + SPACING;
                    let arrow_x = x + w - MARGIN - SMALL_ICON;
                    dc.text_v_centered(label_x, y, ROW_H, name, label, Some((arrow_x - SPACING - label_x).max(0.0)));
                    dc.icon(&Icon::name("pan-end-symbolic"), arrow_x + ((SMALL_ICON - GLYPH_ICON) / 2.0).round(), y + ((ROW_H - GLYPH_ICON) / 2.0).round(), GLYPH_ICON, Some(symbolic));
                }
            }
        }
    }

    fn draw_window(&mut self, dc: &mut DrawCx) {
        self.size = (dc.width, dc.height);
        let theme = dc.theme;
        let l = self.layout(theme.entry.height);
        let now = dc.now;
        dc.fill_rect(0.0, 0.0, dc.width, dc.height, theme.bg.fade(0.8));

        let label = theme.text_style().color(Color::WHITE).shadows(&LABEL_SHADOWS);

        // Kinetic scrolling.
        for list in 0..2 {
            let max = self.max_scroll(list, l.list_h);
            if self.lists[list].scroll.tick(now, max) {
                dc.request_frame();
            }
            self.lists[list].scroll.clamp(max);
        }

        dc.push_clip(0.0, 0.0, l.w, l.list_h);
        match &self.slide {
            Some(s) if now.duration_since(s.start) < TRANSITION => {
                let t = ease_out_cubic(now.duration_since(s.start).as_secs_f32() / TRANSITION.as_secs_f32());
                let (from, to) = match s.direction {
                    Direction::Forward => (-l.w * t, l.w * (1.0 - t)),
                    Direction::Back => (l.w * t, -l.w * (1.0 - t)),
                };
                let old = s.from;
                self.draw_list(dc, old, from.round(), &l, &label);
                self.draw_list(dc, self.current, to.round(), &l, &label);
                dc.request_frame();
            }
            _ => {
                self.slide = None;
                self.draw_list(dc, self.current, 0.0, &l, &label);
            }
        }
        dc.pop_clip();

        dc.fill_rect(0.0, l.sep1_y + 2.0, l.w, 1.0, SEPARATOR);
        dc.fill_rect(0.0, l.sep2_y + 2.0, l.w, 1.0, SEPARATOR);

        // Navigation button: 30px tall below a 5px top margin.
        let ny = l.nav_y + MARGIN;
        let nh = NAV_H - MARGIN;
        dc.icon(&Icon::name(self.nav.icon), MARGIN, ny + ((nh - CATEGORY_ICON) / 2.0).round(), CATEGORY_ICON, Some(theme.fg));
        let label_x = MARGIN + CATEGORY_ICON + SPACING;
        let arrow_x = l.w - MARGIN - SMALL_ICON;
        let max_w = if self.nav.forward { arrow_x - SPACING - label_x } else { l.w - MARGIN - label_x };
        dc.text_v_centered(label_x, ny, nh, self.nav.label, &label, Some(max_w.max(0.0)));
        if self.nav.forward {
            dc.icon(&Icon::name("pan-end-symbolic"), arrow_x + ((SMALL_ICON - GLYPH_ICON) / 2.0).round(), ny + ((nh - GLYPH_ICON) / 2.0).round(), GLYPH_ICON, Some(theme.fg));
        }

        let (ex, ey, ew, eh) = l.entry;
        let font = theme.text_style();
        self.entry.draw(dc, ex, ey, ew, eh, &font, &theme.entry);
    }

    fn draw_menu(&mut self, dc: &mut DrawCx) {
        let Some(menu) = &self.menu else { return };
        let theme = dc.theme;
        let m = &theme.menu;
        dc.fill_bordered(0.0, 0.0, dc.width, dc.height, m.radius, 1.0, m.bg, m.border);
        let font = theme.text_style().color(m.fg);
        let mut y = MENU_PAD;
        for (i, item) in menu.items.iter().enumerate() {
            match item {
                Some((label, _)) => {
                    if menu.hover == Some(i) {
                        dc.fill_rect(1.0, y, dc.width - 2.0, MENU_ITEM_H, m.hover_bg);
                    }
                    let color = if menu.hover == Some(i) { m.hover_fg } else { m.fg };
                    dc.text_v_centered(MENU_ITEM_PAD_X + 1.0, y, MENU_ITEM_H, label, &font.color(color), None);
                    y += MENU_ITEM_H;
                }
                None => {
                    dc.fill_rect(1.0, y + (MENU_SEPARATOR_H / 2.0).floor(), dc.width - 2.0, 1.0, m.separator);
                    y += MENU_SEPARATOR_H;
                }
            }
        }
    }
}

const MENU_PAD: f32 = 3.0;
const MENU_ITEM_H: f32 = 25.0;
const MENU_ITEM_PAD_X: f32 = 6.0;
const MENU_SEPARATOR_H: f32 = 7.0;

fn menu_size(cx: &mut Cx<App>, items: &[Option<(String, MenuAction)>]) -> (u32, u32) {
    let font_family = cx.theme().font.family.clone();
    let (size, weight, color) = (cx.theme().font.size, cx.theme().font.weight, cx.theme().menu.fg);
    let style = TextStyle::new(&font_family, size, weight, color);
    let mut w: f32 = 40.0;
    let mut h = 2.0 * MENU_PAD;
    for item in items {
        match item {
            Some((label, _)) => {
                let (tw, _) = cx.text().measure(label, &style, 1.0);
                w = w.max(tw.ceil() + 2.0 * MENU_ITEM_PAD_X + 2.0);
                h += MENU_ITEM_H;
            }
            None => h += MENU_SEPARATOR_H,
        }
    }
    (w.ceil() as u32, h.ceil() as u32)
}

fn menu_item_at(items: &[Option<(String, MenuAction)>], y: f32) -> Option<usize> {
    let mut top = MENU_PAD;
    for (i, item) in items.iter().enumerate() {
        let h = if item.is_some() { MENU_ITEM_H } else { MENU_SEPARATOR_H };
        if y >= top && y < top + h {
            return item.as_ref().map(|_| i);
        }
        top += h;
    }
    None
}

impl gui_toolkit::App for App {
    fn event(&mut self, cx: &mut Cx<Self>, win: WindowId, event: Event) {
        if self.menu.as_ref().is_some_and(|m| m.win == win) {
            return self.on_menu_event(cx, event);
        }
        if Some(win) != self.win {
            return;
        }
        match event {
            Event::Resized { width, height } => {
                self.size = (width, height);
                let l = self.layout_cx(cx);
                for list in 0..2 {
                    let max = self.max_scroll(list, l.list_h);
                    self.lists[list].scroll.clamp(max);
                }
            }
            Event::CloseRequested => self.hide_launcher(cx),
            Event::FocusIn => self.on_focus_in(cx),
            Event::FocusOut => self.on_focus_out(cx),
            Event::PointerEnter { x, y } => self.on_pointer_enter(cx, x, y),
            Event::PointerLeave => {
                self.pointer = None;
                cx.set_cursor(CursorIcon::Default);
            }
            Event::PointerMotion { x, y } => self.on_pointer_motion(cx, x, y),
            Event::PointerButton { x, y, button, pressed } => self.on_button(cx, x, y, button, pressed),
            Event::Scroll(ev) => self.on_scroll(cx, &ev),
            Event::Key(ev) => {
                if ev.pressed {
                    self.on_key_press(cx, &ev);
                } else {
                    self.on_key_release(cx, &ev);
                }
            }
            Event::Paste(text) => {
                self.focus_entry();
                if self.entry.insert(&text) {
                    self.on_search_changed();
                }
            }
        }
        self.redraw(cx);
    }

    fn draw(&mut self, dc: &mut DrawCx, win: WindowId) {
        if self.menu.as_ref().is_some_and(|m| m.win == win) {
            self.draw_menu(dc);
        } else {
            self.draw_window(dc);
        }
    }
}

fn open_file_location(desktop_path: &str) {
    if let Some(dir) = std::path::Path::new(desktop_path).parent() {
        let _ = spawn_detached(&["xdg-open".into(), dir.to_string_lossy().into_owned()], None, &[], true);
    }
}

#[cfg(test)]
mod render {
    use super::*;

    #[test]
    #[ignore]
    fn render_views() {
        let out = PathBuf::from(std::env::var("RENDER_OUT").unwrap());
        let mut app = App::build(None);
        let save = |app: &mut App, name: &str| {
            let pm = gui_toolkit::render_offscreen(app, DEFAULT_SIZE.0, DEFAULT_SIZE.1, 1.0).unwrap();
            pm.save_png(out.join(name)).unwrap();
        };
        save(&mut app, "favorites.png");
        let rows = app.populate_categories();
        app.repopulate(app.current, rows);
        app.lists[app.current].selected = Some(2);
        save(&mut app, "categories.png");
        let rows = app.populate_category("Internet");
        app.repopulate(app.current, rows);
        save(&mut app, "category.png");
        app.entry.focus();
        app.entry.insert("fire");
        app.on_search_changed();
        save(&mut app, "search.png");
    }
}
