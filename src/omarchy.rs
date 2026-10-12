//! Follow the user's Omarchy theme.

use std::path::{Path, PathBuf};

use rataflow::Palette;
use tokio::sync::mpsc;

/// The palette a theme file's text asks for.
pub fn palette_from(_colors: &str) -> Option<Palette> {
    None
}

/// Where Omarchy keeps the current theme.
pub fn current_dir() -> Option<PathBuf> {
    None
}

/// The palette now in effect, and the ones that follow it.
pub struct Follow {
    pub palette: Palette,
    changes: mpsc::UnboundedReceiver<Palette>,
}

impl Follow {
    pub fn at(_current: &Path) -> Follow {
        Follow {
            palette: crate::state::graph::base_palette(false),
            changes: mpsc::unbounded_channel().1,
        }
    }

    pub async fn changed(&mut self) -> Option<Palette> {
        self.changes.recv().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::style::Color;

    use super::*;
    use crate::state::graph::base_palette;

    const SOLITUDE: &str = r##"mode = "dark"

accent = "#798186"
selection = "#343d41"
muted = "#4b4e55"

background = "#101315"
foreground = "#cacccc"

red = "#565d60"
green = "#9fa5a9"
hyprland_active_border = "rgba(798186ee) rgba(caccccee)"
"##;

    const FLEXOKI_LIGHT: &str = r##"mode = "light"
accent = "#205EA6"
muted = "#B7B5AC"
background = "#FFFCF0"
foreground = "#100F0F"
red = "#AF3029"
green = "#66800B"
"##;

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(r, g, b)
    }

    /// Whether `c` lies on the line from `from` to `to`, channel by channel.
    fn between(c: Color, from: Color, to: Color) -> bool {
        let (Color::Rgb(c0, c1, c2), Color::Rgb(a0, a1, a2), Color::Rgb(b0, b1, b2)) =
            (c, from, to)
        else {
            return false;
        };
        [(c0, a0, b0), (c1, a1, b1), (c2, a2, b2)]
            .iter()
            .all(|&(c, a, b)| a.min(b) <= c && c <= a.max(b))
    }

    #[test]
    fn a_dark_theme_sets_the_look_and_keeps_the_status_colors() {
        let palette = palette_from(SOLITUDE).unwrap();
        let own = base_palette(false);
        assert_eq!(palette.canvas_bg, rgb(0x10, 0x13, 0x15));
        assert_eq!(palette.text, rgb(0xca, 0xcc, 0xcc));
        assert_eq!(palette.muted, rgb(0x4b, 0x4e, 0x55));
        // Shades the file does not name sit between the ones it does.
        assert!(between(palette.surface, palette.canvas_bg, palette.text));
        assert_ne!(palette.surface, palette.canvas_bg);
        assert!(between(palette.subtle, palette.muted, palette.text));
        // The theme's own red and green never replace zoetrope's.
        assert_eq!(
            (palette.accent, palette.success, palette.error),
            (own.accent, own.success, own.error)
        );
    }

    #[test]
    fn a_light_theme_gets_dark_text_and_the_light_status_colors() {
        let palette = palette_from(FLEXOKI_LIGHT).unwrap();
        let own = base_palette(true);
        assert_eq!(palette.canvas_bg, rgb(0xff, 0xfc, 0xf0));
        assert_eq!(palette.text, rgb(0x10, 0x0f, 0x0f));
        assert_ne!(palette.surface, palette.canvas_bg);
        assert_eq!(
            (palette.accent, palette.success, palette.error),
            (own.accent, own.success, own.error)
        );
    }

    #[test]
    fn a_missing_muted_is_derived() {
        let palette = palette_from("background = \"#000000\"\nforeground = \"#ffffff\"").unwrap();
        assert!(between(palette.muted, palette.canvas_bg, palette.text));
        assert_ne!(palette.muted, palette.canvas_bg);
    }

    #[test]
    fn a_file_that_names_no_usable_look_is_no_theme() {
        for colors in [
            "",
            "not toml at all {{{",
            "mode = \"dark\"",
            "background = \"#101315\"",
            "background = \"#101315\"\nforeground = \"cacccc\"",
            "background = \"rgba(1,2,3,4)\"\nforeground = \"#cacccc\"",
        ] {
            assert_eq!(palette_from(colors), None, "{colors:?}");
        }
    }

    fn theme_dir(current: &Path, colors: &str) {
        std::fs::create_dir_all(current.join("theme")).unwrap();
        std::fs::write(current.join("theme/colors.toml"), colors).unwrap();
    }

    fn temp_current(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zoetrope_omarchy_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn without_a_usable_theme_file_the_colors_are_zoetropes_own() {
        let current = temp_current("none");
        assert_eq!(Follow::at(&current).palette, base_palette(false));
        theme_dir(&current, "background = \n{{{");
        assert_eq!(Follow::at(&current).palette, base_palette(false));
        assert_eq!(
            Follow::at(&current.join("missing")).palette,
            base_palette(false)
        );
        let _ = std::fs::remove_dir_all(&current);
    }

    /// `omarchy theme set` stages `next-theme`, then removes `theme` and moves
    /// the staged folder into its place: the file never changes in place.
    #[tokio::test]
    async fn a_theme_set_repaints_and_a_removed_theme_goes_back() {
        let current = temp_current("live");
        theme_dir(&current, SOLITUDE);
        let mut follow = Follow::at(&current);
        assert_eq!(follow.palette, palette_from(SOLITUDE).unwrap());

        let next = current.join("next-theme");
        std::fs::create_dir_all(&next).unwrap();
        std::fs::write(next.join("colors.toml"), FLEXOKI_LIGHT).unwrap();
        std::fs::remove_dir_all(current.join("theme")).unwrap();
        std::fs::rename(&next, current.join("theme")).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), follow.changed()).await;
        assert_eq!(changed.unwrap(), palette_from(FLEXOKI_LIGHT));

        std::fs::remove_dir_all(current.join("theme")).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), follow.changed()).await;
        assert_eq!(changed.unwrap(), Some(base_palette(false)));
        let _ = std::fs::remove_dir_all(&current);
    }
}
