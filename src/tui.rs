use std::fs::{File, OpenOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use color_eyre::eyre::{Context, Result};
use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// Run a TUI session against /dev/tty rather than stdout, so wrappers like the
/// shell-init function (`target="$(choros work …)"`) can capture the result
/// path on stdout without swallowing the terminal rendering.
pub fn with_tui<T>(
    f: impl FnOnce(&mut Terminal<CrosstermBackend<File>>) -> Result<T>,
) -> Result<T> {
    // The panic hook (color_eyre's report printer) runs *before* unwinding
    // reaches RestoreGuard, i.e. while the alternate screen is still up — the
    // report would be printed there and then discarded by LeaveAlternateScreen.
    // Restore first so the report lands on the real screen.
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_ACTIVE.load(Ordering::SeqCst) {
                restore_terminal();
            }
            prev(info);
        }));
    });

    let mut tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .wrap_err("opening /dev/tty for TUI rendering")?;
    enable_raw_mode()?;
    TUI_ACTIVE.store(true, Ordering::SeqCst);
    let _guard = RestoreGuard;
    execute!(tty, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(tty))?;
    f(&mut terminal)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    // Fresh handle: the Terminal owning the original one may still be alive.
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        let _ = execute!(tty, LeaveAlternateScreen, Show);
    }
}

/// Restores the terminal on drop — the normal-exit and error paths; panics are
/// handled earlier by the hook above (restoring twice is harmless).
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
        TUI_ACTIVE.store(false, Ordering::SeqCst);
    }
}
