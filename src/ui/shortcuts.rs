//! The global keymap used by dispatch, the palette, help, and the README.

use crate::ui::app::{CassMsg, FocusDirection};
use crate::ui::data::InputMode;
use ftui::{KeyCode, KeyEvent, Modifiers};
use std::collections::HashSet;

/// An action and every supported global spelling of its shortcut.
/// The first spelling is safe on legacy terminals; enhanced aliases follow it.
pub struct Shortcut {
    pub keys: &'static [&'static str],
    pub description: &'static str,
    pub message: fn() -> CassMsg,
}

macro_rules! shortcut {
    ($description:literal, $message:expr, $($key:literal),+ $(,)?) => {
        Shortcut { keys: &[$($key),+], description: $description, message: || $message }
    };
}

pub static KEYMAP: &[Shortcut] = &[
    shortcut!("Force quit", CassMsg::ForceQuit, "Ctrl+C"),
    shortcut!(
        "Close modal or surface; otherwise quit",
        CassMsg::QuitRequested,
        "Esc",
        "F10"
    ),
    shortcut!("Toggle help", CassMsg::HelpToggled, "F1", "Alt+?"),
    shortcut!(
        "Next theme (19 presets)",
        CassMsg::ThemeToggled,
        "F2",
        "Alt+T"
    ),
    shortcut!(
        "Previous theme",
        CassMsg::ThemePreviousToggled,
        "Shift+F2",
        "Alt+Shift+T"
    ),
    shortcut!(
        "Open agent filter",
        CassMsg::InputModeEntered(InputMode::Agent),
        "F3",
        "Alt+G"
    ),
    shortcut!(
        "Clear agent filter",
        CassMsg::FilterAgentSet(HashSet::new()),
        "Shift+F3",
        "Alt+Shift+G"
    ),
    shortcut!(
        "Open workspace filter",
        CassMsg::InputModeEntered(InputMode::Workspace),
        "F4",
        "Alt+W"
    ),
    shortcut!(
        "Clear all filters",
        CassMsg::FiltersClearAll,
        "Shift+F4",
        "Alt+Shift+W",
        "Ctrl+Del"
    ),
    shortcut!(
        "Set from time",
        CassMsg::InputModeEntered(InputMode::CreatedFrom),
        "F5"
    ),
    shortcut!("Cycle time presets", CassMsg::TimePresetCycled, "Shift+F5"),
    shortcut!(
        "Set to time",
        CassMsg::InputModeEntered(InputMode::CreatedTo),
        "F6"
    ),
    shortcut!(
        "Cycle context size",
        CassMsg::ContextWindowCycled,
        "F7",
        "Alt+C"
    ),
    shortcut!(
        "Open selected result in editor",
        CassMsg::OpenInEditor,
        "F8",
        "Alt+O"
    ),
    shortcut!("Cycle match mode", CassMsg::MatchModeCycled, "F9"),
    shortcut!("Cycle source filter", CassMsg::SourceFilterCycled, "F11"),
    shortcut!(
        "Open source filter menu",
        CassMsg::SourceFilterMenuToggled,
        "Shift+F11"
    ),
    shortcut!("Cycle ranking", CassMsg::RankingModeCycled, "F12", "Alt+R"),
    shortcut!("Toggle stats bar", CassMsg::StatsBarToggled, "Ctrl+S"),
    shortcut!("Cycle search mode", CassMsg::SearchModeCycled, "Alt+S"),
    shortcut!("Open analytics", CassMsg::AnalyticsEntered, "Alt+A"),
    shortcut!(
        "Toggle macro recording",
        CassMsg::MacroRecordingToggled,
        "Alt+M"
    ),
    shortcut!(
        "Open command palette",
        CassMsg::PaletteOpened,
        "Ctrl+P",
        "Alt+P"
    ),
    shortcut!(
        "Open pane filter (find in detail)",
        CassMsg::PaneFilterOpened,
        "Alt+/"
    ),
    shortcut!("Copy snippet", CassMsg::CopySnippet, "Alt+Y"),
    shortcut!("View raw", CassMsg::ViewRaw, "Alt+V"),
    shortcut!("Toggle JSON view", CassMsg::ToggleJsonView, "Alt+Shift+J"),
    shortcut!("Open bulk actions", CassMsg::BulkActionsOpened, "Alt+B"),
    shortcut!(
        "Bookmark selected result",
        CassMsg::BookmarkSelectedHit,
        "Alt+E"
    ),
    shortcut!(
        "Cycle grouping",
        CassMsg::GroupingCycled,
        "Alt+F",
        "Alt+Shift+F"
    ),
    shortcut!(
        "Timeline backward",
        CassMsg::TimelineJumped { forward: false },
        "Alt+,"
    ),
    shortcut!(
        "Timeline forward",
        CassMsg::TimelineJumped { forward: true },
        "Alt+."
    ),
    shortcut!(
        "Refresh index",
        CassMsg::IndexRefreshRequested,
        "Alt+Shift+R",
        "Ctrl+Shift+R"
    ),
    shortcut!("Cycle query history", CassMsg::HistoryCycled, "Ctrl+R"),
    shortcut!(
        "Previous query in history",
        CassMsg::HistoryNavigated { forward: false },
        "Alt+Shift+N",
        "Ctrl+Shift+N"
    ),
    shortcut!(
        "Next query in history",
        CassMsg::HistoryNavigated { forward: true },
        "Ctrl+N"
    ),
    shortcut!(
        "Reset TUI state",
        CassMsg::StateResetRequested,
        "Ctrl+Shift+Del"
    ),
    shortcut!(
        "Upgrade (update banner)",
        CassMsg::UpdateUpgradeRequested,
        "Alt+U"
    ),
    shortcut!(
        "Release notes (update banner)",
        CassMsg::UpdateReleaseNotesRequested,
        "Alt+N"
    ),
    shortcut!(
        "Skip version (update banner)",
        CassMsg::UpdateSkipped,
        "Alt+I"
    ),
    shortcut!(
        "Open sources",
        CassMsg::SourcesEntered,
        "Alt+Shift+S",
        "Ctrl+Shift+S"
    ),
    shortcut!(
        "Toggle inspector",
        CassMsg::InspectorToggled,
        "Alt+Shift+I",
        "Ctrl+Shift+I"
    ),
    shortcut!("Toggle borders", CassMsg::BordersToggled, "Ctrl+B"),
    shortcut!("Redo", CassMsg::Redo, "Alt+Z", "Ctrl+Shift+Z"),
    shortcut!("Undo", CassMsg::Undo, "Ctrl+Z"),
    shortcut!("Clear query", CassMsg::QueryCleared, "Ctrl+L"),
    shortcut!(
        "Kill query to line start",
        CassMsg::QueryLineKilled,
        "Ctrl+U"
    ),
    shortcut!(
        "Kill query to line end",
        CassMsg::QueryForwardKilled,
        "Ctrl+K"
    ),
    shortcut!("Delete previous word", CassMsg::QueryWordDeleted, "Ctrl+W"),
    shortcut!(
        "Toggle wildcard fallback (find in detail)",
        CassMsg::WildcardFallbackToggled,
        "Ctrl+F"
    ),
    shortcut!("Cycle density", CassMsg::DensityModeCycled, "Ctrl+D"),
    shortcut!(
        "Toggle current selection",
        CassMsg::SelectionToggled,
        "Ctrl+X"
    ),
    shortcut!(
        "Select all visible results",
        CassMsg::SelectAllToggled,
        "Ctrl+A"
    ),
    shortcut!(
        "Open detail (submit query without a selected hit)",
        CassMsg::DetailOpened,
        "Enter",
        "Ctrl+M",
        "Ctrl+J"
    ),
    shortcut!(
        "Queue selected result",
        CassMsg::ItemEnqueued,
        "Alt+Enter",
        "Ctrl+Enter"
    ),
    shortcut!(
        "Open queued results in editor",
        CassMsg::OpenAllQueued,
        "Ctrl+O"
    ),
    shortcut!(
        "Export Markdown",
        CassMsg::ExportMarkdownExecuted,
        "Alt+Shift+E",
        "Ctrl+Shift+E"
    ),
    shortcut!("Open export modal", CassMsg::ExportModalOpened, "Ctrl+E"),
    shortcut!(
        "Copy query",
        CassMsg::CopyQuery,
        "Alt+Shift+Y",
        "Ctrl+Shift+Y"
    ),
    shortcut!("Copy selected path", CassMsg::CopyPath, "Ctrl+Y"),
    shortcut!(
        "Copy selected content",
        CassMsg::CopyContent,
        "Alt+Shift+C",
        "Ctrl+Shift+C"
    ),
    shortcut!(
        "Toggle XL peek",
        CassMsg::PeekToggled,
        "Alt+0",
        "Ctrl+Space"
    ),
    shortcut!(
        "Focus left",
        CassMsg::FocusDirectional {
            direction: FocusDirection::Left
        },
        "Shift+Tab",
        "Alt+H",
        "Alt+Left"
    ),
    shortcut!(
        "Toggle results/detail focus (cycle tabs in detail)",
        CassMsg::FocusToggled,
        "Tab"
    ),
    shortcut!(
        "Focus up",
        CassMsg::FocusDirectional {
            direction: FocusDirection::Up
        },
        "Alt+K",
        "Alt+Up"
    ),
    shortcut!(
        "Focus down",
        CassMsg::FocusDirectional {
            direction: FocusDirection::Down
        },
        "Alt+J",
        "Alt+Down"
    ),
    shortcut!(
        "Focus right",
        CassMsg::FocusDirectional {
            direction: FocusDirection::Right
        },
        "Alt+L",
        "Alt+Right"
    ),
    shortcut!(
        "Move cursor one word left",
        CassMsg::CursorWordMoved { forward: false },
        "Ctrl+Left"
    ),
    shortcut!(
        "Move cursor one word right",
        CassMsg::CursorWordMoved { forward: true },
        "Ctrl+Right"
    ),
    shortcut!(
        "Move cursor left",
        CassMsg::CursorMoved { delta: -1 },
        "Left"
    ),
    shortcut!(
        "Move cursor right",
        CassMsg::CursorMoved { delta: 1 },
        "Right"
    ),
    shortcut!("Delete next character", CassMsg::DeleteForward, "Del"),
    shortcut!(
        "Previous result (scroll up in detail)",
        CassMsg::SelectionMoved { delta: -1 },
        "Up"
    ),
    shortcut!(
        "Next result (scroll down in detail)",
        CassMsg::SelectionMoved { delta: 1 },
        "Down"
    ),
    shortcut!(
        "Query start (top in detail)",
        CassMsg::CursorJumped { to_end: false },
        "Home"
    ),
    shortcut!(
        "Query end (bottom in detail)",
        CassMsg::CursorJumped { to_end: true },
        "End"
    ),
    shortcut!(
        "Previous page",
        CassMsg::PageScrolled { delta: -1 },
        "PageUp"
    ),
    shortcut!("Next page", CassMsg::PageScrolled { delta: 1 }, "PageDown"),
    shortcut!("Shrink results pane", CassMsg::PaneShrunk, "Alt+-"),
    shortcut!(
        "Grow results pane",
        CassMsg::PaneGrew,
        "Alt+=",
        "Alt+Shift+="
    ),
    shortcut!("Toggle detail pane", CassMsg::DetailPaneToggled, "Alt+D"),
    shortcut!("Open saved views", CassMsg::SavedViewsOpened, "Alt+Shift+V"),
    shortcut!(
        "Save view slot 1",
        CassMsg::ViewSaved(1),
        "Ctrl+F1",
        "Ctrl+1"
    ),
    shortcut!(
        "Save view slot 2",
        CassMsg::ViewSaved(2),
        "Ctrl+F2",
        "Ctrl+2"
    ),
    shortcut!(
        "Save view slot 3",
        CassMsg::ViewSaved(3),
        "Ctrl+F3",
        "Ctrl+3"
    ),
    shortcut!(
        "Save view slot 4",
        CassMsg::ViewSaved(4),
        "Ctrl+F4",
        "Ctrl+4"
    ),
    shortcut!(
        "Save view slot 5",
        CassMsg::ViewSaved(5),
        "Ctrl+F5",
        "Ctrl+5"
    ),
    shortcut!(
        "Save view slot 6",
        CassMsg::ViewSaved(6),
        "Ctrl+F6",
        "Ctrl+6"
    ),
    shortcut!(
        "Save view slot 7",
        CassMsg::ViewSaved(7),
        "Ctrl+F7",
        "Ctrl+7"
    ),
    shortcut!(
        "Save view slot 8",
        CassMsg::ViewSaved(8),
        "Ctrl+F8",
        "Ctrl+8"
    ),
    shortcut!(
        "Save view slot 9",
        CassMsg::ViewSaved(9),
        "Ctrl+F9",
        "Ctrl+9"
    ),
    shortcut!("Load view slot 1", CassMsg::ViewLoaded(1), "Alt+F1"),
    shortcut!("Load view slot 2", CassMsg::ViewLoaded(2), "Alt+F2"),
    shortcut!("Load view slot 3", CassMsg::ViewLoaded(3), "Alt+F3"),
    shortcut!("Load view slot 4", CassMsg::ViewLoaded(4), "Alt+F4"),
    shortcut!("Load view slot 5", CassMsg::ViewLoaded(5), "Alt+F5"),
    shortcut!("Load view slot 6", CassMsg::ViewLoaded(6), "Alt+F6"),
    shortcut!("Load view slot 7", CassMsg::ViewLoaded(7), "Alt+F7"),
    shortcut!("Load view slot 8", CassMsg::ViewLoaded(8), "Alt+F8"),
    shortcut!("Load view slot 9", CassMsg::ViewLoaded(9), "Alt+F9"),
    shortcut!(
        "Switch to pane 1",
        CassMsg::ActivePaneChanged { index: 0 },
        "Alt+1"
    ),
    shortcut!(
        "Switch to pane 2",
        CassMsg::ActivePaneChanged { index: 1 },
        "Alt+2"
    ),
    shortcut!(
        "Switch to pane 3",
        CassMsg::ActivePaneChanged { index: 2 },
        "Alt+3"
    ),
    shortcut!(
        "Switch to pane 4",
        CassMsg::ActivePaneChanged { index: 3 },
        "Alt+4"
    ),
    shortcut!(
        "Switch to pane 5",
        CassMsg::ActivePaneChanged { index: 4 },
        "Alt+5"
    ),
    shortcut!(
        "Switch to pane 6",
        CassMsg::ActivePaneChanged { index: 5 },
        "Alt+6"
    ),
    shortcut!(
        "Switch to pane 7",
        CassMsg::ActivePaneChanged { index: 6 },
        "Alt+7"
    ),
    shortcut!(
        "Switch to pane 8",
        CassMsg::ActivePaneChanged { index: 7 },
        "Alt+8"
    ),
    shortcut!(
        "Switch to pane 9",
        CassMsg::ActivePaneChanged { index: 8 },
        "Alt+9"
    ),
];

pub static DETAIL_KEYMAP: &[Shortcut] = &[
    shortcut!("Start find in detail", CassMsg::PaneFilterOpened, "/"),
    shortcut!(
        "Next contextual search hit",
        CassMsg::DetailSessionHitNavigated { forward: true },
        "N"
    ),
    shortcut!(
        "Previous contextual search hit",
        CassMsg::DetailSessionHitNavigated { forward: false },
        "Shift+N"
    ),
    shortcut!(
        "Scroll down three lines",
        CassMsg::DetailScrolled { delta: 3 },
        "J"
    ),
    shortcut!(
        "Scroll up three lines",
        CassMsg::DetailScrolled { delta: -3 },
        "K"
    ),
    shortcut!(
        "Scroll to top",
        CassMsg::CursorJumped { to_end: false },
        "G"
    ),
    shortcut!(
        "Scroll to bottom",
        CassMsg::CursorJumped { to_end: true },
        "Shift+G"
    ),
    shortcut!("Toggle line wrap", CassMsg::DetailWrapToggled, "W"),
    shortcut!(
        "Expand tool and system messages",
        CassMsg::ToolExpandAll,
        "E"
    ),
    shortcut!(
        "Collapse tool and system messages",
        CassMsg::ToolCollapseAll,
        "C"
    ),
    shortcut!(
        "Previous message",
        CassMsg::DetailMessageJumped {
            forward: false,
            user_only: false
        },
        "{"
    ),
    shortcut!(
        "Next message",
        CassMsg::DetailMessageJumped {
            forward: true,
            user_only: false
        },
        "}"
    ),
    shortcut!(
        "Previous user message",
        CassMsg::DetailMessageJumped {
            forward: false,
            user_only: true
        },
        "["
    ),
    shortcut!(
        "Next user message",
        CassMsg::DetailMessageJumped {
            forward: true,
            user_only: true
        },
        "]"
    ),
];

pub static EXPORT_KEYMAP: &[Shortcut] = &[
    shortcut!(
        "Open HTML export modal",
        CassMsg::ExportModalOpened,
        "E",
        "H",
        "Shift+H"
    ),
    shortcut!(
        "Export Markdown",
        CassMsg::ExportMarkdownExecuted,
        "M",
        "Shift+M"
    ),
];

fn normalized_key(key: &KeyEvent) -> (KeyCode, Modifiers) {
    let mut modifiers = key.modifiers;
    let code = match key.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::BackTab => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Tab
        }
        KeyCode::Null => {
            modifiers.insert(Modifiers::CTRL);
            KeyCode::Char(' ')
        }
        KeyCode::Char('?') => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Char('/')
        }
        KeyCode::Char('+') => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Char('=')
        }
        KeyCode::Char('{') => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Char('[')
        }
        KeyCode::Char('}') => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Char(']')
        }
        code => code,
    };
    (code, modifiers)
}

fn parse_key(label: &str) -> Option<(KeyCode, Modifiers)> {
    let mut modifiers = Modifiers::empty();
    let mut parts = label.split('+').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            modifiers.insert(match part {
                "Ctrl" => Modifiers::CTRL,
                "Alt" => Modifiers::ALT,
                "Shift" => Modifiers::SHIFT,
                _ => return None,
            });
            continue;
        }
        let code = match part {
            "Esc" => KeyCode::Escape,
            "Enter" => KeyCode::Enter,
            "Space" => KeyCode::Char(' '),
            "Tab" => KeyCode::Tab,
            "?" => {
                modifiers.insert(Modifiers::SHIFT);
                KeyCode::Char('/')
            }
            "{" => {
                modifiers.insert(Modifiers::SHIFT);
                KeyCode::Char('[')
            }
            "}" => {
                modifiers.insert(Modifiers::SHIFT);
                KeyCode::Char(']')
            }
            "Del" => KeyCode::Delete,
            "Left" => KeyCode::Left,
            "Right" => KeyCode::Right,
            "Up" => KeyCode::Up,
            "Down" => KeyCode::Down,
            "Home" => KeyCode::Home,
            "End" => KeyCode::End,
            "PageUp" => KeyCode::PageUp,
            "PageDown" => KeyCode::PageDown,
            value if value.starts_with('F') && value.len() > 1 => {
                KeyCode::F(value.strip_prefix('F')?.parse().ok()?)
            }
            value if value.len() == 1 => KeyCode::Char(value.chars().next()?.to_ascii_lowercase()),
            _ => return None,
        };
        return Some((code, modifiers));
    }
    None
}

/// Resolve one terminal event without losing modifiers or aliasing Shift away.
pub fn message_for_key(key: &KeyEvent) -> Option<CassMsg> {
    message_from_map(KEYMAP, key)
}

fn message_from_map(map: &[Shortcut], key: &KeyEvent) -> Option<CassMsg> {
    let actual = normalized_key(key);
    map.iter().find_map(|entry| {
        entry
            .keys
            .iter()
            .any(|label| parse_key(label) == Some(actual))
            .then(|| (entry.message)())
    })
}

pub fn markdown_reference() -> String {
    markdown_map(KEYMAP)
}

pub fn markdown_detail_reference() -> String {
    markdown_map(DETAIL_KEYMAP)
}

pub fn markdown_export_reference() -> String {
    markdown_map(EXPORT_KEYMAP)
}

fn markdown_map(map: &[Shortcut]) -> String {
    let mut text = String::from("| Key | Action |\n|-----|--------|\n");
    for entry in map {
        text.push_str("| ");
        for (index, key) in entry.keys.iter().enumerate() {
            if index > 0 {
                text.push_str(" / ");
            }
            text.push('`');
            text.push_str(key);
            text.push('`');
        }
        text.push_str(" | ");
        text.push_str(entry.description);
        text.push_str(" |\n");
    }
    text
}

pub fn message_for_detail(key: &KeyEvent, export_tab: bool) -> Option<CassMsg> {
    if export_tab && let Some(message) = message_from_map(EXPORT_KEYMAP, key) {
        return Some(message);
    }
    message_from_map(DETAIL_KEYMAP, key)
}

pub fn message_for_shortcut(primary: &str) -> Option<CassMsg> {
    KEYMAP
        .iter()
        .find(|entry| entry.keys.first().is_some_and(|key| *key == primary))
        .map(|entry| (entry.message)())
}

/// Result of the bounded Kitty support query, including unrelated early input.
#[derive(Default)]
pub struct KeyboardProbe {
    pub supported: bool,
    pub pending: Vec<u8>,
}

/// Query a nonblocking terminal stream. An ignored query leaves legacy input
/// usable; unrelated bytes are replayed through the normal input parser.
pub fn query_keyboard_protocol(
    stream: &mut (impl std::io::Read + std::io::Write),
    timeout: std::time::Duration,
) -> KeyboardProbe {
    let mut result = KeyboardProbe::default();
    if stream
        .write_all(b"\x1b[?u")
        .and_then(|()| stream.flush())
        .is_err()
    {
        return result;
    }
    let started = std::time::Instant::now();
    // Stop exactly at the reply terminator. Reading past it could consume a
    // partial UTF-8 character or escape sequence belonging to runtime input.
    let mut buffer = [0_u8; 1];
    while started.elapsed() < timeout && result.pending.len() < 4096 {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let Some(bytes) = buffer.get(..n) else {
                    break;
                };
                result.pending.extend_from_slice(bytes);
                let reply = result
                    .pending
                    .windows(3)
                    .enumerate()
                    .find_map(|(start, prefix)| {
                        if prefix != b"\x1b[?" {
                            return None;
                        }
                        let digits = result
                            .pending
                            .get(start + 3..)?
                            .iter()
                            .take_while(|b| b.is_ascii_digit())
                            .count();
                        (digits > 0 && result.pending.get(start + 3 + digits) == Some(&b'u'))
                            .then_some(start..start + 4 + digits)
                    });
                if let Some(reply) = reply {
                    result.pending.drain(reply);
                    result.supported = true;
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    result
}

pub fn keyboard_query_enabled(
    value: Option<&str>,
    headless: bool,
    in_mux: bool,
) -> Result<bool, &'static str> {
    let requested = match value {
        None | Some("auto" | "1" | "true") => true,
        Some("0" | "false") => false,
        Some(_) => return Err("CASS_TUI_KITTY_KEYBOARD must be auto, 1, true, 0, or false"),
    };
    Ok(requested && !headless && !in_mux)
}

pub const HELP: &str = "F1";
pub const THEME: &str = "F2";
pub const FILTER_AGENT: &str = "F3";
pub const FILTER_WORKSPACE: &str = "F4";
pub const FILTER_DATE_FROM: &str = "F5";
pub const FILTER_DATE_TO: &str = "F6";
pub const CONTEXT_WINDOW: &str = "F7";
pub const EDITOR: &str = "F8";
pub const MATCH_MODE: &str = "F9";
pub const SEARCH_MODE: &str = "Alt+S";
pub const QUIT: &str = "Esc/F10";
pub const CLEAR_FILTERS: &str = "Ctrl+Del";
pub const RESET_STATE: &str = "Ctrl+Shift+Del";
pub const RANKING: &str = "F12";
pub const GROUPING: &str = "Alt+F";
pub const REFRESH: &str = "Alt+Shift+R";
pub const DETAIL_OPEN: &str = "Enter";
pub const DETAIL_CLOSE: &str = "Esc";
pub const FOCUS_QUERY: &str = "Tab/Shift+Tab";
pub const HISTORY_NEXT: &str = "Ctrl+n";
pub const HISTORY_PREV: &str = "Alt+Shift+N";
pub const HISTORY_CYCLE: &str = "Ctrl+R";

// Filter scopes
pub const SCOPE_AGENT: &str = "Shift+F3";
pub const SCOPE_WORKSPACE: &str = "Shift+F4";
pub const CYCLE_TIME_PRESETS: &str = "Shift+F5";

// Command palette
pub const PALETTE: &str = "Ctrl+P";
pub const DENSITY: &str = "Ctrl+D";
pub const BORDERS: &str = "Ctrl+B";
pub const STATS_BAR: &str = "Ctrl+S";

// Actions
pub const COPY: &str = "Alt+Y";
pub const COPY_PATH: &str = "Ctrl+Y";
pub const COPY_CONTENT: &str = "Alt+Shift+C";
pub const BOOKMARK: &str = "Alt+E";
pub const BULK_MENU: &str = "Alt+B";
pub const JSON_VIEW: &str = "Alt+Shift+J";
pub const TOGGLE_SELECT: &str = "Ctrl+X";
pub const PANE_FILTER: &str = "Alt+/";
pub const EXPORT_HTML: &str = "Ctrl+E";
pub const EXPORT_MARKDOWN: &str = "Alt+Shift+E";

// Find in detail
pub const DETAIL_FIND: &str = "/";
pub const DETAIL_FIND_NEXT: &str = "n";
pub const DETAIL_FIND_PREV: &str = "N";

// Theme cycling
pub const THEME_PREV: &str = "Shift+F2";

// Sources management
pub const SOURCES: &str = "Alt+Shift+S";

// Inspector
pub const INSPECTOR: &str = "Alt+Shift+I";

// Macro recording
pub const MACRO_TOGGLE: &str = "Alt+M";

// Surface navigation
pub const SURFACE_ANALYTICS: &str = "Alt+A";

// Navigation
pub const TAB_FOCUS: &str = "Tab";
pub const VIM_NAV: &str = "Alt+h/j/k/l";
pub const JUMP_TOP: &str = "Home";
pub const JUMP_BOTTOM: &str = "End";

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keymap_aliases_are_valid_and_unambiguous() {
        for map in [KEYMAP, DETAIL_KEYMAP, EXPORT_KEYMAP] {
            let mut seen = std::collections::HashMap::new();
            for entry in map {
                assert!(
                    !entry.keys.is_empty(),
                    "Every shortcut needs a primary key: {}",
                    entry.description
                );
                for key in entry.keys {
                    let parsed =
                        parse_key(key).unwrap_or_else(|| panic!("Invalid keymap label: {key}"));
                    let actual = format!("{parsed:?}");
                    assert!(
                        seen.insert(actual, *key).is_none(),
                        "Ambiguous keymap label: {key}"
                    );
                }
            }
        }
    }

    // =========================================================================
    // Function Key Constants Tests
    // =========================================================================

    #[test]
    fn function_key_shortcuts_are_not_empty() {
        assert!(!HELP.is_empty());
        assert!(!THEME.is_empty());
        assert!(!FILTER_AGENT.is_empty());
        assert!(!FILTER_WORKSPACE.is_empty());
        assert!(!FILTER_DATE_FROM.is_empty());
        assert!(!FILTER_DATE_TO.is_empty());
        assert!(!CONTEXT_WINDOW.is_empty());
        assert!(!EDITOR.is_empty());
        assert!(!MATCH_MODE.is_empty());
        assert!(!RANKING.is_empty());
    }

    #[test]
    fn function_key_shortcuts_have_expected_values() {
        assert_eq!(HELP, "F1");
        assert_eq!(THEME, "F2");
        assert_eq!(FILTER_AGENT, "F3");
        assert_eq!(FILTER_WORKSPACE, "F4");
        assert_eq!(FILTER_DATE_FROM, "F5");
        assert_eq!(FILTER_DATE_TO, "F6");
        assert_eq!(CONTEXT_WINDOW, "F7");
        assert_eq!(EDITOR, "F8");
        assert_eq!(MATCH_MODE, "F9");
        assert_eq!(RANKING, "F12");
    }

    // =========================================================================
    // Modifier Key Constants Tests
    // =========================================================================

    #[test]
    fn modifier_shortcuts_are_not_empty() {
        assert!(!SEARCH_MODE.is_empty());
        assert!(!SURFACE_ANALYTICS.is_empty());
        assert!(!CLEAR_FILTERS.is_empty());
        assert!(!RESET_STATE.is_empty());
        assert!(!REFRESH.is_empty());
        assert!(!HISTORY_NEXT.is_empty());
        assert!(!HISTORY_PREV.is_empty());
        assert!(!HISTORY_CYCLE.is_empty());
        assert!(!STATS_BAR.is_empty());
        assert!(!TOGGLE_SELECT.is_empty());
    }

    #[test]
    fn modifier_shortcuts_have_expected_values() {
        assert_eq!(SEARCH_MODE, "Alt+S");
        assert_eq!(SURFACE_ANALYTICS, "Alt+A");
        assert_eq!(CLEAR_FILTERS, "Ctrl+Del");
        assert_eq!(RESET_STATE, "Ctrl+Shift+Del");
        assert_eq!(REFRESH, "Alt+Shift+R");
        assert_eq!(HISTORY_NEXT, "Ctrl+n");
        assert_eq!(HISTORY_PREV, "Alt+Shift+N");
        assert_eq!(HISTORY_CYCLE, "Ctrl+R");
        assert_eq!(STATS_BAR, "Ctrl+S");
        assert_eq!(TOGGLE_SELECT, "Ctrl+X");
    }

    // =========================================================================
    // Scope Constants Tests
    // =========================================================================

    #[test]
    fn scope_shortcuts_are_not_empty() {
        assert!(!SCOPE_AGENT.is_empty());
        assert!(!SCOPE_WORKSPACE.is_empty());
        assert!(!CYCLE_TIME_PRESETS.is_empty());
    }

    #[test]
    fn scope_shortcuts_have_expected_values() {
        assert_eq!(SCOPE_AGENT, "Shift+F3");
        assert_eq!(SCOPE_WORKSPACE, "Shift+F4");
        assert_eq!(CYCLE_TIME_PRESETS, "Shift+F5");
    }

    // =========================================================================
    // Action Constants Tests
    // =========================================================================

    #[test]
    fn action_shortcuts_are_not_empty() {
        assert!(!COPY.is_empty());
        assert!(!BULK_MENU.is_empty());
        assert!(!PANE_FILTER.is_empty());
        assert!(!EXPORT_HTML.is_empty());
        assert!(!EXPORT_MARKDOWN.is_empty());
    }

    #[test]
    fn action_shortcuts_have_expected_values() {
        assert_eq!(COPY, "Alt+Y");
        assert_eq!(BOOKMARK, "Alt+E");
        assert_eq!(BULK_MENU, "Alt+B");
        assert_eq!(PANE_FILTER, "Alt+/");
        assert_eq!(EXPORT_HTML, "Ctrl+E");
        assert_eq!(EXPORT_MARKDOWN, "Alt+Shift+E");
    }

    // =========================================================================
    // Navigation Constants Tests
    // =========================================================================

    #[test]
    fn navigation_shortcuts_are_not_empty() {
        assert!(!TAB_FOCUS.is_empty());
        assert!(!VIM_NAV.is_empty());
        assert!(!JUMP_TOP.is_empty());
        assert!(!JUMP_BOTTOM.is_empty());
    }

    #[test]
    fn navigation_shortcuts_have_expected_values() {
        assert_eq!(TAB_FOCUS, "Tab");
        assert_eq!(VIM_NAV, "Alt+h/j/k/l");
        assert_eq!(JUMP_TOP, "Home");
        assert_eq!(JUMP_BOTTOM, "End");
    }

    // =========================================================================
    // Detail View Constants Tests
    // =========================================================================

    #[test]
    fn detail_shortcuts_are_not_empty() {
        assert!(!DETAIL_OPEN.is_empty());
        assert!(!DETAIL_CLOSE.is_empty());
        assert!(!FOCUS_QUERY.is_empty());
    }

    #[test]
    fn detail_shortcuts_have_expected_values() {
        assert_eq!(DETAIL_OPEN, "Enter");
        assert_eq!(DETAIL_CLOSE, "Esc");
        assert_eq!(FOCUS_QUERY, "Tab/Shift+Tab");
    }

    // =========================================================================
    // Quit Constants Tests
    // =========================================================================

    #[test]
    fn quit_shortcut_is_not_empty() {
        assert!(!QUIT.is_empty());
    }

    #[test]
    fn quit_shortcut_has_expected_value() {
        assert_eq!(QUIT, "Esc/F10");
    }

    // =========================================================================
    // Uniqueness Tests (Primary shortcuts should not conflict)
    // =========================================================================

    #[test]
    fn primary_function_keys_are_unique() {
        let mut seen = HashSet::new();
        let function_keys = [
            HELP,
            THEME,
            FILTER_AGENT,
            FILTER_WORKSPACE,
            FILTER_DATE_FROM,
            FILTER_DATE_TO,
            CONTEXT_WINDOW,
            EDITOR,
            MATCH_MODE,
            RANKING,
        ];

        for key in &function_keys {
            assert!(seen.insert(*key), "Duplicate function key found: {}", key);
        }
    }

    #[test]
    fn shift_function_keys_are_unique() {
        let mut seen = HashSet::new();
        let shift_keys = [THEME_PREV, SCOPE_AGENT, SCOPE_WORKSPACE, CYCLE_TIME_PRESETS];

        for key in &shift_keys {
            assert!(
                seen.insert(*key),
                "Duplicate shift+function key found: {}",
                key
            );
        }
    }

    #[test]
    fn ctrl_shortcuts_are_unique() {
        let mut seen = HashSet::new();
        let ctrl_keys = [
            CLEAR_FILTERS,
            RESET_STATE,
            REFRESH,
            HISTORY_NEXT,
            HISTORY_PREV,
            HISTORY_CYCLE,
            STATS_BAR,
            TOGGLE_SELECT,
        ];

        for key in &ctrl_keys {
            assert!(seen.insert(*key), "Duplicate ctrl shortcut found: {}", key);
        }
    }

    // =========================================================================
    // Format Validation Tests
    // =========================================================================

    #[test]
    fn function_key_format_is_valid() {
        // Function keys should start with "F" followed by a number
        let function_keys = [
            HELP,
            THEME,
            FILTER_AGENT,
            FILTER_WORKSPACE,
            FILTER_DATE_FROM,
            FILTER_DATE_TO,
            CONTEXT_WINDOW,
            EDITOR,
            MATCH_MODE,
            RANKING,
        ];

        for key in &function_keys {
            assert!(
                key.starts_with('F') && key[1..].chars().all(|c| c.is_ascii_digit()),
                "Invalid function key format: {}",
                key
            );
        }
    }

    #[test]
    fn shift_function_key_format_is_valid() {
        let shift_keys = [THEME_PREV, SCOPE_AGENT, SCOPE_WORKSPACE, CYCLE_TIME_PRESETS];

        for key in &shift_keys {
            assert!(
                key.starts_with("Shift+F"),
                "Shift key should start with 'Shift+F': {}",
                key
            );
        }
    }

    #[test]
    fn modifier_shortcuts_contain_plus_separator() {
        let modifier_keys = [
            SEARCH_MODE,
            SURFACE_ANALYTICS,
            CLEAR_FILTERS,
            RESET_STATE,
            REFRESH,
            HISTORY_NEXT,
            HISTORY_PREV,
            HISTORY_CYCLE,
            STATS_BAR,
            TOGGLE_SELECT,
        ];

        for key in &modifier_keys {
            assert!(
                key.contains('+'),
                "Modifier shortcut should contain '+': {}",
                key
            );
        }
    }
}
