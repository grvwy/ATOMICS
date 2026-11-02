mod fonts;

use std::path::Path;

use atomics_core::config::ThemeConfig;
use atomics_core::theme::{Resolved, ThemeMode, ThemeTokens};

pub use atomics_ui::keys::{
    consume_chord, consume_physical_shortcut, is_app_chord, is_digit, shortcut, shortcut_label,
};
pub use atomics_ui::theme::{Fonts, Palette, lerp};
pub use fonts::SystemFonts;

/// The theme as the host keeps it, free of egui: a built-in or theme file plus `[theme]` overrides, resolved
/// to tokens, the fonts they name, and the syntax theme the highlighter draws with. Terminals take their colors
/// from it; the UI turns it into a [`Palette`].
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Every token set.
    pub tokens: ThemeTokens,
    pub fonts: Fonts,
    /// Syntax theme for the diff and file view; one the highlighter has.
    pub syntax: String,
}

impl Theme {
    /// `[theme]` resolved against the built-in themes only.
    #[cfg(test)]
    pub fn from_config(theme: &ThemeConfig) -> Theme {
        Theme::from_resolved(atomics_core::theme::resolve(theme, |_| None), Path::new(".")).0
    }

    /// `[theme]` resolved against the theme files in `dir`, with what could not be applied.
    pub fn load(theme: &ThemeConfig, dir: &Path) -> (Theme, Vec<String>) {
        Theme::from_resolved(atomics_core::theme::load(theme, dir), dir)
    }

    fn from_resolved(resolved: Resolved, dir: &Path) -> (Theme, Vec<String>) {
        let Resolved { tokens, mut errors, .. } = resolved;
        let mode = tokens.mode.unwrap_or(ThemeMode::Dark);
        let syntax = match &tokens.syntax {
            Some(name) if crate::highlight::theme_named(name).is_some() => name.clone(),
            Some(name) => {
                errors.push(format!("unknown syntax theme \"{name}\""));
                mode.default_syntax().into()
            }
            None => mode.default_syntax().into(),
        };
        let fonts = Fonts {
            ui: tokens.ui_font.as_deref().map(|value| fonts::parse(value, dir)),
            mono: tokens.mono_font.as_deref().map(|value| fonts::parse(value, dir)),
            ui_size: tokens.ui_size,
            mono_size: tokens.mono_size,
            diff_size: tokens.diff_size,
        };
        (Theme { tokens, fonts, syntax }, errors)
    }

    /// What a proposed theme file would look like, resolved against the other theme files in `dir`.
    pub fn preview(name: &str, text: &str, dir: &Path) -> (Theme, Vec<String>) {
        let config = ThemeConfig { name: name.into(), ..ThemeConfig::default() };
        let resolved = atomics_core::theme::resolve(&config, |file| {
            if file == name { Some(Ok(text.to_string())) } else { atomics_core::theme::read_file(dir, file) }
        });
        Theme::from_resolved(resolved, dir)
    }

    pub fn mode(&self) -> ThemeMode {
        self.tokens.mode.unwrap_or(ThemeMode::Dark)
    }

    /// `COLORFGBG` for programs that read it instead of asking the terminal: white on black, or black on white.
    pub fn colorfgbg(&self) -> &'static str {
        match self.mode() {
            ThemeMode::Dark => "15;0",
            ThemeMode::Light => "0;15",
        }
    }

    /// The theme in egui colors, with its fonts.
    pub fn palette(&self) -> Palette {
        Palette::new(&self.tokens, self.fonts.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_ui::theme::FontSource;
    use egui::Color32;

    fn rgb(hex: u32) -> Color32 {
        Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    #[test]
    fn themes_resolve_their_syntax_and_overrides() {
        let theme = Theme::from_config(&ThemeConfig::default());
        assert_eq!(theme.syntax, "Monokai Extended");
        assert_eq!(theme.colorfgbg(), "15;0");
        let palette = theme.palette();
        assert_eq!(palette.accent, rgb(0xd77757));
        assert!(palette.mono_ui);
        let mut custom = ThemeConfig::default();
        custom.tokens.cursor = Some("#00ff00".into());
        assert_eq!(Theme::from_config(&custom).palette().cursor, rgb(0x00ff00));
        let midnight = ThemeConfig { name: "midnight".into(), ..ThemeConfig::default() };
        assert_eq!(Theme::from_config(&midnight).syntax, "OneHalfDark");
        let paper = Theme::from_config(&ThemeConfig { name: "paper".into(), ..ThemeConfig::default() });
        assert_eq!(
            (paper.mode(), paper.syntax.as_str(), paper.colorfgbg()),
            (ThemeMode::Light, "OneHalfLight", "0;15")
        );
        assert_eq!(paper.palette().mode, ThemeMode::Light);
    }

    #[test]
    fn theme_files_reach_the_palette_and_style() {
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mine.toml"),
            "extends = \"midnight\"\nsidebar = \"#010203\"\noverlay = \"#040506\"\nseparator = \"#070809\"\nquiet = \"#0a0b0c\"\nsyntax = \"no such theme\"\nui_font = \"fonts/ui.ttf\"\nmono_font = \"IBM Plex Mono\"\nui_size = 15.6",
        )
        .unwrap();
        let config = ThemeConfig { name: "mine".into(), ..ThemeConfig::default() };
        let (theme, errors) = Theme::load(&config, dir.path());
        assert_eq!(errors, ["unknown syntax theme \"no such theme\""]);
        assert_eq!(theme.syntax, "OneHalfDark", "an unknown syntax theme falls back to the mode's default");
        assert_eq!(theme.fonts.ui, Some(FontSource::File(dir.path().join("fonts/ui.ttf"))));
        assert_eq!(theme.fonts.mono, Some(FontSource::Family("IBM Plex Mono".into())));
        let palette = theme.palette();
        assert_eq!(palette.sidebar, rgb(0x010203));
        assert_eq!(palette.fonts, theme.fonts);
        assert!((palette.ui_scale() - 1.2).abs() < 1e-6);
        assert!((palette.mono(13.0).size * 1.2 - 13.0).abs() < 1e-4, "terminal text keeps its size under zoom");
        let style = palette.style();
        assert_eq!(style.visuals.window_fill, rgb(0x040506));
        assert_eq!(style.visuals.widgets.noninteractive.bg_stroke.color, rgb(0x070809));
        assert_eq!(style.visuals.weak_text_color, Some(rgb(0x0a0b0c)));
        assert_eq!(style.visuals.selection.bg_fill, palette.selection);
        assert!(style.visuals.dark_mode);
    }

    #[test]
    fn proposed_themes_preview_with_their_own_colors() {
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::write(dir.path().join("base.toml"), "extends = \"paper\"\nadded = \"#c8f0d0\"").unwrap();
        let text = "extends = \"base\"\nterminal = \"#fffdf6\"\naccent = \"#2a6fdb\"\non_accent = \"#fafafa\"";
        let (theme, errors) = Theme::preview("ocean", text, dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        let palette = theme.palette();
        assert_eq!(palette.mode, ThemeMode::Light, "the preview follows extends through the other files");
        assert_eq!(palette.added, rgb(0xc8f0d0));
        assert_eq!(palette.accent, rgb(0x2a6fdb));

        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            palette.show_preview(ui);
        });
        let fills: Vec<Color32> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) => Some(rect.fill),
                _ => None,
            })
            .collect();
        for (token, color) in [
            ("terminal", palette.terminal),
            ("accent", palette.accent),
            ("added", palette.added),
            ("selection", palette.selection),
            ("overlay", palette.overlay),
            ("ansi 9", palette.ansi[9]),
        ] {
            assert!(fills.contains(&color), "the preview paints {token}");
        }
        let send = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Send" => Some(text.clone()),
            _ => None,
        });
        assert_eq!(send.unwrap().fallback_color, palette.on_accent, "Send is written in on_accent");
        output.drop_without_applying_deltas();
    }

    /// Colors written into UI code instead of taken from the theme, as `file:line: code`.
    fn color_literals(file: &Path, text: &str) -> Vec<String> {
        let code: Vec<&str> = text.lines().map(|line| line.split("//").next().unwrap_or("")).collect();
        let joined = code.join("\n");
        let numeric = |arg: &str| {
            let arg = arg.trim().replace('_', "");
            let unsuffixed = |text: &str, suffixes: &[&str]| {
                suffixes.iter().find_map(|suffix| text.strip_suffix(suffix)).unwrap_or(text).to_string()
            };
            match arg.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(&unsuffixed(hex, &["u8", "u32", "u64"]), 16).is_ok(),
                None => unsuffixed(&arg, &["u8", "u32", "u64", "f32", "f64"]).parse::<f64>().is_ok(),
            }
        };
        let mut found = Vec::new();
        for ty in ["Color32::", "Rgba::", "Hsva::", "Hsva {", "hex_color!("] {
            for (at, _) in joined.match_indices(ty) {
                let rest = &joined[at + ty.len()..];
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let args = rest[name.len()..].strip_prefix('(').map(|args| {
                    let mut depth = 1;
                    let end = args
                        .char_indices()
                        .find(|&(_, c)| {
                            depth += match c {
                                '(' => 1,
                                ')' => -1,
                                _ => 0,
                            };
                            depth == 0
                        })
                        .map_or(args.len(), |(end, _)| end);
                    &args[..end]
                });
                let literal = match name.as_str() {
                    "TRANSPARENT" => false,
                    constant if constant.chars().next().is_some_and(char::is_uppercase) => true,
                    "from_gray" | "from_white_alpha" | "from_black_alpha" | "additive_luminance" | "from_hex" => true,
                    _ if ty == "Hsva {" || ty == "hex_color!(" => true,
                    _ => args.is_some_and(|args| args.split(',').any(numeric)),
                };
                if literal {
                    let line = joined[..at].matches('\n').count();
                    found.push(format!("{}:{}: {}", file.display(), line + 1, code[line].trim()));
                }
            }
        }
        found
    }

    #[test]
    fn ui_code_takes_every_color_from_the_theme() {
        fn rust_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, files);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }
        // The desktop's UI code and the web UI crate's; only their theme.rs write colors.
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut files = Vec::new();
        for src in [crates.join("atomics/src"), crates.join("atomics-ui/src")] {
            let mut found = Vec::new();
            rust_files(&src, &mut found);
            files.extend(found.into_iter().filter(|file| *file != src.join("theme.rs")));
        }
        assert!(files.len() > 20, "the scan must see the UI code");
        assert!(
            files.iter().any(|file| file.ends_with("atomics-ui/src/backend.rs")),
            "the scan must see the web UI crate"
        );
        let found: Vec<String> = files
            .iter()
            .flat_map(|file| {
                color_literals(file.strip_prefix(crates).unwrap(), &std::fs::read_to_string(file).unwrap())
            })
            .collect();
        assert!(found.is_empty(), "use a theme token from the theme instead of:\n{}", found.join("\n"));

        // The scan itself catches each way of writing a color.
        let file = Path::new("x.rs");
        for literal in [
            "let c = Color32::WHITE;",
            "egui::Color32::from_rgb(255, 0, 0)",
            "Color32::from_rgba_unmultiplied(\n    0x10,\n    a,\n    b,\n    c,\n)",
            "Color32::from_white_alpha(8)",
            "Color32::from_gray(v)",
            "egui::Rgba::from_rgb(1.0, 0.5, 0.0)",
            "Color32::from_rgb(10u8, g, b)",
            "Color32::from_rgb(0xff, 0x32, 0)",
            "Color32::from_rgb(r, 0x1fu8, b)",
        ] {
            assert_eq!(color_literals(file, literal).len(), 1, "{literal}");
        }
        for allowed in [
            "Color32::TRANSPARENT",
            "Color32::from_rgb(rgb.r, rgb.g, rgb.b)",
            "Color32::from_rgba_unmultiplied(s.color[0], s.color[1], s.color[2], s.color[3])",
            "fn chip(color: Color32) {} // Color32::WHITE in a comment",
        ] {
            assert!(color_literals(file, allowed).is_empty(), "{allowed}");
        }
    }
}
