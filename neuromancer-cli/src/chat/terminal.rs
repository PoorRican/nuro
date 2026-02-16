use std::io;

use crossterm::event::DisableBracketedPaste;
use crossterm::execute;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

pub(super) struct TerminalCleanup {
    pub(super) enabled: bool,
}

impl TerminalCleanup {
    pub(super) fn disable(&mut self) {
        if !self.enabled {
            return;
        }

        self.enabled = false;
        let _ = disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        self.disable();
    }
}
