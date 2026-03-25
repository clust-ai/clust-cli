use tray_icon::{Icon, TrayIconBuilder};

/// Create a tray icon for the macOS menu bar.
/// Must be called on the main thread, inside the tao event loop's Init handler.
pub fn create_tray_icon() -> tray_icon::TrayIcon {
    let icon = make_icon();
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("clust pool")
        .build()
        .expect("failed to create tray icon")
}

/// Generate a simple 22x22 solid RGBA icon programmatically.
fn make_icon() -> Icon {
    let size = 22u32;
    let rgba: Vec<u8> = vec![0xFF; (size * size * 4) as usize];
    Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}
