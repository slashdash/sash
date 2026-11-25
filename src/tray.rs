//! The menu-bar extra: a template icon plus Show / New Tab / Quit.

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Forward menu activations into app state. The handler runs on AppKit's
/// thread, and a hidden panel has no frame clock, so it must not wait for the
/// next poll: Rune's update applies the action on the UI thread and wakes the
/// loop even while the window is hidden.
pub fn forward_events() {
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        let id = event.id().0.clone();
        if let Some(handle) = crate::handle() {
            handle.update(move |sash, _tx| sash.handle_tray(&id));
        }
    }));
}

pub fn build() -> Option<TrayIcon> {
    let menu = Menu::new();
    // A disabled first row, so a tester's screenshot or report names the build.
    let about = MenuItem::with_id(
        "about",
        format!("Sash {}", crate::version_label()),
        false,
        None,
    );
    let about_separator = PredefinedMenuItem::separator();
    let toggle = MenuItem::with_id("toggle", "Show/Hide Sash", true, None);
    let new_tab = MenuItem::with_id("new-tab", "New Tab", true, None);
    let settings = MenuItem::with_id("settings", "Settings…", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("quit", "Quit Sash", true, None);
    if menu
        .append_items(&[
            &about,
            &about_separator,
            &toggle,
            &new_tab,
            &settings,
            &separator,
            &quit,
        ])
        .is_err()
    {
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

/// A ghost hanging from the drop-down ceiling: the menu-bar echo of the app
/// icon. Template art, so the menu bar supplies the colour.
fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let p = (px * 36.0 / s, py * 36.0 / s);

            // Ceiling bar the ghost hangs from.
            let mut d = rounded_rect_sdf(p, (4.0, 4.5), (32.0, 7.5), 1.5);

            // Two arms reaching up to the bar, clear of the head.
            let arm = 1.1;
            d = d
                .min(seg(p, (11.2, 13.5), (9.8, 7.5)) - arm)
                .min(seg(p, (24.8, 13.5), (26.2, 7.5)) - arm);

            // Head and body, with three rounded tails instead of a flat hem.
            let mut body = rounded_rect_sdf(p, (11.0, 11.0), (25.0, 26.0), 5.5);
            body = body
                .min(circle(p, (13.0, 26.0), 2.0))
                .min(circle(p, (18.0, 26.0), 2.0))
                .min(circle(p, (23.0, 26.0), 2.0));

            // Eyes are holes, so the silhouette still reads at menu-bar size.
            body = body.max(-circle(p, (15.2, 16.5), 1.4));
            body = body.max(-circle(p, (20.8, 16.5), 1.4));

            d = d.min(body);
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

fn circle(p: (f32, f32), c: (f32, f32), r: f32) -> f32 {
    ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - r
}

fn seg(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let ap = (p.0 - a.0, p.1 - a.1);
    let len2 = ab.0 * ab.0 + ab.1 * ab.1;
    let t = ((ap.0 * ab.0 + ap.1 * ab.1) / len2).clamp(0.0, 1.0);
    let q = (a.0 + ab.0 * t, a.1 + ab.1 * t);
    ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(icon: &[u8], x: usize, y: usize) -> u8 {
        icon[(y * 36 + x) * 4 + 3]
    }

    #[test]
    fn the_menu_bar_icon_is_a_ghost_hanging_from_a_bar() {
        let icon = icon_rgba(36);
        assert!(alpha(&icon, 18, 6) > 200, "ceiling bar");
        assert!(alpha(&icon, 18, 12) > 200, "head");
        assert_eq!(alpha(&icon, 15, 16), 0, "left eye");
        assert_eq!(alpha(&icon, 20, 16), 0, "right eye");
        assert!(alpha(&icon, 18, 24) > 200, "body");
        assert_eq!(alpha(&icon, 18, 34), 0, "below the hem");
    }
}
