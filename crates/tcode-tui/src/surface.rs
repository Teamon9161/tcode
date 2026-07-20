//! The surface a frame is painted onto.
//!
//! Production paints to the terminal; a test paints to an in-memory buffer it
//! can read back. Both go through the identical `redraw` code — that is the
//! whole point, and it is why this is one enum rather than `App` becoming
//! generic over `Backend`: a type parameter would spread across every `impl
//! App` block in this crate for no gain, since only the leaf writes differ.
//!
//! `Backend` is not object safe (`draw` and `set_cursor_position` are generic),
//! so `Box<dyn Backend>` is unavailable and the variants are enumerated here.

use std::io::{self, Stdout};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub enum Surface {
    Live(CrosstermBackend<Stdout>),
    /// Renders into a buffer instead of a terminal, so a test can read the
    /// frame back as text. Present in release builds too: keeping the variants
    /// unconditional avoids a `cfg` arm on every delegating method below, and
    /// `TestBackend` costs a few hundred bytes of never-reached code.
    #[cfg_attr(not(test), allow(dead_code))]
    Test(TestBackend),
}

impl Surface {
    pub fn live() -> Self {
        Surface::Live(CrosstermBackend::new(std::io::stdout()))
    }

    /// The frame most recently painted, one string per row with trailing blanks
    /// trimmed. This is the "screenshot": what a reader would see on screen,
    /// minus colour.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<String> {
        let Surface::Test(backend) = self else {
            panic!("only a test surface can be read back");
        };
        let buffer = backend.buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The whole frame as one newline-joined string, for substring assertions.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.rows().join("\n")
    }
}

/// Every method forwards to the active backend and nothing else. Written as a
/// macro so adding a backend stays a one-line change per method rather than a
/// second body to keep in sync.
macro_rules! forward {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            Surface::Live(backend) => backend.$method($($arg),*),
            Surface::Test(backend) => backend.$method($($arg),*),
        }
    };
}

impl Backend for Surface {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        forward!(self, draw, content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        forward!(self, hide_cursor)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        forward!(self, show_cursor)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        forward!(self, get_cursor_position)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        forward!(self, set_cursor_position, position)
    }

    fn clear(&mut self) -> io::Result<()> {
        forward!(self, clear)
    }

    // Not defaulted through: the crossterm backend implements both natively,
    // and the trait's fallbacks would silently downgrade it.
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        forward!(self, clear_region, clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        forward!(self, append_lines, n)
    }

    fn size(&self) -> io::Result<Size> {
        forward!(self, size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        forward!(self, window_size)
    }

    fn flush(&mut self) -> io::Result<()> {
        forward!(self, flush)
    }
}
