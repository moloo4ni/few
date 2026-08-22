use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand as _;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};

const ENHANCE_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
    .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init() -> anyhow::Result<Tui> {
    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(EnableMouseCapture)?;
    let _ = out.execute(EnableBracketedPaste);
    let _ = out.execute(PushKeyboardEnhancementFlags(ENHANCE_FLAGS));
    let terminal = Terminal::new(CrosstermBackend::new(out))?;
    Ok(terminal)
}

fn setup_screen() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(EnableMouseCapture)?;
    let _ = out.execute(EnableBracketedPaste);
    let _ = out.execute(PushKeyboardEnhancementFlags(ENHANCE_FLAGS));
    Ok(())
}

fn teardown_screen() -> anyhow::Result<()> {
    disable_raw_mode()?;
    let mut out = stdout();
    let _ = out.execute(PopKeyboardEnhancementFlags);
    let _ = out.execute(DisableBracketedPaste);
    out.execute(DisableMouseCapture)?;
    out.execute(LeaveAlternateScreen)?;
    Ok(())
}

pub fn suspend() -> anyhow::Result<()> {
    teardown_screen()
}

pub fn resume() -> anyhow::Result<()> {
    setup_screen()
}

pub fn restore(mut terminal: Tui) {
    let _ = terminal.show_cursor();
    let _ = teardown_screen();
}
