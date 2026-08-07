use ratatui::layout::Rect;

pub const WIDE_MIN_WIDTH: u16 = 120;
pub const WIDE_MIN_HEIGHT: u16 = 32;
pub const COMPACT_MIN_WIDTH: u16 = 60;
pub const COMPACT_MIN_HEIGHT: u16 = 18;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LayoutMode {
    Wide,
    Compact,
    Tiny,
}

impl LayoutMode {
    #[must_use]
    pub const fn from_dimensions(width: u16, height: u16) -> Self {
        if width >= WIDE_MIN_WIDTH && height >= WIDE_MIN_HEIGHT {
            Self::Wide
        } else if width >= COMPACT_MIN_WIDTH && height >= COMPACT_MIN_HEIGHT {
            Self::Compact
        } else {
            Self::Tiny
        }
    }

    #[must_use]
    pub const fn for_area(area: Rect) -> Self {
        Self::from_dimensions(area.width, area.height)
    }
}
