//! One process-wide Ctrl-C handler, with a fresh cancellation token per run.

use agent_core::types::CancellationToken;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Interrupt {
    active: Arc<Mutex<Option<CancellationToken>>>,
}

impl Interrupt {
    /// Install once when the CLI starts. The first interrupt of an active run
    /// requests graceful cancellation; idle or repeated interrupts exit with 130.
    pub fn install() -> Result<Self, String> {
        let interrupt = Self {
            active: Arc::new(Mutex::new(None)),
        };
        let handler = interrupt.clone();
        ctrlc::set_handler(move || {
            if !handler.cancel_active() {
                std::process::exit(130);
            }
        })
        .map_err(|error| format!("Ctrl-C handler: {error}"))?;
        Ok(interrupt)
    }

    pub fn begin(&self) -> CancellationToken {
        let signal = CancellationToken::new();
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(signal.clone());
        signal
    }

    pub fn end(&self) {
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    /// `false` means the handler should perform the immediate exit transition.
    fn cancel_active(&self) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match active.as_ref() {
            Some(signal) if !signal.is_cancelled() => {
                signal.cancel();
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn fixture() -> Interrupt {
        Interrupt {
            active: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn idle_interrupt_selects_immediate_exit_without_installing_a_handler() {
        assert!(!fixture().cancel_active());
    }

    #[test]
    fn first_active_interrupt_cancels_and_repeated_interrupt_selects_exit() {
        let interrupt = fixture();
        let signal = interrupt.begin();
        assert!(!signal.is_cancelled());
        assert!(interrupt.cancel_active());
        assert!(signal.is_cancelled());
        assert!(!interrupt.cancel_active());
    }

    #[test]
    fn ending_a_run_clears_the_active_token() {
        let interrupt = fixture();
        let signal = interrupt.begin();
        interrupt.end();
        assert!(!interrupt.cancel_active());
        assert!(!signal.is_cancelled());
    }

    #[test]
    fn each_run_gets_an_independent_fresh_token() {
        let interrupt = fixture();
        let first = interrupt.begin();
        assert!(interrupt.cancel_active());
        interrupt.end();
        let second = interrupt.begin();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(interrupt.cancel_active());
        assert!(second.is_cancelled());
    }

    #[test]
    fn external_abort_makes_a_later_interrupt_select_exit() {
        let interrupt = fixture();
        let signal = interrupt.begin();
        signal.cancel();
        assert!(!interrupt.cancel_active());
    }

    #[test]
    fn clones_and_concurrent_handlers_share_one_active_transition() {
        let interrupt = fixture();
        let signal = interrupt.begin();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let handler = interrupt.clone();
                thread::spawn(move || handler.cancel_active())
            })
            .collect();
        let cancelled = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|cancelled| *cancelled)
            .count();
        assert_eq!(cancelled, 1);
        assert!(signal.is_cancelled());
        assert!(!interrupt.clone().cancel_active());
    }
}
