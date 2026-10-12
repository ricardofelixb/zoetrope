//! Follow the user's Omarchy theme.
//!
//! Omarchy writes the current theme's colors to
//! `~/.local/state/omarchy/current/theme/colors.toml` and replaces the whole
//! `theme` folder on every `omarchy theme set` (a staged `next-theme` is moved
//! over it), so the watch is on `current/`, not on the file, and every change
//! reads the file afresh. Where that file is absent, unreadable or not a theme
//! (macOS, Windows, a machine without Omarchy) the colors are zoetrope's own,
//! and nothing is reported: a theme is a courtesy, never a failure.
//!
//! What the theme names sets the look: the canvas, the text and the borders.
//! The surface of a card and the subtle text are derived from them, so they
//! stay off the background whatever the theme. The status colors are not the
//! theme's: a near-monochrome theme would make running and failed look alike.
//! See [`base_palette`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rataflow::Palette;
use ratatui::style::Color;
use tokio::sync::mpsc;

use crate::state::graph::base_palette;

/// How long a change is left to settle before the theme is read: one
/// `omarchy theme set` touches the folder several times over a moment.
const SETTLE: Duration = Duration::from_millis(100);

type Rgb = [u8; 3];

/// The `key = "value"` lines of a theme file. Anything else is skipped, so a
/// line it cannot read costs only that line.
fn entries(colors: &str) -> impl Iterator<Item = (&str, &str)> {
    colors.lines().filter_map(|line| {
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        Some((key.trim(), value[1..].split(quote).next()?))
    })
}

/// A `#rrggbb` color.
fn hex(value: &str) -> Option<Rgb> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(digits, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// `from`, `t` of the way to `to`.
fn mix(from: Rgb, to: Rgb, t: f32) -> Rgb {
    std::array::from_fn(|i| (from[i] as f32 + (to[i] as f32 - from[i] as f32) * t).round() as u8)
}

fn color([r, g, b]: Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// The palette a theme file asks for, if it names a background and a
/// foreground. Without a `muted` the borders are derived from those two.
pub fn palette_from(colors: &str) -> Option<Palette> {
    let named = |key: &str| entries(colors).find(|(k, _)| *k == key).map(|(_, v)| v);
    let (background, foreground) = (hex(named("background")?)?, hex(named("foreground")?)?);
    let muted = named("muted")
        .and_then(hex)
        .unwrap_or_else(|| mix(background, foreground, 0.25));
    let mut palette = base_palette(named("mode") == Some("light"));
    palette.canvas_bg = color(background);
    palette.surface = color(mix(background, foreground, 0.06));
    palette.muted = color(muted);
    palette.subtle = color(mix(muted, foreground, 0.35));
    palette.text = color(foreground);
    Some(palette)
}

/// Where Omarchy keeps the current theme.
pub fn current_dir() -> Option<PathBuf> {
    Some(crate::provider::home_dir()?.join(".local/state/omarchy/current"))
}

/// The palette in effect for the theme in `current`.
fn resolve(current: &Path) -> Palette {
    std::fs::read_to_string(current.join("theme/colors.toml"))
        .ok()
        .and_then(|colors| palette_from(&colors))
        .unwrap_or_else(|| base_palette(false))
}

/// The palette now in effect, and the ones that follow it.
pub struct Follow {
    pub palette: Palette,
    changes: mpsc::UnboundedReceiver<Palette>,
    /// Held so the watch stays open.
    _watch: Option<RecommendedWatcher>,
}

impl Follow {
    /// Follow Omarchy's current theme.
    pub fn start() -> Follow {
        current_dir().map_or_else(Follow::unthemed, |dir| Follow::at(&dir))
    }

    /// Follow the theme in `current`: read it now, and again after each change
    /// to the folder. Needs a tokio runtime.
    pub fn at(current: &Path) -> Follow {
        let palette = resolve(current);
        let (touched_tx, mut touched) = mpsc::unbounded_channel();
        let watch = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // Looking at the folder is not changing it.
            if event.is_ok_and(|e| !e.kind.is_access()) {
                let _ = touched_tx.send(());
            }
        })
        .and_then(|mut watch| {
            watch
                .watch(current, RecursiveMode::NonRecursive)
                .map(|()| watch)
        })
        .ok();
        let Some(watch) = watch else {
            return Follow {
                palette,
                ..Follow::unthemed()
            };
        };

        let (tx, changes) = mpsc::unbounded_channel();
        let (dir, mut shown) = (current.to_path_buf(), palette);
        tokio::spawn(async move {
            // Ends when the watch, and with it `touched`, is dropped.
            while touched.recv().await.is_some() {
                tokio::time::sleep(SETTLE).await;
                while touched.try_recv().is_ok() {}
                let now = resolve(&dir);
                if now != shown {
                    shown = now;
                    if tx.send(now).is_err() {
                        break;
                    }
                }
            }
        });
        Follow {
            palette,
            changes,
            _watch: Some(watch),
        }
    }

    /// Zoetrope's own colors, with nothing to follow: [`changed`](Self::changed)
    /// never answers.
    fn unthemed() -> Follow {
        Follow {
            palette: base_palette(false),
            changes: mpsc::unbounded_channel().1,
            _watch: None,
        }
    }

    /// The next palette, once the theme changes to one. `None` when nothing
    /// is being followed.
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
