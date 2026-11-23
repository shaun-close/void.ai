//! Keeps a fast printer's output from piling up between a run and the UI. Output waiting to
//! be read by the UI (in the engine's queue or the event channel) is counted; past
//! [`QUEUED_MAX`] a run's output is held instead, only its last [`HELD_MAX`] bytes kept, and
//! sent (after a note saying how much was skipped) once there's room again, or before the
//! run's next step or its end so nothing is reordered.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::runner::run::RunUpdate;

/// Run output waiting for the UI, across all runs, before more is held back.
pub const QUEUED_MAX: usize = 4 * 1024 * 1024;
/// How much of a held-back run's newest output is kept.
pub const HELD_MAX: usize = 64 * 1024;

/// Bytes of run output sent towards the UI and not yet received by it.
#[derive(Debug, Default)]
pub struct OutputBudget {
    queued: AtomicUsize,
}

impl OutputBudget {
    /// Count `n` more bytes if they fit.
    fn try_take(&self, n: usize) -> bool {
        self.queued
            .try_update(Ordering::AcqRel, Ordering::Acquire, |q| {
                (q + n <= QUEUED_MAX).then_some(q + n)
            })
            .is_ok()
    }

    fn force_take(&self, n: usize) {
        self.queued.fetch_add(n, Ordering::AcqRel);
    }

    /// `n` bytes reached the UI (or were dropped on the way).
    pub fn release(&self, n: usize) {
        let _ = self
            .queued
            .try_update(Ordering::AcqRel, Ordering::Acquire, |q| {
                Some(q.saturating_sub(n))
            });
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct Held {
    text: String,
    /// Bytes dropped from the front of `text`.
    skipped: usize,
}

impl Held {
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.skipped == 0
    }

    fn push(&mut self, more: &str) {
        self.text.push_str(more);
        if self.text.len() <= HELD_MAX {
            return;
        }
        // Keep the newest HELD_MAX bytes, from the start of a line when one is near.
        let mut cut = self.text.len() - HELD_MAX;
        while !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        if let Some(nl) = self.text[cut..].find('\n')
            && nl < HELD_MAX / 2
        {
            cut += nl + 1;
        }
        self.text.drain(..cut);
        self.skipped += cut;
    }

    /// What to send: a note about what was skipped (if anything was), then the kept text.
    fn take(&mut self) -> String {
        let held = std::mem::take(self);
        if held.skipped == 0 {
            return held.text;
        }
        format!(
            "\x1b[0m\n[void: {} of output skipped: it came faster than it could be shown]\n{}",
            fmt_bytes(held.skipped),
            held.text
        )
    }
}

fn fmt_bytes(n: usize) -> String {
    const MB: usize = 1024 * 1024;
    match n {
        n if n >= MB => format!("{:.1} MB", n as f64 / MB as f64),
        n if n >= 1024 => format!("{} KB", n / 1024),
        n => format!("{n} bytes"),
    }
}

/// One run's gate in front of the engine's queue (its updates arrive one at a time).
pub(super) struct Gate {
    budget: std::sync::Arc<OutputBudget>,
    held: Mutex<Held>,
}

impl Gate {
    pub(super) fn new(budget: std::sync::Arc<OutputBudget>) -> Self {
        Self {
            budget,
            held: Mutex::new(Held::default()),
        }
    }

    /// Pass `update` on through `send`, holding output back while the UI is behind.
    pub(super) fn pass(&self, update: RunUpdate, mut send: impl FnMut(RunUpdate)) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        match update {
            RunUpdate::Output(text) => {
                if held.is_empty() && self.budget.try_take(text.len()) {
                    send(RunUpdate::Output(text));
                    return;
                }
                held.push(&text);
                // Room again? (The note is at most ~100 bytes; the estimate is corrected.)
                let estimate = held.text.len() + 128;
                if self.budget.try_take(estimate) {
                    let out = held.take();
                    self.budget.release(estimate - out.len().min(estimate));
                    if out.len() > estimate {
                        self.budget.force_take(out.len() - estimate);
                    }
                    send(RunUpdate::Output(out));
                }
            }
            other => {
                // A step header or the end comes after everything printed before it.
                if !held.is_empty() {
                    let out = held.take();
                    self.budget.force_take(out.len());
                    send(RunUpdate::Output(out));
                }
                send(other);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn outputs(v: &[RunUpdate]) -> Vec<&str> {
        v.iter()
            .filter_map(|u| match u {
                RunUpdate::Output(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn holds_the_tail_while_full_and_keeps_order() {
        let budget = Arc::new(OutputBudget::default());
        let gate = Gate::new(budget.clone());
        let mut sent = Vec::new();
        let chunk = format!("{}\n", "x".repeat(64 * 1024 - 1));
        // Far more than fits, nobody reading.
        for _ in 0..200 {
            gate.pass(RunUpdate::Output(chunk.clone()), |u| sent.push(u));
        }
        gate.pass(RunUpdate::Output("the last line\n".into()), |u| {
            sent.push(u)
        });
        assert!(budget.queued() <= QUEUED_MAX, "{}", budget.queued());
        let n = sent.len();
        assert_eq!(n, QUEUED_MAX / chunk.len(), "only what fits went");
        gate.pass(
            RunUpdate::Finished {
                ok: true,
                failed_step: None,
                exit_code: Some(0),
                stopped: false,
                error: None,
            },
            |u| sent.push(u),
        );
        // The end flushed the held tail first: a note, then the newest output.
        assert_eq!(sent.len(), n + 2);
        let RunUpdate::Output(tail) = &sent[n] else {
            panic!("held output before the end")
        };
        assert!(tail.contains("skipped") && tail.ends_with("the last line\n"));
        assert!(tail.len() <= HELD_MAX + 200);
        assert!(matches!(sent[n + 1], RunUpdate::Finished { .. }));
        assert!(budget.queued() <= QUEUED_MAX + HELD_MAX + 200);
    }

    #[test]
    fn room_again_sends_the_held_output() {
        let budget = Arc::new(OutputBudget::default());
        let gate = Gate::new(budget.clone());
        let mut sent = Vec::new();
        budget.force_take(QUEUED_MAX);
        gate.pass(RunUpdate::Output("held\n".into()), |u| sent.push(u));
        assert!(sent.is_empty());
        // The UI read everything.
        budget.release(QUEUED_MAX);
        gate.pass(RunUpdate::Output("next\n".into()), |u| sent.push(u));
        assert_eq!(outputs(&sent), ["held\nnext\n"], "nothing skipped, no note");
        assert_eq!(budget.queued(), "held\nnext\n".len());
    }
}
