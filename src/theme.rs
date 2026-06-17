//! Color themes for the TUI. Ships Ethan Schoonover's canonical Solarized Dark
//! palette, exposed through a semantic [`Theme`] so the rest of the UI never
//! references raw palette values and additional themes can be added later.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};

/// Which palette is active. Cycled at runtime with the `t` key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThemeVariant {
    SolarizedDark,
    SolarizedLight,
}

impl ThemeVariant {
    /// Materialize the palette for this variant.
    pub fn theme(self) -> Theme {
        match self {
            ThemeVariant::SolarizedDark => Theme::solarized_dark(),
            ThemeVariant::SolarizedLight => Theme::solarized_light(),
        }
    }

    /// The next variant in the cycle.
    pub fn next(self) -> Self {
        match self {
            ThemeVariant::SolarizedDark => ThemeVariant::SolarizedLight,
            ThemeVariant::SolarizedLight => ThemeVariant::SolarizedDark,
        }
    }
}

/// Semantic colors used across the TUI, decoupled from any specific palette.
pub struct Theme {
    /// Human-readable palette name, shown in the status line.
    pub name: &'static str,
    /// Window background.
    pub bg: Color,
    /// Default body text.
    pub fg: Color,
    /// Secondary / de-emphasized text.
    pub dim: Color,
    /// Panel borders.
    pub border: Color,
    /// Panel titles.
    pub title: Color,
    /// Active tab / primary accent.
    pub accent: Color,
    /// Error and abort messages.
    pub error: Color,
    /// Foreground of the selected row.
    pub sel_fg: Color,
    /// Background of the selected row.
    pub sel_bg: Color,
    /// Per-kind accents for the backups list.
    pub staged: Color,
    pub git: Color,
    pub archive: Color,
    pub restore: Color,
}

impl Theme {
    /// Solarized Dark — https://ethanschoonover.com/solarized/.
    pub fn solarized_dark() -> Self {
        // Base tones (dark background → light content).
        let base03 = Color::Rgb(0x00, 0x2b, 0x36);
        let base01 = Color::Rgb(0x58, 0x6e, 0x75);
        let base0 = Color::Rgb(0x83, 0x94, 0x96);
        let base1 = Color::Rgb(0x93, 0xa1, 0xa1);
        // Accent tones.
        let yellow = Color::Rgb(0xb5, 0x89, 0x00);
        let red = Color::Rgb(0xdc, 0x32, 0x2f);
        let magenta = Color::Rgb(0xd3, 0x36, 0x82);
        let blue = Color::Rgb(0x26, 0x8b, 0xd2);
        let cyan = Color::Rgb(0x2a, 0xa1, 0x98);
        let green = Color::Rgb(0x85, 0x99, 0x00);

        Theme {
            name: "Solarized Dark",
            bg: base03,
            fg: base0,
            dim: base01,
            border: base01,
            title: base1,
            accent: cyan,
            error: red,
            sel_fg: base03,
            sel_bg: cyan,
            staged: yellow,
            git: green,
            archive: magenta,
            restore: blue,
        }
    }

    /// Solarized Light — same accent hues as Dark over the light base tones.
    pub fn solarized_light() -> Self {
        // Base tones (light background → dark content).
        let base3 = Color::Rgb(0xfd, 0xf6, 0xe3);
        let base1 = Color::Rgb(0x93, 0xa1, 0xa1);
        let base00 = Color::Rgb(0x65, 0x7b, 0x83);
        let base01 = Color::Rgb(0x58, 0x6e, 0x75);
        // Accent tones (identical to Dark).
        let yellow = Color::Rgb(0xb5, 0x89, 0x00);
        let red = Color::Rgb(0xdc, 0x32, 0x2f);
        let magenta = Color::Rgb(0xd3, 0x36, 0x82);
        let blue = Color::Rgb(0x26, 0x8b, 0xd2);
        let cyan = Color::Rgb(0x2a, 0xa1, 0x98);
        let green = Color::Rgb(0x85, 0x99, 0x00);

        Theme {
            name: "Solarized Light",
            bg: base3,
            fg: base00,
            dim: base1,
            border: base1,
            title: base01,
            accent: cyan,
            error: red,
            sel_fg: base3,
            sel_bg: cyan,
            staged: yellow,
            git: green,
            archive: magenta,
            restore: blue,
        }
    }

    /// Base text style (foreground over the theme background).
    pub fn text(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }

    /// A bordered panel themed with this palette and the given title.
    pub fn panel(&self, title: &str) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.border).bg(self.bg))
            .title(title.to_string())
            .title_style(Style::default().fg(self.title).add_modifier(Modifier::BOLD))
            .style(Style::default().bg(self.bg))
    }

    /// Highlight style for the selected list/tab entry.
    pub fn selection(&self) -> Style {
        Style::default()
            .fg(self.sel_fg)
            .bg(self.sel_bg)
            .add_modifier(Modifier::BOLD)
    }
}
