//! Terminal initialisation and teardown.
//!
//! Wraps crossterm raw-mode setup and the ratatui [`Terminal`] type.
//! [`init`] switches the terminal into raw mode and enables mouse capture;
//! [`restore`] undoes that so the shell returns to its normal state when rundev exits.
//! A panic hook ensures the terminal is always restored even on unexpected crashes.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::panic;

pub type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Guard that restores the terminal on drop — catches panics, early returns,
/// and normal exits. Only signals (SIGKILL) can bypass this.
pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore();
    }
}

pub fn init() -> Result<Tui> {
    // Install panic hook so terminal is restored on panic
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore();
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    // Belt-and-suspenders: explicitly reset all mouse tracking modes.
    // crossterm's DisableMouseCapture may not cover SGR extended mode
    // on all terminals, leaving the shell flooded with escape sequences.
    print!("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
    let _ = io::Write::flush(&mut io::stdout());
    Ok(())
}
