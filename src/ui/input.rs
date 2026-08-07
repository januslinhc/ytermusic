use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MediaKeyCode};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextEntryContext {
    Search,
    Palette,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum InputMode {
    #[default]
    Normal,
    TextEntry(TextEntryContext),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticAction {
    Quit,
    ToggleHelp,
    OpenSearch,
    OpenPalette,
    TogglePlayback,
    NextTrack,
    PreviousTrack,
    ToggleFavorite,
    SeekBackward,
    SeekForward,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    CycleFocusForward,
    CycleFocusBackward,
    VolumeUp,
    VolumeDown,
    ToggleShuffle,
    CycleRepeat,
    ToggleRadio,
    MoveQueueItemUp,
    MoveQueueItemDown,
    ConnectAccount,
    LoadMore,
    RecheckDependencies,
    ChooseCountry,
    ToggleLyrics,
    ToggleQueuePanel,
    Cancel,
    DeleteBackward,
    Submit,
}

impl SemanticAction {
    pub const ALL: [Self; 32] = [
        Self::Quit,
        Self::ToggleHelp,
        Self::OpenSearch,
        Self::OpenPalette,
        Self::TogglePlayback,
        Self::NextTrack,
        Self::PreviousTrack,
        Self::ToggleFavorite,
        Self::SeekBackward,
        Self::SeekForward,
        Self::MoveUp,
        Self::MoveDown,
        Self::MoveLeft,
        Self::MoveRight,
        Self::CycleFocusForward,
        Self::CycleFocusBackward,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::ToggleShuffle,
        Self::CycleRepeat,
        Self::ToggleRadio,
        Self::MoveQueueItemUp,
        Self::MoveQueueItemDown,
        Self::ConnectAccount,
        Self::LoadMore,
        Self::RecheckDependencies,
        Self::ChooseCountry,
        Self::ToggleLyrics,
        Self::ToggleQueuePanel,
        Self::Cancel,
        Self::DeleteBackward,
        Self::Submit,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputAction {
    Semantic(SemanticAction),
    InsertCharacter(char),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PaletteEntry {
    pub action: SemanticAction,
    pub label: &'static str,
    pub shortcut: &'static str,
}

const PALETTE_ENTRIES: [PaletteEntry; SemanticAction::ALL.len()] = [
    PaletteEntry {
        action: SemanticAction::Quit,
        label: "Quit",
        shortcut: "q",
    },
    PaletteEntry {
        action: SemanticAction::ToggleHelp,
        label: "Toggle help",
        shortcut: "?",
    },
    PaletteEntry {
        action: SemanticAction::OpenSearch,
        label: "Open search",
        shortcut: "/",
    },
    PaletteEntry {
        action: SemanticAction::OpenPalette,
        label: "Open command palette",
        shortcut: ":",
    },
    PaletteEntry {
        action: SemanticAction::TogglePlayback,
        label: "Play or pause",
        shortcut: "Space / F8 / Media Play/Pause",
    },
    PaletteEntry {
        action: SemanticAction::NextTrack,
        label: "Next track",
        shortcut: "n / F9 / Media Next",
    },
    PaletteEntry {
        action: SemanticAction::PreviousTrack,
        label: "Previous track",
        shortcut: "p / F7 / Media Previous",
    },
    PaletteEntry {
        action: SemanticAction::ToggleFavorite,
        label: "Toggle favorite",
        shortcut: "f",
    },
    PaletteEntry {
        action: SemanticAction::SeekBackward,
        label: "Seek backward",
        shortcut: "Shift+Left",
    },
    PaletteEntry {
        action: SemanticAction::SeekForward,
        label: "Seek forward",
        shortcut: "Shift+Right",
    },
    PaletteEntry {
        action: SemanticAction::MoveUp,
        label: "Move up",
        shortcut: "↑ / k",
    },
    PaletteEntry {
        action: SemanticAction::MoveDown,
        label: "Move down",
        shortcut: "↓ / j",
    },
    PaletteEntry {
        action: SemanticAction::MoveLeft,
        label: "Move left",
        shortcut: "← / h",
    },
    PaletteEntry {
        action: SemanticAction::MoveRight,
        label: "Move right",
        shortcut: "→ / l",
    },
    PaletteEntry {
        action: SemanticAction::CycleFocusForward,
        label: "Focus next region",
        shortcut: "Tab",
    },
    PaletteEntry {
        action: SemanticAction::CycleFocusBackward,
        label: "Focus previous region",
        shortcut: "Shift+Tab",
    },
    PaletteEntry {
        action: SemanticAction::VolumeUp,
        label: "Volume up",
        shortcut: "+",
    },
    PaletteEntry {
        action: SemanticAction::VolumeDown,
        label: "Volume down",
        shortcut: "-",
    },
    PaletteEntry {
        action: SemanticAction::ToggleShuffle,
        label: "Toggle shuffle",
        shortcut: "s",
    },
    PaletteEntry {
        action: SemanticAction::CycleRepeat,
        label: "Cycle repeat",
        shortcut: "r",
    },
    PaletteEntry {
        action: SemanticAction::ToggleRadio,
        label: "Toggle endless radio",
        shortcut: "e",
    },
    PaletteEntry {
        action: SemanticAction::MoveQueueItemUp,
        label: "Move queue item up",
        shortcut: "[",
    },
    PaletteEntry {
        action: SemanticAction::MoveQueueItemDown,
        label: "Move queue item down",
        shortcut: "]",
    },
    PaletteEntry {
        action: SemanticAction::ConnectAccount,
        label: "Connect account",
        shortcut: "a",
    },
    PaletteEntry {
        action: SemanticAction::LoadMore,
        label: "Load more results",
        shortcut: "m",
    },
    PaletteEntry {
        action: SemanticAction::RecheckDependencies,
        label: "Recheck dependencies",
        shortcut: "d",
    },
    PaletteEntry {
        action: SemanticAction::ChooseCountry,
        label: "Choose country",
        shortcut: "c",
    },
    PaletteEntry {
        action: SemanticAction::ToggleLyrics,
        label: "Toggle lyrics",
        shortcut: "L",
    },
    PaletteEntry {
        action: SemanticAction::ToggleQueuePanel,
        label: "Toggle compact queue",
        shortcut: "Q",
    },
    PaletteEntry {
        action: SemanticAction::Cancel,
        label: "Close or cancel",
        shortcut: "Esc",
    },
    PaletteEntry {
        action: SemanticAction::DeleteBackward,
        label: "Delete previous character",
        shortcut: "Backspace",
    },
    PaletteEntry {
        action: SemanticAction::Submit,
        label: "Submit text",
        shortcut: "Enter",
    },
];

#[must_use]
pub const fn palette_entries() -> &'static [PaletteEntry] {
    &PALETTE_ENTRIES
}

#[must_use]
pub fn map_event(mode: InputMode, event: KeyEvent) -> Option<InputAction> {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if matches!(mode, InputMode::TextEntry(_)) {
        if let KeyCode::Char(character) = event.code
            && printable_modifiers(event.modifiers)
            && !character.is_control()
        {
            return Some(InputAction::InsertCharacter(character));
        }

        match event.code {
            KeyCode::F(7..=9)
            | KeyCode::Media(
                MediaKeyCode::TrackPrevious | MediaKeyCode::PlayPause | MediaKeyCode::TrackNext,
            )
            | KeyCode::Tab
                if event.modifiers.is_empty() =>
            {
                return None;
            }
            KeyCode::Left | KeyCode::Right if event.modifiers == KeyModifiers::SHIFT => {
                return None;
            }
            KeyCode::Backspace if event.modifiers.is_empty() => {
                return Some(semantic(SemanticAction::DeleteBackward));
            }
            KeyCode::Enter if event.modifiers.is_empty() => {
                return Some(semantic(SemanticAction::Submit));
            }
            KeyCode::Esc if event.modifiers.is_empty() => {
                return Some(semantic(SemanticAction::Cancel));
            }
            KeyCode::BackTab if event.modifiers == KeyModifiers::SHIFT => return None,
            _ => {}
        }
    }

    map_global(event)
}

#[must_use]
pub fn map_key_event(event: KeyEvent, mode: InputMode) -> Option<InputAction> {
    map_event(mode, event)
}

fn map_global(event: KeyEvent) -> Option<InputAction> {
    let no_modifiers = event.modifiers.is_empty();
    let symbol_modifiers = no_modifiers || event.modifiers == KeyModifiers::SHIFT;

    let action = match event.code {
        KeyCode::Char('c') if event.modifiers == KeyModifiers::CONTROL => SemanticAction::Quit,
        KeyCode::Char('q') if no_modifiers => SemanticAction::Quit,
        KeyCode::Char('?') if symbol_modifiers => SemanticAction::ToggleHelp,
        KeyCode::Char('/') if symbol_modifiers => SemanticAction::OpenSearch,
        KeyCode::Char(':') if symbol_modifiers => SemanticAction::OpenPalette,
        KeyCode::Char(' ') if no_modifiers => SemanticAction::TogglePlayback,
        KeyCode::Char('n') if no_modifiers => SemanticAction::NextTrack,
        KeyCode::Char('p') if no_modifiers => SemanticAction::PreviousTrack,
        KeyCode::Char('f') if no_modifiers => SemanticAction::ToggleFavorite,
        KeyCode::F(7) | KeyCode::Media(MediaKeyCode::TrackPrevious) if no_modifiers => {
            SemanticAction::PreviousTrack
        }
        KeyCode::F(8) | KeyCode::Media(MediaKeyCode::PlayPause) if no_modifiers => {
            SemanticAction::TogglePlayback
        }
        KeyCode::F(9) | KeyCode::Media(MediaKeyCode::TrackNext) if no_modifiers => {
            SemanticAction::NextTrack
        }
        KeyCode::Left if event.modifiers == KeyModifiers::SHIFT => SemanticAction::SeekBackward,
        KeyCode::Right if event.modifiers == KeyModifiers::SHIFT => SemanticAction::SeekForward,
        KeyCode::Up | KeyCode::Char('k') if no_modifiers => SemanticAction::MoveUp,
        KeyCode::Down | KeyCode::Char('j') if no_modifiers => SemanticAction::MoveDown,
        KeyCode::Left | KeyCode::Char('h') if no_modifiers => SemanticAction::MoveLeft,
        KeyCode::Right | KeyCode::Char('l') if no_modifiers => SemanticAction::MoveRight,
        KeyCode::Tab if no_modifiers => SemanticAction::CycleFocusForward,
        KeyCode::BackTab if event.modifiers == KeyModifiers::SHIFT => {
            SemanticAction::CycleFocusBackward
        }
        KeyCode::Char('+') if symbol_modifiers => SemanticAction::VolumeUp,
        KeyCode::Char('-') if no_modifiers => SemanticAction::VolumeDown,
        KeyCode::Char('s') if no_modifiers => SemanticAction::ToggleShuffle,
        KeyCode::Char('r') if no_modifiers => SemanticAction::CycleRepeat,
        KeyCode::Char('e') if no_modifiers => SemanticAction::ToggleRadio,
        KeyCode::Char('[') if no_modifiers => SemanticAction::MoveQueueItemUp,
        KeyCode::Char(']') if no_modifiers => SemanticAction::MoveQueueItemDown,
        KeyCode::Char('a') if no_modifiers => SemanticAction::ConnectAccount,
        KeyCode::Char('m') if no_modifiers => SemanticAction::LoadMore,
        KeyCode::Char('d') if no_modifiers => SemanticAction::RecheckDependencies,
        KeyCode::Char('c') if no_modifiers => SemanticAction::ChooseCountry,
        KeyCode::Char('L') if symbol_modifiers => SemanticAction::ToggleLyrics,
        KeyCode::Char('Q') if symbol_modifiers => SemanticAction::ToggleQueuePanel,
        KeyCode::Char('q') if event.modifiers == KeyModifiers::SHIFT => {
            SemanticAction::ToggleQueuePanel
        }
        KeyCode::Enter if no_modifiers => SemanticAction::Submit,
        KeyCode::Esc if no_modifiers => SemanticAction::Cancel,
        _ => return None,
    };

    Some(semantic(action))
}

const fn semantic(action: SemanticAction) -> InputAction {
    InputAction::Semantic(action)
}

fn printable_modifiers(modifiers: KeyModifiers) -> bool {
    [
        KeyModifiers::NONE,
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::ALT | KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
    ]
    .contains(&modifiers)
}
