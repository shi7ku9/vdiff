//! Colors for the TUI.
//!
//! Every style the TUI draws comes from a [`Theme`], built for the color
//! depth the terminal supports (24-bit color, the 256-color palette, the
//! 16 basic ANSI colors, or no color at all with `NO_COLOR`) and for a
//! [`Scheme`]: dark or light, with red and green or, for readers who
//! tell those apart poorly, orange and blue.

use ratatui::style::{Color, Modifier, Style};

/// How many colors the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ColorDepth {
    /// Read the color depth from the environment.
    pub fn detect() -> ColorDepth {
        ColorDepth::from_env(|name| std::env::var(name).ok())
    }

    /// A non-empty `NO_COLOR` wins. `COLORTERM=truecolor` or `24bit`
    /// and Windows Terminal (`WT_SESSION`) select 24-bit color; a
    /// `TERM` containing `256color` selects the 256-color palette.
    /// Anything else gets the 16 basic colors.
    fn from_env(var: impl Fn(&str) -> Option<String>) -> ColorDepth {
        if var("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return ColorDepth::NoColor;
        }
        if var("COLORTERM").is_some_and(|v| v == "truecolor" || v == "24bit")
            || var("WT_SESSION").is_some()
        {
            return ColorDepth::TrueColor;
        }
        if var("TERM").is_some_and(|v| v.contains("256color")) {
            return ColorDepth::Ansi256;
        }
        ColorDepth::Ansi16
    }
}

/// A family of colors, picked with `--theme` or `VDIFF_THEME`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Scheme {
    /// Red and green on a dark terminal.
    #[default]
    Dark,
    /// Red and green on a light terminal.
    Light,
    /// Orange and blue on a dark terminal.
    DarkColorblind,
    /// Orange and blue on a light terminal.
    LightColorblind,
}

/// Every style the TUI draws, by role.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Changed cells on file rows.
    pub delete: Style,
    pub insert: Style,
    /// The fainter tint that continues a changed column below the file.
    pub delete_fill: Style,
    pub insert_fill: Style,
    /// Marker-row cells above changed columns.
    pub delete_tab: Style,
    pub insert_tab: Style,
    /// Unchanged cells. Clears any tint.
    pub plain: Style,
    /// `│` between diff groups.
    pub separator: Style,
    /// The sidebar divider and the help border.
    pub rule: Style,
    /// Secondary text.
    pub muted: Style,
    /// Key names in hints and help.
    pub key: Style,
    /// The `vdiff` badge in the top bar.
    pub logo: Style,
    /// The mode badge in the status bar.
    pub mode: Style,
    /// Warnings, such as a skipped LCS.
    pub warning: Style,
    /// The selected sidebar row, and the mark at its left edge.
    pub selection: Style,
    pub selection_mark: Style,
    /// Sidebar status letters.
    pub modified: Style,
    pub added: Style,
    pub deleted: Style,
    pub renamed: Style,
    /// Red and green marks: the column counts and the overview bar's
    /// change ticks.
    pub delete_mark: Style,
    pub insert_mark: Style,
    /// The part of the overview bar the pane shows.
    pub viewport: Style,
    /// The cell under the inspect cursor.
    pub cursor: Style,
}

impl Theme {
    pub fn new(depth: ColorDepth, scheme: Scheme) -> Theme {
        use Scheme::*;
        let palette = match (depth, scheme) {
            (ColorDepth::NoColor, _) => return Theme::monochrome(),
            (ColorDepth::TrueColor, Dark) => &TRUE_COLOR,
            (ColorDepth::TrueColor, Light) => &TRUE_COLOR_LIGHT,
            (ColorDepth::TrueColor, DarkColorblind) => &TRUE_COLOR_COLORBLIND,
            (ColorDepth::TrueColor, LightColorblind) => &TRUE_COLOR_LIGHT_COLORBLIND,
            (ColorDepth::Ansi256, Dark) => &ANSI_256,
            (ColorDepth::Ansi256, Light) => &ANSI_256_LIGHT,
            (ColorDepth::Ansi256, DarkColorblind) => &ANSI_256_COLORBLIND,
            (ColorDepth::Ansi256, LightColorblind) => &ANSI_256_LIGHT_COLORBLIND,
            (ColorDepth::Ansi16, Dark | Light) => &ANSI_16,
            (ColorDepth::Ansi16, DarkColorblind | LightColorblind) => &ANSI_16_COLORBLIND,
        };
        Theme::from_palette(palette)
    }

    pub fn detect(scheme: Scheme) -> Theme {
        Theme::new(ColorDepth::detect(), scheme)
    }

    fn from_palette(p: &Palette) -> Theme {
        // Every style that sets a background also sets the text color,
        // so the text stays legible on light and dark terminals alike.
        let tinted = |bg: Color| Style::new().fg(p.lane_text).bg(bg);
        let badge = |bg: Color| {
            let style = Style::new().fg(p.ink).bg(bg);
            if p.bold_badges {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        };
        let fg = |color: Color| Style::new().fg(color);
        Theme {
            delete: tinted(p.delete),
            insert: tinted(p.insert),
            delete_fill: tinted(p.delete_fill),
            insert_fill: tinted(p.insert_fill),
            delete_tab: badge(p.delete_tab),
            insert_tab: badge(p.insert_tab),
            plain: Style::new().fg(Color::Reset).bg(Color::Reset),
            separator: Style::new().fg(p.rule).bg(Color::Reset),
            rule: fg(p.rule),
            muted: fg(p.muted),
            key: Style::new().add_modifier(Modifier::BOLD),
            logo: badge(p.accent),
            mode: Style::new()
                .fg(p.badge_text)
                .bg(p.badge)
                .add_modifier(Modifier::BOLD),
            warning: badge(p.warning),
            selection: Style::new()
                .fg(p.badge_text)
                .bg(p.selection)
                .add_modifier(Modifier::BOLD),
            selection_mark: fg(p.accent),
            modified: fg(p.modified),
            added: fg(p.added),
            deleted: fg(p.deleted),
            renamed: fg(p.renamed),
            delete_mark: fg(p.deleted),
            insert_mark: fg(p.added),
            viewport: fg(p.accent),
            cursor: badge(p.accent),
        }
    }

    /// `NO_COLOR`: changes stay visible through the marker row, the
    /// separators, and reversed cells.
    fn monochrome() -> Theme {
        let plain = Style::new().fg(Color::Reset).bg(Color::Reset);
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let dim = Style::new().add_modifier(Modifier::DIM);
        let reversed = Style::new().add_modifier(Modifier::REVERSED);
        Theme {
            delete: plain.add_modifier(Modifier::REVERSED),
            insert: plain.add_modifier(Modifier::REVERSED),
            delete_fill: plain,
            insert_fill: plain,
            delete_tab: plain.add_modifier(Modifier::BOLD),
            insert_tab: plain.add_modifier(Modifier::BOLD),
            plain,
            separator: plain.add_modifier(Modifier::DIM),
            rule: dim,
            muted: dim,
            key: bold,
            logo: reversed.add_modifier(Modifier::BOLD),
            mode: reversed,
            warning: reversed.add_modifier(Modifier::BOLD),
            selection: reversed,
            selection_mark: bold,
            modified: bold,
            added: bold,
            deleted: bold,
            renamed: bold,
            delete_mark: Style::new(),
            insert_mark: Style::new(),
            viewport: bold,
            cursor: reversed.add_modifier(Modifier::BOLD),
        }
    }
}

struct Palette {
    /// Text on tinted diff cells.
    lane_text: Color,
    /// Dark text on bright badges and tabs.
    ink: Color,
    /// Text on the neutral badge and the selection.
    badge_text: Color,
    delete: Color,
    delete_fill: Color,
    delete_tab: Color,
    insert: Color,
    insert_fill: Color,
    insert_tab: Color,
    /// Separators and divider lines.
    rule: Color,
    muted: Color,
    accent: Color,
    warning: Color,
    /// Neutral badge background.
    badge: Color,
    selection: Color,
    modified: Color,
    added: Color,
    deleted: Color,
    renamed: Color,
    bold_badges: bool,
}

const TRUE_COLOR: Palette = Palette {
    lane_text: Color::Rgb(0xdc, 0xdf, 0xe4),
    ink: Color::Rgb(0x1a, 0x1b, 0x26),
    badge_text: Color::Rgb(0xc0, 0xca, 0xf5),
    delete: Color::Rgb(0x6b, 0x2a, 0x35),
    delete_fill: Color::Rgb(0x3a, 0x21, 0x29),
    delete_tab: Color::Rgb(0xe8, 0x66, 0x71),
    insert: Color::Rgb(0x2c, 0x5a, 0x3c),
    insert_fill: Color::Rgb(0x1f, 0x33, 0x26),
    insert_tab: Color::Rgb(0x8f, 0xcf, 0x7f),
    rule: Color::Rgb(0x3b, 0x42, 0x61),
    muted: Color::Rgb(0x56, 0x5f, 0x89),
    accent: Color::Rgb(0x7a, 0xa2, 0xf7),
    warning: Color::Rgb(0xe0, 0xaf, 0x68),
    badge: Color::Rgb(0x3b, 0x42, 0x61),
    selection: Color::Rgb(0x28, 0x34, 0x57),
    modified: Color::Rgb(0xe0, 0xaf, 0x68),
    added: Color::Rgb(0x8f, 0xcf, 0x7f),
    deleted: Color::Rgb(0xe8, 0x66, 0x71),
    renamed: Color::Rgb(0x7d, 0xcf, 0xff),
    bold_badges: true,
};

/// For light terminals: dark text on pale tints, white text on badges.
const TRUE_COLOR_LIGHT: Palette = Palette {
    lane_text: Color::Rgb(0x1f, 0x23, 0x28),
    ink: Color::Rgb(0xff, 0xff, 0xff),
    badge_text: Color::Rgb(0x1f, 0x23, 0x28),
    delete: Color::Rgb(0xff, 0xce, 0xcb),
    delete_fill: Color::Rgb(0xff, 0xeb, 0xe9),
    delete_tab: Color::Rgb(0xcf, 0x22, 0x2e),
    insert: Color::Rgb(0xac, 0xee, 0xbb),
    insert_fill: Color::Rgb(0xda, 0xfb, 0xe1),
    insert_tab: Color::Rgb(0x1a, 0x7f, 0x37),
    rule: Color::Rgb(0xd0, 0xd7, 0xde),
    muted: Color::Rgb(0x6e, 0x77, 0x81),
    accent: Color::Rgb(0x09, 0x69, 0xda),
    warning: Color::Rgb(0x9a, 0x67, 0x00),
    badge: Color::Rgb(0xd8, 0xde, 0xe4),
    selection: Color::Rgb(0xdd, 0xf4, 0xff),
    modified: Color::Rgb(0x9a, 0x67, 0x00),
    added: Color::Rgb(0x1a, 0x7f, 0x37),
    deleted: Color::Rgb(0xcf, 0x22, 0x2e),
    renamed: Color::Rgb(0x1b, 0x7c, 0x83),
    bold_badges: true,
};

/// Orange and blue from the Okabe–Ito palette in place of red and
/// green, and a neutral accent that neither can be mistaken for.
const TRUE_COLOR_COLORBLIND: Palette = Palette {
    delete: Color::Rgb(0x5c, 0x3a, 0x14),
    delete_fill: Color::Rgb(0x33, 0x24, 0x0f),
    delete_tab: Color::Rgb(0xe6, 0x9f, 0x00),
    insert: Color::Rgb(0x1d, 0x45, 0x63),
    insert_fill: Color::Rgb(0x14, 0x28, 0x3a),
    insert_tab: Color::Rgb(0x56, 0xb4, 0xe9),
    accent: Color::Rgb(0xc0, 0xca, 0xf5),
    warning: Color::Rgb(0xf0, 0xe4, 0x42),
    modified: Color::Rgb(0xf0, 0xe4, 0x42),
    added: Color::Rgb(0x56, 0xb4, 0xe9),
    deleted: Color::Rgb(0xe6, 0x9f, 0x00),
    renamed: Color::Rgb(0xcc, 0x79, 0xa7),
    ..TRUE_COLOR
};

const TRUE_COLOR_LIGHT_COLORBLIND: Palette = Palette {
    delete: Color::Rgb(0xff, 0xd8, 0xa8),
    delete_fill: Color::Rgb(0xff, 0xf0, 0xdb),
    delete_tab: Color::Rgb(0xb3, 0x59, 0x00),
    insert: Color::Rgb(0xc6, 0xe2, 0xf7),
    insert_fill: Color::Rgb(0xe6, 0xf2, 0xfb),
    insert_tab: Color::Rgb(0x00, 0x72, 0xb2),
    accent: Color::Rgb(0x57, 0x60, 0x6a),
    added: Color::Rgb(0x00, 0x72, 0xb2),
    deleted: Color::Rgb(0xb3, 0x59, 0x00),
    renamed: Color::Rgb(0x82, 0x50, 0xdf),
    ..TRUE_COLOR_LIGHT
};

/// The 256-color cube's darkest red and green (52, 22) are its only
/// tints dark enough for light text, and the next steps up are much
/// brighter in green than in red, so changed cells and the fill below
/// the file share them.
const ANSI_256: Palette = Palette {
    lane_text: Color::Indexed(255),
    ink: Color::Indexed(234),
    badge_text: Color::Indexed(252),
    delete: Color::Indexed(52),
    delete_fill: Color::Indexed(52),
    delete_tab: Color::Indexed(167),
    insert: Color::Indexed(22),
    insert_fill: Color::Indexed(22),
    insert_tab: Color::Indexed(114),
    rule: Color::Indexed(238),
    muted: Color::Indexed(60),
    accent: Color::Indexed(111),
    warning: Color::Indexed(179),
    badge: Color::Indexed(238),
    selection: Color::Indexed(237),
    modified: Color::Indexed(179),
    added: Color::Indexed(114),
    deleted: Color::Indexed(167),
    renamed: Color::Indexed(117),
    bold_badges: true,
};

const ANSI_256_LIGHT: Palette = Palette {
    lane_text: Color::Indexed(235),
    ink: Color::Indexed(231),
    badge_text: Color::Indexed(235),
    delete: Color::Indexed(224),
    delete_fill: Color::Indexed(224),
    delete_tab: Color::Indexed(160),
    insert: Color::Indexed(194),
    insert_fill: Color::Indexed(194),
    insert_tab: Color::Indexed(28),
    rule: Color::Indexed(252),
    muted: Color::Indexed(243),
    accent: Color::Indexed(25),
    warning: Color::Indexed(94),
    badge: Color::Indexed(252),
    selection: Color::Indexed(189),
    modified: Color::Indexed(94),
    added: Color::Indexed(28),
    deleted: Color::Indexed(160),
    renamed: Color::Indexed(31),
    bold_badges: true,
};

const ANSI_256_COLORBLIND: Palette = Palette {
    delete: Color::Indexed(94),
    delete_fill: Color::Indexed(94),
    delete_tab: Color::Indexed(214),
    insert: Color::Indexed(24),
    insert_fill: Color::Indexed(24),
    insert_tab: Color::Indexed(75),
    accent: Color::Indexed(252),
    warning: Color::Indexed(228),
    modified: Color::Indexed(228),
    added: Color::Indexed(75),
    deleted: Color::Indexed(214),
    renamed: Color::Indexed(176),
    ..ANSI_256
};

const ANSI_256_LIGHT_COLORBLIND: Palette = Palette {
    delete: Color::Indexed(223),
    delete_fill: Color::Indexed(223),
    delete_tab: Color::Indexed(130),
    insert: Color::Indexed(153),
    insert_fill: Color::Indexed(153),
    insert_tab: Color::Indexed(25),
    accent: Color::Indexed(240),
    added: Color::Indexed(25),
    deleted: Color::Indexed(130),
    renamed: Color::Indexed(97),
    ..ANSI_256_LIGHT
};

/// The terminal's own palette: black text on red and green, as the TUI
/// has always drawn it. Bold is left off because many terminals turn
/// bold black into gray.
const ANSI_16: Palette = Palette {
    lane_text: Color::Black,
    ink: Color::Black,
    badge_text: Color::White,
    delete: Color::Red,
    delete_fill: Color::Red,
    delete_tab: Color::Red,
    insert: Color::Green,
    insert_fill: Color::Green,
    insert_tab: Color::Green,
    rule: Color::DarkGray,
    muted: Color::DarkGray,
    accent: Color::Cyan,
    warning: Color::Yellow,
    badge: Color::DarkGray,
    selection: Color::DarkGray,
    modified: Color::Yellow,
    added: Color::Green,
    deleted: Color::Red,
    renamed: Color::Cyan,
    bold_badges: false,
};

/// Yellow and cyan in place of red and green; the accent turns white
/// so it differs from both.
const ANSI_16_COLORBLIND: Palette = Palette {
    delete: Color::Yellow,
    delete_fill: Color::Yellow,
    delete_tab: Color::Yellow,
    insert: Color::Cyan,
    insert_fill: Color::Cyan,
    insert_tab: Color::Cyan,
    accent: Color::White,
    modified: Color::Magenta,
    added: Color::Cyan,
    deleted: Color::Yellow,
    renamed: Color::LightBlue,
    ..ANSI_16
};

#[cfg(test)]
mod tests {
    use super::*;

    fn depth(vars: &[(&str, &str)]) -> ColorDepth {
        ColorDepth::from_env(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn detects_color_depth_from_environment() {
        assert_eq!(depth(&[]), ColorDepth::Ansi16);
        assert_eq!(depth(&[("TERM", "xterm")]), ColorDepth::Ansi16);
        assert_eq!(depth(&[("TERM", "xterm-256color")]), ColorDepth::Ansi256);
        assert_eq!(depth(&[("TERM", "tmux-256color")]), ColorDepth::Ansi256);
        assert_eq!(depth(&[("COLORTERM", "truecolor")]), ColorDepth::TrueColor);
        assert_eq!(depth(&[("COLORTERM", "24bit")]), ColorDepth::TrueColor);
        assert_eq!(depth(&[("WT_SESSION", "1")]), ColorDepth::TrueColor);
        assert_eq!(
            depth(&[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")]),
            ColorDepth::TrueColor
        );
    }

    #[test]
    fn no_color_wins_unless_empty() {
        assert_eq!(
            depth(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]),
            ColorDepth::NoColor
        );
        assert_eq!(
            depth(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")]),
            ColorDepth::TrueColor
        );
    }

    const SCHEMES: [Scheme; 4] = [
        Scheme::Dark,
        Scheme::Light,
        Scheme::DarkColorblind,
        Scheme::LightColorblind,
    ];
    const COLORED: [ColorDepth; 3] = [
        ColorDepth::TrueColor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
    ];

    #[test]
    fn tinted_styles_set_both_colors() {
        for (depth, scheme) in COLORED.into_iter().flat_map(|d| SCHEMES.map(|s| (d, s))) {
            let theme = Theme::new(depth, scheme);
            for style in [
                theme.delete,
                theme.insert,
                theme.delete_fill,
                theme.insert_fill,
                theme.delete_tab,
                theme.insert_tab,
                theme.logo,
                theme.mode,
                theme.warning,
                theme.selection,
            ] {
                assert!(
                    style.fg.is_some() && style.bg.is_some(),
                    "{depth:?} {scheme:?}: {style:?}"
                );
            }
        }
    }

    #[test]
    fn colorblind_schemes_leave_out_red_and_green() {
        for depth in COLORED {
            for (normal, colorblind) in [
                (Scheme::Dark, Scheme::DarkColorblind),
                (Scheme::Light, Scheme::LightColorblind),
            ] {
                let (normal, colorblind) =
                    (Theme::new(depth, normal), Theme::new(depth, colorblind));
                for (a, b) in [
                    (normal.delete_tab, colorblind.delete_tab),
                    (normal.insert_tab, colorblind.insert_tab),
                    (normal.delete, colorblind.delete),
                    (normal.insert, colorblind.insert),
                    (normal.logo, colorblind.logo),
                ] {
                    assert_ne!(a.bg, b.bg, "{depth:?}");
                }
            }
        }
        let theme = Theme::new(ColorDepth::Ansi16, Scheme::DarkColorblind);
        assert_eq!(theme.delete.bg, Some(Color::Yellow));
        assert_eq!(theme.insert.bg, Some(Color::Cyan));
    }

    #[test]
    fn light_schemes_put_dark_text_on_pale_tints() {
        for scheme in [Scheme::Light, Scheme::LightColorblind] {
            let theme = Theme::new(ColorDepth::TrueColor, scheme);
            for style in [theme.delete, theme.insert, theme.mode, theme.selection] {
                let (Some(Color::Rgb(r, g, b)), Some(Color::Rgb(br, bg, bb))) =
                    (style.fg, style.bg)
                else {
                    panic!("{style:?}");
                };
                let luma = |r: u8, g: u8, b: u8| 2 * u32::from(r) + 7 * u32::from(g) + u32::from(b);
                assert!(luma(r, g, b) < luma(br, bg, bb), "{scheme:?}: {style:?}");
            }
        }
    }

    #[test]
    fn monochrome_uses_no_colors() {
        let theme = Theme::new(ColorDepth::NoColor, Scheme::Light);
        for style in [
            theme.delete,
            theme.insert,
            theme.delete_fill,
            theme.delete_tab,
            theme.separator,
            theme.rule,
            theme.muted,
            theme.logo,
            theme.mode,
            theme.warning,
            theme.selection,
            theme.modified,
        ] {
            for color in [style.fg, style.bg].into_iter().flatten() {
                assert_eq!(color, Color::Reset, "{style:?}");
            }
        }
        assert!(theme.delete.add_modifier.contains(Modifier::REVERSED));
    }
}
