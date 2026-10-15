//! Terminal ownership is RAII: every partially completed setup is compensated.
use crate::model::TuiModel;
use crate::render::render;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::{self, Stdout, Write};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalMode {
    #[default]
    Regular,
    Fullscreen,
}

#[derive(Default)]
struct TerminalGuard {
    raw: bool,
    alternate: bool,
    paste: bool,
    mouse: bool,
}
impl TerminalGuard {
    fn restore(&mut self) {
        if !self.raw && !self.alternate && !self.paste && !self.mouse {
            return;
        }
        let mut output = io::stdout();
        if self.mouse {
            let _ = execute!(output, DisableMouseCapture);
            self.mouse = false;
        }
        if self.paste {
            let _ = execute!(output, DisableBracketedPaste);
            self.paste = false;
        }
        let _ = execute!(output, Show);
        if self.alternate {
            let _ = execute!(output, LeaveAlternateScreen);
            self.alternate = false;
        }
        if self.raw {
            let _ = disable_raw_mode();
            self.raw = false;
        }
        let _ = output.flush();
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub struct NativeTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    guard: TerminalGuard,
    mode: TerminalMode,
}
impl NativeTerminal {
    pub fn open(mode: TerminalMode) -> io::Result<Self> {
        let mut guard = TerminalGuard::default();
        enable_raw_mode()?;
        guard.raw = true;
        let mut output = io::stdout();
        if mode == TerminalMode::Fullscreen {
            guard.alternate = true;
            execute!(output, EnterAlternateScreen)?;
        }
        guard.paste = true;
        execute!(output, EnableBracketedPaste)?;
        guard.mouse = true;
        execute!(output, EnableMouseCapture)?;
        let viewport = if mode == TerminalMode::Fullscreen {
            Viewport::Fullscreen
        } else {
            Viewport::Inline(24)
        };
        let terminal =
            Terminal::with_options(CrosstermBackend::new(output), TerminalOptions { viewport })?;
        Ok(Self {
            terminal,
            guard,
            mode,
        })
    }
    pub fn draw(&mut self, model: &mut TuiModel) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, model)).map(|_| ())
    }
    pub fn next_event(timeout: Duration) -> io::Result<Option<Event>> {
        if event::poll(timeout)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn restore(&mut self) {
        if !self.guard.raw {
            return;
        }
        if self.mode == TerminalMode::Regular {
            let area = self.terminal.get_frame().area();
            let _ = self
                .terminal
                .set_cursor_position((0, area.y.saturating_add(area.height.saturating_sub(1))));
            let _ = self.terminal.backend_mut().write_all(b"\r\n");
        }
        let _ = self.terminal.show_cursor();
        self.guard.restore();
    }
}
impl Drop for NativeTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Lazily accessed only in response to an explicit copy/paste shortcut.
#[derive(Default)]
pub struct NativeClipboard(Option<arboard::Clipboard>);
impl NativeClipboard {
    fn get(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.0.is_none() {
            self.0 = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        Ok(self.0.as_mut().unwrap())
    }
    pub fn copy(&mut self, text: &str) -> Result<(), String> {
        self.get()?
            .set_text(text)
            .map_err(|error| error.to_string())
    }
    pub fn paste(&mut self) -> Result<String, String> {
        self.get()?.get_text().map_err(|error| error.to_string())
    }
}
