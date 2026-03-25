use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Create a tray icon with a context menu for the macOS menu bar.
/// Must be called on the main thread, inside the tao event loop's Init handler.
/// Returns the tray icon and the Quit menu item's ID for event matching.
pub fn create_tray_icon() -> (tray_icon::TrayIcon, MenuId) {
    let menu = Menu::new();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();
    menu.append(&quit_item).expect("failed to add quit menu item");

    let icon = make_icon();
    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("clust pool")
        .with_menu(Box::new(menu))
        .build()
        .expect("failed to create tray icon");

    (tray_icon, quit_id)
}

/// Generate a simple 22x22 solid RGBA icon programmatically.
fn make_icon() -> Icon {
    let size = 22u32;
    let rgba: Vec<u8> = vec![0xFF; (size * size * 4) as usize];
    Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}
