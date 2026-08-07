use std::io::IsTerminal as _;

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ColorCapability {
    #[default]
    TrueColor,
    Ansi256,
    Basic,
    Monochrome,
}

/// Secret-free terminal facts used for deterministic color-capability detection.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct TerminalColorSnapshot {
    output_is_terminal: bool,
    no_color: bool,
    term: Option<String>,
    colorterm: Option<String>,
}

impl TerminalColorSnapshot {
    #[must_use]
    pub fn new(
        output_is_terminal: bool,
        no_color: bool,
        term: Option<&str>,
        colorterm: Option<&str>,
    ) -> Self {
        Self {
            output_is_terminal,
            no_color,
            term: term.map(str::to_owned),
            colorterm: colorterm.map(str::to_owned),
        }
    }

    fn capture() -> Self {
        Self {
            output_is_terminal: std::io::stdout().is_terminal(),
            no_color: std::env::var_os("NO_COLOR").is_some(),
            term: std::env::var("TERM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
        }
    }
}

/// Maps a captured terminal environment to the closest supported palette.
#[must_use]
pub fn detect_color_capability(snapshot: &TerminalColorSnapshot) -> ColorCapability {
    if !snapshot.output_is_terminal || snapshot.no_color {
        return ColorCapability::Monochrome;
    }
    let term = snapshot
        .term
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if term == "dumb" {
        return ColorCapability::Monochrome;
    }
    let colorterm = snapshot
        .colorterm
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [term.as_str(), colorterm.as_str()].iter().any(|value| {
        value.contains("truecolor") || value.contains("24bit") || value.contains("direct")
    }) {
        return ColorCapability::TrueColor;
    }
    if term.contains("256color") {
        return ColorCapability::Ansi256;
    }
    ColorCapability::Basic
}

/// Detects the color capability for the stdout terminal without exposing env data.
#[must_use]
pub fn detected_color_capability() -> ColorCapability {
    detect_color_capability(&TerminalColorSnapshot::capture())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    capability: ColorCapability,
    pub background: Color,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    pub selection: Color,
    pub warning: Color,
}

impl Theme {
    #[must_use]
    pub const fn for_capability(capability: ColorCapability) -> Self {
        match capability {
            ColorCapability::TrueColor => Self {
                capability,
                background: Color::Rgb(10, 14, 20),
                foreground: Color::Rgb(224, 231, 239),
                muted: Color::Rgb(126, 142, 158),
                accent: Color::Rgb(100, 210, 255),
                selection: Color::Rgb(255, 204, 102),
                warning: Color::Rgb(255, 128, 128),
            },
            ColorCapability::Ansi256 => Self {
                capability,
                background: Color::Indexed(234),
                foreground: Color::Indexed(254),
                muted: Color::Indexed(245),
                accent: Color::Indexed(81),
                selection: Color::Indexed(221),
                warning: Color::Indexed(210),
            },
            ColorCapability::Basic => Self {
                capability,
                background: Color::Black,
                foreground: Color::White,
                muted: Color::DarkGray,
                accent: Color::Cyan,
                selection: Color::Yellow,
                warning: Color::LightRed,
            },
            ColorCapability::Monochrome => Self {
                capability,
                background: Color::Reset,
                foreground: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                selection: Color::Reset,
                warning: Color::Reset,
            },
        }
    }

    #[must_use]
    pub const fn capability(&self) -> ColorCapability {
        self.capability
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::for_capability(ColorCapability::TrueColor)
    }
}
