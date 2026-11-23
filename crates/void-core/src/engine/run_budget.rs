//! Keeps a fast printer's output from piling up between a run and the UI. Output waiting to
//! be read by the UI (in the engine's queue or the event channel) is counted; past
//! [`QUEUED_MAX`] a run's output is held instead, only its last [`HELD_MAX`] bytes kept, and
//! sent (after a note saying how much was skipped) once there's room again, or before the
//! run's next step or its end so nothing is reordered. "Room again" doesn't wait for the run
//! to print more: while any run holds output, the UI reading some wakes the engine, which
//! flushes every held run (see [`OutputBudget::set_waker`]).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::runner::run::RunUpdate;

/// Run output waiting for the UI, across all runs, before more is held back.
pub const QUEUED_MAX: usize = 4 * 1024 * 1024;
/// How much of a held-back run's newest output is kept.
pub const HELD_MAX: usize = 64 * 1024;

type Waker = Box<dyn Fn() + Send + Sync>;

/// Bytes of run output sent towards the UI and not yet received by it.
#[derive(Default)]
pub struct OutputBudget {
    queued: AtomicUsize,
    /// Gates holding output back.
    holding: AtomicUsize,
    /// A wake was sent and the engine hasn't handled it yet.
    wake_pending: AtomicBool,
    waker: OnceLock<Waker>,
}

impl OutputBudget {
    /// What to call (once per wake, without blocking) when output reached the UI while some
    /// run holds output back: the engine then calls [`Gate::flush`] on every run's gate.
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.waker.set(Box::new(f));
    }

    /// The engine is handling the wake: the next release may wake it again.
    pub(super) fn woken(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }

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
        if self.holding.load(Ordering::Acquire) > 0
            && !self.wake_pending.swap(true, Ordering::AcqRel)
        {
            match self.waker.get() {
                Some(wake) => wake(),
                None => self.wake_pending.store(false, Ordering::Release),
            }
        }
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for OutputBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputBudget")
            .field("queued", &self.queued())
            .field("holding", &self.holding.load(Ordering::Acquire))
            .finish()
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

/// One run's gate in front of the engine's queue. Every send happens under its lock, so a
/// run's updates reach the queue in order whoever (the run, or the engine on a wake) sends.
pub(super) struct Gate {
    budget: std::sync::Arc<OutputBudget>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    held: Held,
    /// Counted in the budget's `holding`.
    counted: bool,
}

impl Gate {
    pub(super) fn new(budget: std::sync::Arc<OutputBudget>) -> Self {
        Self {
            budget,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Keep the budget's count of holding gates in step with `inner`.
    fn sync(&self, inner: &mut Inner) {
        let holding = !inner.held.is_empty();
        if holding != inner.counted {
            if holding {
                self.budget.holding.fetch_add(1, Ordering::AcqRel);
            } else {
                self.budget.holding.fetch_sub(1, Ordering::AcqRel);
            }
            inner.counted = holding;
        }
    }

    /// Pass `update` on through `send`, holding output back while the UI is behind.
    pub(super) fn pass(&self, update: RunUpdate, mut send: impl FnMut(RunUpdate)) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match update {
            RunUpdate::Output(text) => {
                if inner.held.is_empty() && self.budget.try_take(text.len()) {
                    send(RunUpdate::Output(text));
                    return;
                }
                inner.held.push(&text);
                // Counted before the retry: a release from now on wakes the engine.
                self.sync(&mut inner);
                // Room again? (The note is at most ~100 bytes; the estimate is corrected.)
                let estimate = inner.held.text.len() + 128;
                if self.budget.try_take(estimate) {
                    let out = inner.held.take();
                    self.sync(&mut inner);
                    self.budget.release(estimate - out.len().min(estimate));
                    if out.len() > estimate {
                        self.budget.force_take(out.len() - estimate);
                    }
                    send(RunUpdate::Output(out));
                }
            }
            other => {
                // A step header or the end comes after everything printed before it.
                if !inner.held.is_empty() {
                    let out = inner.held.take();
                    self.sync(&mut inner);
                    self.budget.force_take(out.len());
                    send(RunUpdate::Output(out));
                }
                send(other);
            }
        }
    }

    /// The UI read some output: send what's held if the backlog is under the limit (it may
    /// then pass it by one held tail per run, never more).
    pub(super) fn flush(&self, mut send: impl FnMut(RunUpdate)) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.held.is_empty() || self.budget.queued() >= QUEUED_MAX {
            return;
        }
        let out = inner.held.take();
        self.sync(&mut inner);
        self.budget.force_take(out.len());
        send(RunUpdate::Output(out));
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        if inner.counted {
            self.budget.holding.fetch_sub(1, Ordering::AcqRel);
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

    /// Held output doesn't wait for the run to print again: the UI reading wakes the engine
    /// (once until it's handled), whose flush sends it; a dropped gate stops counting.
    #[test]
    fn reading_wakes_held_gates() {
        let budget = Arc::new(OutputBudget::default());
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        budget.set_waker(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        let gate = Gate::new(budget.clone());
        let mut sent = Vec::new();
        budget.force_take(QUEUED_MAX);
        gate.pass(RunUpdate::Output("last line\n".into()), |u| sent.push(u));
        assert!(sent.is_empty());
        // Still full: a flush sends nothing.
        gate.flush(|u| sent.push(u));
        assert!(sent.is_empty());
        budget.release(1024);
        budget.release(1024);
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "one wake until it's handled"
        );
        budget.woken();
        gate.flush(|u| sent.push(u));
        assert_eq!(outputs(&sent), ["last line\n"]);
        // Nothing held: reading doesn't wake.
        budget.release(1024);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        // A gate dropped while holding stops counting.
        let g2 = Gate::new(budget.clone());
        budget.force_take(QUEUED_MAX);
        g2.pass(RunUpdate::Output("x\n".into()), |_| {});
        assert_eq!(budget.holding.load(Ordering::SeqCst), 1);
        drop(g2);
        assert_eq!(budget.holding.load(Ordering::SeqCst), 0);
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
