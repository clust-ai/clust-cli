use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Create a tray icon with a context menu.
/// Returns Ok((icon, quit_menu_id)) on success, or Err if the system tray is unavailable.
pub fn create_tray_icon() -> Result<(tray_icon::TrayIcon, MenuId), String> {
    let menu = Menu::new();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();
    menu.append(&quit_item)
        .map_err(|e| format!("failed to add quit menu item: {e}"))?;

    let icon = make_icon()?;
    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("clust pool")
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| format!("failed to create tray icon: {e}"))?;

    Ok((tray_icon, quit_id))
}

/// Generate a simple 22x22 solid RGBA icon programmatically.
fn make_icon() -> Result<Icon, String> {
    let size = 22u32;
    let rgba: Vec<u8> = vec![0xFF; (size * size * 4) as usize];
    Icon::from_rgba(rgba, size, size).map_err(|e| format!("failed to create icon: {e}"))
}
