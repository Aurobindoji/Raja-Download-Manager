//! Colour themes: the original dark look, a Windows 11 light look and an
//! Aero Glass look.
//!
//! The UI was drawn with fixed dark-theme colours. Instead of a second set of
//! hand-picked colours for every widget, `rgb()` maps each dark-theme colour
//! to the active theme: greys are mirrored (dark backgrounds become light,
//! light text becomes dark), accents keep their hue. Colours that must look
//! the same in every theme (progress bars) use `fixed_rgb()`.

use std::sync::atomic::{AtomicU8, Ordering};

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Shape};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    Aero,
    AeroDark,
}

impl Theme {
    pub const ALL: [Theme; 4] = [Theme::Dark, Theme::Light, Theme::Aero, Theme::AeroDark];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Windows 11 Light",
            Theme::Aero => "Aero Glass",
            Theme::AeroDark => "Aero Glass Dark",
        }
    }

    /// Value stored in the settings table.
    pub fn key(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::Aero => "aero",
            Theme::AeroDark => "aero-dark",
        }
    }

    pub fn from_key(key: &str) -> Theme {
        match key.trim() {
            "light" => Theme::Light,
            "aero" => Theme::Aero,
            "aero-dark" => Theme::AeroDark,
            _ => Theme::Dark,
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Theme::Dark | Theme::AeroDark)
    }

    pub fn is_glass(self) -> bool {
        matches!(self, Theme::Aero | Theme::AeroDark)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_current(theme: Theme) {
    let value = match theme {
        Theme::Dark => 0,
        Theme::Light => 1,
        Theme::Aero => 2,
        Theme::AeroDark => 3,
    };
    CURRENT.store(value, Ordering::Relaxed);
}

pub fn current() -> Theme {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Theme::Light,
        2 => Theme::Aero,
        3 => Theme::AeroDark,
        _ => Theme::Dark,
    }
}

/// A colour from the dark design, shown in the active theme.
pub fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    let (r, g, b) = map_for(current(), r, g, b);
    Color32::from_rgb(r, g, b)
}

/// A colour that stays the same in every theme.
pub fn fixed_rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn luminance(r: f32, g: f32, b: f32) -> f32 {
    0.299 * r + 0.587 * g + 0.114 * b
}

fn mix(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// Dark-theme grey level -> Windows 11 light grey level.
fn mirror_grey(level: f32) -> f32 {
    // (dark level, light level): window/panel backgrounds become near-white,
    // borders mid-grey, light text near-black.
    const POINTS: [(f32, f32); 10] = [
        (0.0, 255.0),
        (25.0, 252.0),
        (35.0, 249.0),
        (45.0, 243.0),
        (60.0, 229.0),
        (85.0, 214.0),
        (135.0, 172.0),
        (185.0, 104.0),
        (235.0, 32.0),
        (255.0, 0.0),
    ];
    for pair in POINTS.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if level <= x1 {
            return y0 + (y1 - y0) * (level - x0) / (x1 - x0);
        }
    }
    0.0
}

fn clamp(value: (f32, f32, f32)) -> (u8, u8, u8) {
    let c = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (c(value.0), c(value.1), c(value.2))
}

pub fn map_for(theme: Theme, r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    if theme == Theme::Dark {
        return (r, g, b);
    }
    let (rf, gf, bf) = (f32::from(r), f32::from(g), f32::from(b));
    if theme == Theme::AeroDark {
        return clamp(smoked_glass(rf, gf, bf));
    }
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let level = luminance(rf, gf, bf);
    let neutral = max - min <= 30.0;
    let light = if neutral {
        // Mirror the grey, keep half of any slight tint.
        let target = mirror_grey(level);
        let keep = |c: f32| target + (c - level) * 0.5;
        (keep(rf), keep(gf), keep(bf))
    } else if level >= 160.0 {
        // Bright accents made for a dark background (error text, icons):
        // darker so they stay readable on white.
        (rf * 0.72, gf * 0.72, bf * 0.72)
    } else if level < 100.0 {
        // Dark coloured fills (selection, hover): light tints instead.
        mix((rf, gf, bf), (255.0, 255.0, 255.0), 0.74)
    } else {
        (rf, gf, bf)
    };
    if theme == Theme::Light || !neutral {
        return clamp(light);
    }
    // Aero Glass: sky-blue glass for surfaces, navy text.
    let level = luminance(light.0, light.1, light.2);
    let aero = if level >= 235.0 {
        mix(light, (222.0, 236.0, 251.0), 0.55)
    } else if level >= 150.0 {
        mix(light, (176.0, 206.0, 238.0), 0.40)
    } else {
        mix(light, (12.0, 36.0, 78.0), 0.30)
    };
    clamp(aero)
}

/// Window background, visible as thin divider lines between the menu bar,
/// toolbar and panels.
pub fn divider_colour(theme: Theme) -> Color32 {
    match theme {
        Theme::Dark => Color32::from_rgb(12, 12, 12),
        Theme::Light => Color32::from_rgb(218, 218, 220),
        Theme::Aero => Color32::from_rgb(150, 184, 222),
        Theme::AeroDark => Color32::from_rgb(6, 14, 28),
    }
}

/// Aero Glass Dark: the dark design seen through smoked blue glass. Greys
/// pick up a deep blue tint, light text turns a cool white; accents stay.
fn smoked_glass(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min > 30.0 {
        return (r, g, b);
    }
    let level = luminance(r, g, b);
    if level < 75.0 {
        mix((r, g, b), (14.0, 30.0, 58.0), 0.42)
    } else if level < 170.0 {
        mix((r, g, b), (70.0, 104.0, 150.0), 0.34)
    } else {
        mix((r, g, b), (214.0, 230.0, 252.0), 0.30)
    }
}

/// egui's base widget colours for the theme (the per-widget colours are then
/// set through `rgb()` as before).
pub fn base_visuals(theme: Theme) -> egui::Visuals {
    let mut visuals = if theme.is_dark() {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if theme == Theme::AeroDark {
        visuals.selection.bg_fill = Color32::from_rgb(36, 92, 156);
        visuals.selection.stroke.color = Color32::from_rgb(214, 232, 255);
        visuals.hyperlink_color = Color32::from_rgb(120, 190, 255);
    } else if theme == Theme::Aero {
        visuals.selection.bg_fill = Color32::from_rgb(153, 201, 239);
        visuals.selection.stroke.color = Color32::from_rgb(12, 52, 110);
        visuals.hyperlink_color = Color32::from_rgb(0, 84, 166);
    } else if theme == Theme::Light {
        visuals.selection.bg_fill = Color32::from_rgb(204, 228, 247);
        visuals.selection.stroke.color = Color32::from_rgb(0, 84, 153);
        visuals.hyperlink_color = Color32::from_rgb(0, 95, 184);
    }
    visuals
}

/// Background for the menu bar and toolbar: a plain fill, or in Aero Glass
/// the glossy two-band glass gradient with a white highlight line.
pub fn chrome_background(rect: Rect, dark_fill: Color32) -> Shape {
    let theme = current();
    if !theme.is_glass() {
        let [r, g, b, _] = dark_fill.to_array();
        return Shape::rect_filled(rect, 0.0, rgb(r, g, b));
    }
    // (upper band top, upper band bottom, lower band top, lower band bottom, highlight)
    let (upper_top, upper_bottom, lower_top, lower_bottom, highlight_colour) =
        if theme == Theme::AeroDark {
            (
                Color32::from_rgb(78, 104, 140),
                Color32::from_rgb(44, 62, 92),
                Color32::from_rgb(20, 32, 54),
                Color32::from_rgb(30, 46, 72),
                Color32::from_rgba_unmultiplied(255, 255, 255, 70),
            )
        } else {
            (
                Color32::from_rgb(244, 249, 255),
                Color32::from_rgb(219, 234, 251),
                Color32::from_rgb(196, 220, 247),
                Color32::from_rgb(214, 232, 252),
                Color32::WHITE,
            )
        };
    let mid = rect.top() + rect.height() * 0.48;
    let mut mesh = Mesh::default();
    let mut band = |top: f32, bottom: f32, top_colour: Color32, bottom_colour: Color32| {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(Pos2::new(rect.left(), top), top_colour);
        mesh.colored_vertex(Pos2::new(rect.right(), top), top_colour);
        mesh.colored_vertex(Pos2::new(rect.right(), bottom), bottom_colour);
        mesh.colored_vertex(Pos2::new(rect.left(), bottom), bottom_colour);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    };
    // Bright glossy upper half, deeper lower half: classic Aero glass.
    band(rect.top(), mid, upper_top, upper_bottom);
    band(mid, rect.bottom(), lower_top, lower_bottom);
    let highlight = Shape::line_segment(
        [
            Pos2::new(rect.left(), rect.top() + 0.5),
            Pos2::new(rect.right(), rect.top() + 0.5),
        ],
        egui::Stroke::new(1.0, highlight_colour),
    );
    Shape::Vec(vec![Shape::mesh(mesh), highlight])
}

/// Styles the native title bars of all of Raja's windows (main window and
/// download boxes): dark or light caption, and for Aero Glass the Windows 11
/// acrylic (blurred glass) backdrop. Windows 10 ignores what it does not know.
#[cfg(windows)]
pub fn apply_window_chrome(theme: Theme) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    // DWMSBT_MAINWINDOW (Mica) / DWMSBT_TRANSIENTWINDOW (Acrylic).
    const MICA: i32 = 2;
    const ACRYLIC: i32 = 3;

    unsafe extern "system" fn each_window(hwnd: HWND, data: LPARAM) -> i32 {
        let mut process = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut process);
            if process != std::process::id() || IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let theme = match data {
                1 => Theme::Light,
                2 => Theme::Aero,
                3 => Theme::AeroDark,
                _ => Theme::Dark,
            };
            let dark: i32 = i32::from(theme.is_dark());
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const i32).cast(),
                4,
            );
            let backdrop = if theme.is_glass() { ACRYLIC } else { MICA };
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                (&backdrop as *const i32).cast(),
                4,
            );
        }
        1
    }

    let data: LPARAM = match theme {
        Theme::Dark => 0,
        Theme::Light => 1,
        Theme::Aero => 2,
        Theme::AeroDark => 3,
    };
    unsafe {
        EnumWindows(Some(each_window), data);
    }
}

#[cfg(not(windows))]
pub fn apply_window_chrome(_theme: Theme) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_keeps_every_colour() {
        assert_eq!(map_for(Theme::Dark, 45, 45, 48), (45, 45, 48));
        assert_eq!(map_for(Theme::Dark, 255, 150, 140), (255, 150, 140));
    }

    #[test]
    fn light_theme_mirrors_greys_and_keeps_accents_readable() {
        let (r, _, _) = map_for(Theme::Light, 45, 45, 48); // panel background
        assert!(r > 235, "background should become light: {r}");
        let (r, _, _) = map_for(Theme::Light, 235, 235, 240); // text
        assert!(r < 50, "text should become dark: {r}");
        // Green "complete" accent keeps its hue.
        assert_eq!(map_for(Theme::Light, 72, 185, 105), (72, 185, 105));
        // Light red error text is darkened for a white background.
        let (r, g, _) = map_for(Theme::Light, 255, 150, 140);
        assert!(r < 200 && r > g);
        // Dark blue selection fill becomes a light tint.
        let (_, _, b) = map_for(Theme::Light, 40, 95, 130);
        assert!(b > 200);
    }

    #[test]
    fn aero_theme_tints_surfaces_blue_and_text_navy() {
        let (r, _, b) = map_for(Theme::Aero, 45, 45, 48);
        assert!(b > r + 8, "glass surfaces are sky-blue: {r} vs {b}");
        let (r, _, b) = map_for(Theme::Aero, 235, 235, 240);
        assert!(b > r && b < 120, "text is dark navy");
    }

    #[test]
    fn aero_dark_is_smoked_blue_glass() {
        let (r, _, b) = map_for(Theme::AeroDark, 45, 45, 48); // panel
        assert!(b > r + 12 && b < 90, "dark blue glass: {r} vs {b}");
        let (r, _, b) = map_for(Theme::AeroDark, 235, 235, 240); // text
        assert!(r > 200 && b >= r, "cool light text");
        assert_eq!(map_for(Theme::AeroDark, 72, 185, 105), (72, 185, 105));
        assert!(Theme::AeroDark.is_dark() && Theme::AeroDark.is_glass());
    }

    #[test]
    fn theme_keys_round_trip() {
        for theme in Theme::ALL {
            assert_eq!(Theme::from_key(theme.key()), theme);
        }
        assert_eq!(Theme::from_key("unknown"), Theme::Dark);
    }
}
