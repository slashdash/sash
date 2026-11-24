//! The menu-bar extra: a template icon plus Show / New Tab / Quit.

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub fn build() -> Option<TrayIcon> {
    let menu = Menu::new();
    let toggle = MenuItem::with_id("toggle", "Show/Hide Sash", true, None);
    let new_tab = MenuItem::with_id("new-tab", "New Tab", true, None);
    let settings = MenuItem::with_id("settings", "Settings…", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("quit", "Quit Sash", true, None);
    if menu.append_items(&[&toggle, &new_tab, &settings, &separator, &quit]).is_err() {
        return None;
    }

    let icon = Icon::from_rgba(icon_rgba(36), 36, 36).ok()?;
    #[cfg(target_os = "macos")]
    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("sash")
        .with_icon_templated(icon);
    #[cfg(not(target_os = "macos"))]
    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("sash")
        .with_icon(icon);
    match builder.build() {
        Ok(tray) => Some(tray),
        Err(e) => {
            eprintln!("sash: could not create the menu-bar item: {e}");
            None
        }
    }
}

/// A terminal-window glyph: rounded outline, a prompt chevron and a cursor bar.
fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let stroke = 2.6 * s / 36.0;
    let half = stroke * 0.5;
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let p = (px * 36.0 / s, py * 36.0 / s);

            // Rounded rectangle border.
            let d_rect = rounded_rect_sdf(p, (4.0, 7.0), (32.0, 29.0), 5.0).abs() - half * 36.0 / s;
            let mut d = d_rect;

            // Chevron.
            let d1 = seg(p, (11.0, 14.5), (16.5, 20.0));
            let d2 = seg(p, (16.5, 20.0), (11.0, 25.5));
            let chev = d1.min(d2) - 1.1;
            d = d.min(chev);

            // Cursor bar.
            let bar = seg(p, (19.5, 25.5), (26.5, 25.5)) - 1.1;
            d = d.min(bar);

            if d < 1.0 {
                let alpha = (1.0 - d).clamp(0.0, 1.0);
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = (alpha * 255.0).round() as u8;
            }
        }
    }
    rgba
}

fn rounded_rect_sdf(p: (f32, f32), min: (f32, f32), max: (f32, f32), r: f32) -> f32 {
    let cx = (min.0 + max.0) * 0.5;
    let cy = (min.1 + max.1) * 0.5;
    let hx = (max.0 - min.0) * 0.5 - r;
    let hy = (max.1 - min.1) * 0.5 - r;
    let qx = (p.0 - cx).abs() - hx;
    let qy = (p.1 - cy).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn seg(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let ap = (p.0 - a.0, p.1 - a.1);
    let len2 = ab.0 * ab.0 + ab.1 * ab.1;
    let t = ((ap.0 * ab.0 + ap.1 * ab.1) / len2).clamp(0.0, 1.0);
    let q = (a.0 + ab.0 * t, a.1 + ab.1 * t);
    ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt()
}
