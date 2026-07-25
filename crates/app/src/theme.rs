//! Visual identity (visual-identity.md): two designed palettes (dark/light),
//! a brand accent applied to the surfaces capability 4 names (selection,
//! focus rings, active toggles, the brand mark), a shared corner-radius/
//! spacing pass, and a code-drawn brand mark replacing the tray's placeholder
//! solid square. `Semantic` colours are deliberately **not** derived from the
//! accent (decision 5) — a clip indicator must read as danger under every
//! preset.

use eframe::egui;
use engine::{AccentChoice, ThemeChoice};

/// One accent, in both palettes — two hand-picked values rather than one
/// lightness-shifted (decision 2): a colour that reads well on dark often
/// does not on light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    pub dark: egui::Color32,
    pub light: egui::Color32,
}

/// Maps a persisted choice to its actual colour pair. Contrast-verified
/// against both palettes by `every_accent_meets_minimum_contrast_on_both_palettes`.
pub fn accent(choice: AccentChoice) -> Accent {
    match choice {
        AccentChoice::Brand => Accent {
            dark: egui::Color32::from_rgb(90, 170, 255),
            light: egui::Color32::from_rgb(0, 102, 204),
        },
        AccentChoice::Teal => Accent {
            dark: egui::Color32::from_rgb(64, 200, 180),
            light: egui::Color32::from_rgb(0, 120, 110),
        },
        AccentChoice::Amber => Accent {
            dark: egui::Color32::from_rgb(240, 180, 60),
            light: egui::Color32::from_rgb(170, 110, 0),
        },
        AccentChoice::Violet => Accent {
            dark: egui::Color32::from_rgb(170, 140, 255),
            light: egui::Color32::from_rgb(110, 70, 200),
        },
        AccentChoice::Slate => Accent {
            dark: egui::Color32::from_rgb(140, 160, 190),
            light: egui::Color32::from_rgb(70, 90, 120),
        },
    }
}

/// Colours that carry meaning. **Never derived from the accent** (decision 5):
/// a clip indicator must read as danger regardless of the chosen accent.
/// Replaces `ui.rs`'s `METER_GREEN`/`METER_AMBER`/`METER_RED` and the
/// duplicated routing-degraded warning colour (capability 8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Semantic {
    pub meter_ok: egui::Color32,
    pub meter_hot: egui::Color32,
    pub meter_clip: egui::Color32,
    pub warning: egui::Color32,
}

/// Dark and light get their own hand-picked values, not one value reused —
/// the dark-mode meter colours read fine on `Visuals::dark()`'s near-black
/// background but are too low-contrast against `Visuals::light()`'s.
pub fn semantic(theme: egui::Theme) -> Semantic {
    match theme {
        egui::Theme::Dark => Semantic {
            meter_ok: egui::Color32::from_rgb(60, 180, 90),
            meter_hot: egui::Color32::from_rgb(220, 170, 40),
            meter_clip: egui::Color32::from_rgb(220, 70, 45),
            warning: egui::Color32::from_rgb(220, 80, 40),
        },
        egui::Theme::Light => Semantic {
            meter_ok: egui::Color32::from_rgb(20, 130, 60),
            meter_hot: egui::Color32::from_rgb(150, 100, 0),
            meter_clip: egui::Color32::from_rgb(190, 30, 30),
            warning: egui::Color32::from_rgb(190, 60, 20),
        },
    }
}

/// Per-theme palette (capability 1). Extends egui's own `dark()`/`light()`
/// bases — deliberately *overriding* the accent-linked surfaces capability 4
/// names (selection, focus rings, active toggles) rather than hand-authoring
/// every one of egui's widget-state colours from scratch, which would be
/// restyling far beyond decision 4's scope (palette/accent/radius/spacing,
/// no typography, no per-widget overrides).
pub fn visuals(theme: egui::Theme, accent: Accent) -> egui::Visuals {
    let mut v = match theme {
        egui::Theme::Dark => egui::Visuals::dark(),
        egui::Theme::Light => egui::Visuals::light(),
    };

    let a = match theme {
        egui::Theme::Dark => accent.dark,
        egui::Theme::Light => accent.light,
    };

    v.selection.bg_fill = a;
    v.selection.stroke.color = a;
    v.hyperlink_color = a;
    v.widgets.active.bg_fill = a;
    // Fixed per theme, not derived from `a` at runtime -- decision 8 keeps
    // `contrast_ratio` test-only. Every dark-variant accent above is bright
    // enough for near-black text, every light-variant dark enough for white.
    v.widgets.active.fg_stroke.color = match theme {
        egui::Theme::Dark => egui::Color32::from_gray(20),
        egui::Theme::Light => egui::Color32::WHITE,
    };

    v
}

/// Theme-independent: corner radius, spacing, stroke widths (capability 7) --
/// one set of values serves both palettes, applied on top of each via
/// [`install`]'s `all_styles_mut` call rather than replacing either palette's
/// `Style` wholesale (which would also discard [`visuals`]'s per-theme
/// colours, since `egui::Style` embeds its own `Visuals`).
pub fn style() -> egui::Style {
    let mut style = egui::Style::default();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(10);
    style.spacing.menu_margin = egui::Margin::same(6);

    let radius = egui::CornerRadius::same(6);
    style.visuals.widgets.noninteractive.corner_radius = radius;
    style.visuals.widgets.inactive.corner_radius = radius;
    style.visuals.widgets.hovered.corner_radius = radius;
    style.visuals.widgets.active.corner_radius = radius;
    style.visuals.widgets.open.corner_radius = radius;
    style.visuals.window_corner_radius = radius;
    style.visuals.menu_corner_radius = radius;

    style.visuals.widgets.noninteractive.bg_stroke.width = 1.0;
    style.visuals.widgets.inactive.bg_stroke.width = 1.0;
    style.visuals.widgets.hovered.bg_stroke.width = 1.0;
    style.visuals.widgets.active.bg_stroke.width = 1.0;
    style.visuals.widgets.open.bg_stroke.width = 1.0;

    style
}

/// Copies just the corner-radius/stroke-width slice of `src` onto `dst`,
/// leaving `dst`'s own colours (already installed by [`visuals`]) untouched.
fn copy_radius_and_stroke_width(dst: &mut egui::style::WidgetVisuals, src: &egui::style::WidgetVisuals) {
    dst.corner_radius = src.corner_radius;
    dst.bg_stroke.width = src.bg_stroke.width;
}

/// Applies [`style`]'s shared radius/spacing/stroke values onto an
/// already-colour-installed `Style`, for both the dark and light copy
/// `all_styles_mut` iterates over.
fn apply_shared_style(style: &mut egui::Style, shared: &egui::Style) {
    style.spacing = shared.spacing.clone();
    style.visuals.window_corner_radius = shared.visuals.window_corner_radius;
    style.visuals.menu_corner_radius = shared.visuals.menu_corner_radius;
    copy_radius_and_stroke_width(&mut style.visuals.widgets.noninteractive, &shared.visuals.widgets.noninteractive);
    copy_radius_and_stroke_width(&mut style.visuals.widgets.inactive, &shared.visuals.widgets.inactive);
    copy_radius_and_stroke_width(&mut style.visuals.widgets.hovered, &shared.visuals.widgets.hovered);
    copy_radius_and_stroke_width(&mut style.visuals.widgets.active, &shared.visuals.widgets.active);
    copy_radius_and_stroke_width(&mut style.visuals.widgets.open, &shared.visuals.widgets.open);
}

/// The only impure function here -- registers both palettes, the shared
/// style and the preference (capability 11: called from the `CreationContext`
/// closure before the first frame, and again on any change, Flow B).
pub fn install(ctx: &egui::Context, theme: ThemeChoice, accent_choice: AccentChoice) {
    let acc = accent(accent_choice);
    ctx.set_visuals_of(egui::Theme::Dark, visuals(egui::Theme::Dark, acc));
    ctx.set_visuals_of(egui::Theme::Light, visuals(egui::Theme::Light, acc));

    let shared = style();
    ctx.all_styles_mut(|style| apply_shared_style(style, &shared));

    ctx.set_theme(theme_preference(theme));
}

fn theme_preference(theme: ThemeChoice) -> egui::ThemePreference {
    match theme {
        ThemeChoice::Dark => egui::ThemePreference::Dark,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::System => egui::ThemePreference::System,
    }
}

/// Rasterized brand mark, `size` x `size` RGBA8, straight alpha (decision 6).
/// One implementation feeds the tray icon, the window icon and any in-window
/// brand display. `theme` is the *surface it will sit on* -- the system
/// theme for the tray (decision 7), the app's theme in-window -- so it picks
/// `accent.dark`/`accent.light` directly rather than taking a `ThemeChoice`.
/// A plain filled circle: legible at 16px, no glyph/font risk, same
/// custom-paint idiom as `paint_meter`/`speaker_mute_button`.
pub fn brand_icon_rgba(size: u32, accent: Accent, theme: egui::Theme) -> Vec<u8> {
    let color = match theme {
        egui::Theme::Dark => accent.dark,
        egui::Theme::Light => accent.light,
    };

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0 - 0.5;
    let radius = size as f32 / 2.0 - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius * radius {
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = color.r();
                rgba[i + 1] = color.g();
                rgba[i + 2] = color.b();
                rgba[i + 3] = 255;
            }
        }
    }

    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 1.4.11 "non-text contrast" minimum -- the accent draws
    /// selection fills, focus rings and active-toggle backgrounds, all
    /// graphical rather than small text.
    const MIN_CONTRAST: f64 = 3.0;

    const DARK_BG: egui::Color32 = egui::Color32::from_gray(27); // Visuals::dark().panel_fill
    const LIGHT_BG: egui::Color32 = egui::Color32::from_gray(248); // Visuals::light().panel_fill

    /// Capability 6's guarantee. **Test-only** (decision 8): the only
    /// consumer is this check, and user-defined accents are out of scope, so
    /// a runtime contrast guard would solve a problem this version doesn't
    /// have.
    fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f64 {
        fn channel(c: u8) -> f64 {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        fn relative_luminance(c: egui::Color32) -> f64 {
            0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
        }
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn contrast_ratio_of_black_on_white_is_twenty_one() {
        let ratio = contrast_ratio(egui::Color32::BLACK, egui::Color32::WHITE);
        assert!((ratio - 21.0).abs() < 1e-9, "expected 21.0, got {ratio}");
    }

    #[test]
    fn every_accent_meets_minimum_contrast_on_both_palettes() {
        for choice in [
            AccentChoice::Brand,
            AccentChoice::Teal,
            AccentChoice::Amber,
            AccentChoice::Violet,
            AccentChoice::Slate,
        ] {
            let a = accent(choice);
            let dark_ratio = contrast_ratio(a.dark, DARK_BG);
            assert!(
                dark_ratio >= MIN_CONTRAST,
                "{choice:?} dark variant contrast {dark_ratio} below minimum {MIN_CONTRAST}"
            );
            let light_ratio = contrast_ratio(a.light, LIGHT_BG);
            assert!(
                light_ratio >= MIN_CONTRAST,
                "{choice:?} light variant contrast {light_ratio} below minimum {MIN_CONTRAST}"
            );
        }
    }

    #[test]
    fn semantic_colours_do_not_vary_with_the_accent() {
        // The actual guarantee (decision 5) is `semantic`'s signature: it has
        // no accent parameter at all, so no accent value can reach it -- a
        // compile-time fact, not something a runtime assertion can add to.
        // What *is* worth asserting here: the two themes' semantic sets are
        // real, distinct colours, not e.g. both silently defaulted to the
        // same fallback value regardless of `theme`.
        assert_ne!(semantic(egui::Theme::Dark), semantic(egui::Theme::Light));
    }

    #[test]
    fn brand_icon_rgba_returns_size_squared_times_four_bytes() {
        let a = accent(AccentChoice::Brand);
        for size in [1, 16, 32] {
            let bytes = brand_icon_rgba(size, a, egui::Theme::Dark);
            assert_eq!(bytes.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn the_brand_mark_is_not_fully_transparent() {
        let a = accent(AccentChoice::Brand);
        let bytes = brand_icon_rgba(16, a, egui::Theme::Dark);
        assert!(bytes.iter().skip(3).step_by(4).any(|&alpha| alpha != 0));
    }

    #[test]
    fn the_brand_mark_differs_between_light_and_dark_surfaces() {
        let a = accent(AccentChoice::Brand);
        let dark = brand_icon_rgba(16, a, egui::Theme::Dark);
        let light = brand_icon_rgba(16, a, egui::Theme::Light);
        assert_ne!(dark, light);
    }
}
