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
use std::io::{self, stdout, Stdout};
use std::sync::{Mutex, MutexGuard};

const ENHANCE_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
    .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

// Editor handoff runs on a blocking thread, so the terminal lifecycle cannot be
// owned by the main-loop stack alone. Keep exact feature state to balance push/pop
// commands and retry only cleanup operations that actually failed.
static SCREEN_STATE: Mutex<ScreenState> = Mutex::new(ScreenState::inactive());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenOperation {
    EnableRaw,
    EnterAlternate,
    EnableMouse,
    EnableBracketedPaste,
    PushKeyboardEnhancements,
    PopKeyboardEnhancements,
    DisableBracketedPaste,
    DisableMouse,
    LeaveAlternate,
    DisableRaw,
}

trait ScreenOps {
    fn run(&mut self, operation: ScreenOperation) -> io::Result<()>;
}

struct CrosstermOps;

impl ScreenOps for CrosstermOps {
    fn run(&mut self, operation: ScreenOperation) -> io::Result<()> {
        match operation {
            ScreenOperation::EnableRaw => enable_raw_mode(),
            ScreenOperation::EnterAlternate => stdout().execute(EnterAlternateScreen).map(|_| ()),
            ScreenOperation::EnableMouse => stdout().execute(EnableMouseCapture).map(|_| ()),
            ScreenOperation::EnableBracketedPaste => {
                stdout().execute(EnableBracketedPaste).map(|_| ())
            }
            ScreenOperation::PushKeyboardEnhancements => stdout()
                .execute(PushKeyboardEnhancementFlags(ENHANCE_FLAGS))
                .map(|_| ()),
            ScreenOperation::PopKeyboardEnhancements => {
                stdout().execute(PopKeyboardEnhancementFlags).map(|_| ())
            }
            ScreenOperation::DisableBracketedPaste => {
                stdout().execute(DisableBracketedPaste).map(|_| ())
            }
            ScreenOperation::DisableMouse => stdout().execute(DisableMouseCapture).map(|_| ()),
            ScreenOperation::LeaveAlternate => stdout().execute(LeaveAlternateScreen).map(|_| ()),
            ScreenOperation::DisableRaw => disable_raw_mode(),
        }
    }
}

#[derive(Clone, Copy)]
struct Feature {
    enable: ScreenOperation,
    disable: ScreenOperation,
    mask: u8,
    required: bool,
}

const FEATURES: [Feature; 5] = [
    Feature {
        enable: ScreenOperation::EnableRaw,
        disable: ScreenOperation::DisableRaw,
        mask: 1 << 0,
        required: true,
    },
    Feature {
        enable: ScreenOperation::EnterAlternate,
        disable: ScreenOperation::LeaveAlternate,
        mask: 1 << 1,
        required: true,
    },
    Feature {
        enable: ScreenOperation::EnableMouse,
        disable: ScreenOperation::DisableMouse,
        mask: 1 << 2,
        required: true,
    },
    Feature {
        enable: ScreenOperation::EnableBracketedPaste,
        disable: ScreenOperation::DisableBracketedPaste,
        mask: 1 << 3,
        required: false,
    },
    Feature {
        enable: ScreenOperation::PushKeyboardEnhancements,
        disable: ScreenOperation::PopKeyboardEnhancements,
        mask: 1 << 4,
        required: false,
    },
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScreenState(u8);

impl ScreenState {
    const fn inactive() -> Self {
        Self(0)
    }

    #[cfg(test)]
    fn active() -> Self {
        Self(
            FEATURES
                .iter()
                .fold(0, |state, feature| state | feature.mask),
        )
    }

    fn contains(self, feature: Feature) -> bool {
        self.0 & feature.mask != 0
    }

    fn teardown(&mut self, selected: Self, ops: &mut impl ScreenOps) -> io::Result<()> {
        let mut first_error = None;
        for feature in FEATURES.iter().rev().copied() {
            if !selected.contains(feature) || !self.contains(feature) {
                continue;
            }
            match ops.run(feature.disable) {
                Ok(()) => self.0 &= !feature.mask,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn setup_screen_with(
    ops: &mut impl ScreenOps,
    active: &mut ScreenState,
) -> io::Result<ScreenState> {
    let mut enabled = ScreenState::default();
    for feature in FEATURES {
        if active.contains(feature) {
            continue;
        }
        match ops.run(feature.enable) {
            Ok(()) => {
                active.0 |= feature.mask;
                enabled.0 |= feature.mask;
            }
            Err(error) if feature.required => {
                let _ = active.teardown(enabled, ops);
                return Err(error);
            }
            Err(_) => {}
        }
    }
    Ok(enabled)
}

fn initialize_screen<T>(
    ops: &mut impl ScreenOps,
    active: &mut ScreenState,
    create: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let enabled = setup_screen_with(ops, active)?;
    match create() {
        Ok(value) => Ok(value),
        Err(error) => {
            let _ = active.teardown(enabled, ops);
            Err(error)
        }
    }
}

fn screen_state() -> MutexGuard<'static, ScreenState> {
    SCREEN_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn init() -> anyhow::Result<Tui> {
    let mut ops = CrosstermOps;
    initialize_screen(&mut ops, &mut screen_state(), || {
        Terminal::new(CrosstermBackend::new(stdout()))
    })
    .map_err(Into::into)
}

fn setup_screen() -> anyhow::Result<()> {
    let mut ops = CrosstermOps;
    initialize_screen(&mut ops, &mut screen_state(), || Ok(())).map_err(Into::into)
}

fn teardown_screen() -> anyhow::Result<()> {
    let mut active = screen_state();
    let selected = *active;
    active
        .teardown(selected, &mut CrosstermOps)
        .map_err(Into::into)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeOps {
        calls: Vec<ScreenOperation>,
        failures: Vec<ScreenOperation>,
    }

    impl FakeOps {
        fn failing(operation: ScreenOperation) -> Self {
            Self {
                failures: vec![operation],
                ..Self::default()
            }
        }
    }

    impl ScreenOps for FakeOps {
        fn run(&mut self, operation: ScreenOperation) -> io::Result<()> {
            self.calls.push(operation);
            if self.failures.contains(&operation) {
                Err(io::Error::other(format!("{operation:?} failed")))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn partial_required_setup_is_rolled_back_in_reverse_order() {
        let mut alternate_failure = FakeOps::failing(ScreenOperation::EnterAlternate);
        let mut active = ScreenState::default();
        assert!(setup_screen_with(&mut alternate_failure, &mut active).is_err());
        assert_eq!(
            alternate_failure.calls,
            vec![
                ScreenOperation::EnableRaw,
                ScreenOperation::EnterAlternate,
                ScreenOperation::DisableRaw,
            ]
        );

        let mut mouse_failure = FakeOps::failing(ScreenOperation::EnableMouse);
        let mut active = ScreenState::default();
        assert!(setup_screen_with(&mut mouse_failure, &mut active).is_err());
        assert_eq!(
            mouse_failure.calls,
            vec![
                ScreenOperation::EnableRaw,
                ScreenOperation::EnterAlternate,
                ScreenOperation::EnableMouse,
                ScreenOperation::LeaveAlternate,
                ScreenOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn setup_keeps_primary_error_when_rollback_also_fails() {
        let mut ops = FakeOps {
            failures: vec![
                ScreenOperation::EnableMouse,
                ScreenOperation::LeaveAlternate,
                ScreenOperation::DisableRaw,
            ],
            ..FakeOps::default()
        };
        let mut active = ScreenState::default();

        let error = setup_screen_with(&mut ops, &mut active).unwrap_err();

        assert_eq!(error.to_string(), "EnableMouse failed");
        assert_eq!(
            ops.calls,
            vec![
                ScreenOperation::EnableRaw,
                ScreenOperation::EnterAlternate,
                ScreenOperation::EnableMouse,
                ScreenOperation::LeaveAlternate,
                ScreenOperation::DisableRaw,
            ]
        );
        assert_eq!(active, ScreenState(FEATURES[0].mask | FEATURES[1].mask));
    }

    #[test]
    fn terminal_creation_failure_rolls_back_only_enabled_features() {
        let mut ops = FakeOps::failing(ScreenOperation::EnableBracketedPaste);
        let mut active = ScreenState::default();
        let error = initialize_screen(&mut ops, &mut active, || {
            Err::<(), _>(io::Error::other("terminal creation failed"))
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "terminal creation failed");
        assert_eq!(active, ScreenState::default());
        assert_eq!(
            ops.calls,
            vec![
                ScreenOperation::EnableRaw,
                ScreenOperation::EnterAlternate,
                ScreenOperation::EnableMouse,
                ScreenOperation::EnableBracketedPaste,
                ScreenOperation::PushKeyboardEnhancements,
                ScreenOperation::PopKeyboardEnhancements,
                ScreenOperation::DisableMouse,
                ScreenOperation::LeaveAlternate,
                ScreenOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn teardown_attempts_every_operation_and_returns_the_first_error() {
        let mut ops = FakeOps {
            failures: vec![
                ScreenOperation::PopKeyboardEnhancements,
                ScreenOperation::DisableMouse,
            ],
            ..FakeOps::default()
        };

        let mut active = ScreenState::active();
        let selected = active;
        let error = active.teardown(selected, &mut ops).unwrap_err();
        assert_eq!(error.to_string(), "PopKeyboardEnhancements failed");
        assert_eq!(
            ops.calls,
            vec![
                ScreenOperation::PopKeyboardEnhancements,
                ScreenOperation::DisableBracketedPaste,
                ScreenOperation::DisableMouse,
                ScreenOperation::LeaveAlternate,
                ScreenOperation::DisableRaw,
            ]
        );
        assert_eq!(active, ScreenState(FEATURES[2].mask | FEATURES[4].mask));

        ops.calls.clear();
        ops.failures.clear();
        let selected = active;
        active.teardown(selected, &mut ops).unwrap();
        assert_eq!(
            ops.calls,
            vec![
                ScreenOperation::PopKeyboardEnhancements,
                ScreenOperation::DisableMouse,
            ]
        );
        assert_eq!(active, ScreenState::default());

        ops.calls.clear();
        active.teardown(active, &mut ops).unwrap();
        assert!(ops.calls.is_empty());
    }
}
