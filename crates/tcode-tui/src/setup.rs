//! The terminal's half of provider setup: crossterm keys in, shared state
//! machine out.
//!
//! The decisions themselves live in `tcode_frontend::setup`, which draws
//! nothing and knows no terminal. All this module adds is the keyboard
//! mapping, so the wizard (`wizard.rs`) and the `/provider` overlay keep
//! feeding it the crossterm events they already read.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub use tcode_frontend::setup::{Key, Mark, Progress, Row, Setup, Tone, View};

/// Translate a crossterm key into the setup vocabulary. `None` means the event
/// is not input at all (a key *release*), which the caller reports as
/// `Progress::Stay` — feeding it in would double every keystroke.
pub fn key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    Some(match event.code {
        KeyCode::Esc => Key::Cancel,
        KeyCode::Char('c') if ctrl => Key::Cancel,
        // Every other chord is inert: a Ctrl+X must not be typed into an API
        // key field as a bare character.
        KeyCode::Char(_) if ctrl => Key::Other,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        _ => Key::Other,
    })
}

/// `setup.on_key(...)` for a crossterm event, releases handled.
pub fn on_key(setup: &mut Setup, event: KeyEvent) -> Progress {
    match key(event) {
        Some(key) => setup.on_key(key),
        None => Progress::Stay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
        key(KeyEvent::new(code, modifiers))
    }

    /// The one decision this module makes on its own: which chords reach the
    /// state machine as characters. Ctrl+C backs out, every other Ctrl chord is
    /// inert — an accidental Ctrl+Space must not toggle a provider, and a
    /// Ctrl+V (no bracketed paste) must not type a `v` into an API key.
    #[test]
    fn control_chords_never_become_characters() {
        assert_eq!(
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Key::Cancel)
        );
        assert_eq!(press(KeyCode::Esc, KeyModifiers::NONE), Some(Key::Cancel));
        assert_eq!(
            press(KeyCode::Char('v'), KeyModifiers::CONTROL),
            Some(Key::Other)
        );
        assert_eq!(
            press(KeyCode::Char(' '), KeyModifiers::CONTROL),
            Some(Key::Other)
        );
        assert_eq!(
            press(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(Key::Char(' '))
        );
        // Shift is how capitals arrive; it must stay a character.
        assert_eq!(
            press(KeyCode::Char('K'), KeyModifiers::SHIFT),
            Some(Key::Char('K'))
        );
    }

    /// A release is not input. Feeding it in would replay every keystroke.
    #[test]
    fn releases_are_not_input() {
        let mut event = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert_eq!(key(event), None);
        event.kind = KeyEventKind::Press;
        assert_eq!(key(event), Some(Key::Enter));
    }
}
