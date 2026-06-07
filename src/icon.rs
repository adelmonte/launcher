use std::cell::RefCell;
use std::collections::HashMap;

use gtk::gdk::prelude::GdkPixbufExt;
use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, gio};

use crate::desktop::AppEntry;

type Surface = gdk::cairo::Surface;

pub fn create_icon(
    app: &AppEntry,
    scale_factor: i32,
    cache: &RefCell<HashMap<String, Surface>>,
) -> gtk::Image {
    let image = gtk::Image::new();

    let icon = match &app.icon {
        Some(i) => i,
        None => {
            image.set_from_icon_name(Some("application-x-executable"), gtk::IconSize::Dnd);
            return image;
        }
    };

    let target_size = 32;
    let load_size = target_size * scale_factor;
    let key = icon_cache_key(icon, scale_factor);

    if let Some(k) = &key {
        if let Some(surface) = cache.borrow().get(k) {
            image.set_from_surface(Some(surface));
            return image;
        }
    }

    match load_surface(icon, scale_factor, load_size) {
        Some(surface) => {
            image.set_from_surface(Some(&surface));
            if let Some(k) = key {
                cache.borrow_mut().insert(k, surface);
            }
        }
        None => {
            image.set_from_icon_name(Some("application-x-executable"), gtk::IconSize::Dnd);
        }
    }

    image
}

fn icon_cache_key(icon: &gio::Icon, scale_factor: i32) -> Option<String> {
    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        let name = themed.names().into_iter().next()?;
        return Some(format!("{}@{}", name, scale_factor));
    }
    if let Some(file_icon) = icon.downcast_ref::<gio::FileIcon>() {
        let path = file_icon.file().path()?;
        return Some(format!("{}@{}", path.display(), scale_factor));
    }
    None
}

fn load_surface(icon: &gio::Icon, scale_factor: i32, load_size: i32) -> Option<Surface> {
    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        let names = themed.names();
        let name = names.first()?;
        let theme = gtk::IconTheme::default()?;
        let pixbuf = theme
            .load_icon(name.as_str(), load_size, gtk::IconLookupFlags::FORCE_SIZE)
            .ok()??;
        return pixbuf.create_surface(scale_factor, None::<&gdk::Window>);
    }
    if let Some(file_icon) = icon.downcast_ref::<gio::FileIcon>() {
        let path = file_icon.file().path()?;
        let pixbuf =
            gdk_pixbuf::Pixbuf::from_file_at_size(&path, load_size, load_size).ok()?;
        return pixbuf.create_surface(scale_factor, None::<&gdk::Window>);
    }
    None
}
