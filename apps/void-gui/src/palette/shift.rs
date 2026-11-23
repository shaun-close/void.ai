//! Shift+Shift: two taps of Shift on their own, close together, open the palette.
//!
//! The detector only reads events (it never consumes them), so an editor that has focus still
//! gets every key, Shift+letter included.

use std::time::{Duration, Instant};

use egui::Event;

/// Both taps must land within this of each other (first release to second release), and
/// each tap must be shorter than this.
pub const WINDOW: Duration = Duration::from_millis(400);

#[derive(Debug, Default)]
pub struct DoubleShift {
    /// The modifiers the last report said were down.
    mods: egui::Modifiers,
    /// When this Shift press started, while it is still a clean tap (nothing else pressed).
    press: Option<Instant>,
    /// When the last clean tap ended.
    tap: Option<Instant>,
}

impl DoubleShift {
    /// Feed one input event that happened at `now`. True on the second Shift release of a
    /// double tap with no other key, button, scroll or modifier in between.
    pub fn feed(&mut self, event: &Event, now: Instant) -> bool {
        match event {
            Event::ModifiersChanged(m) => {
                let was = std::mem::replace(&mut self.mods, *m);
                let others = m.alt || m.ctrl || m.command || m.mac_cmd;
                if others || *m == was {
                    // A report that changes nothing egui shows means a modifier egui doesn't
                    // show changed: on Linux egui-winit drops Super, but winit still reports
                    // it (X11 when its full modifier state changes, Wayland on each compositor
                    // modifiers event). That brackets a window manager's Super+Shift+<key>,
                    // whose key the compositor keeps, so without this two such chords would
                    // look like Shift, Shift. A plain Shift tap only ever changes Shift.
                    self.spoil();
                } else if m.shift && !was.shift {
                    self.press = Some(now);
                } else if !m.shift && was.shift {
                    return self.released(now);
                }
                false
            }
            // Typing (Shift+letter included), input-method composing, shortcuts, clicks,
            // scrolling (Shift+wheel scrolls sideways) and losing the window all end a tap.
            Event::Key { .. }
            | Event::Text(_)
            | Event::Ime(_)
            | Event::Copy
            | Event::Cut
            | Event::Paste(_)
            | Event::PointerButton { pressed: true, .. }
            | Event::MouseWheel { .. }
            | Event::WindowFocused(_) => {
                self.spoil();
                false
            }
            _ => false,
        }
    }

    fn released(&mut self, now: Instant) -> bool {
        let tapped = self
            .press
            .take()
            .is_some_and(|t| now.saturating_duration_since(t) < WINDOW);
        if !tapped {
            self.tap = None;
            return false;
        }
        match self.tap.take() {
            Some(first) if now.saturating_duration_since(first) <= WINDOW => true,
            _ => {
                self.tap = Some(now);
                false
            }
        }
    }

    fn spoil(&mut self) {
        self.press = None;
        self.tap = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    fn shift(on: bool) -> Event {
        Event::ModifiersChanged(if on {
            Modifiers::SHIFT
        } else {
            Modifiers::NONE
        })
    }

    fn key(k: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    /// Feed `(ms, event)` pairs; the times at which it fired.
    fn run(events: &[(u64, Event)]) -> Vec<u64> {
        let t0 = Instant::now();
        let mut d = DoubleShift::default();
        events
            .iter()
            .filter(|(ms, e)| d.feed(e, t0 + Duration::from_millis(*ms)))
            .map(|(ms, _)| *ms)
            .collect()
    }

    #[test]
    fn double_shift_detector() {
        // Shift, Shift fires on the second release.
        let tap_tap = [
            (0, shift(true)),
            (60, shift(false)),
            (150, shift(true)),
            (210, shift(false)),
        ];
        assert_eq!(run(&tap_tap), vec![210]);

        // Shift+A, then Shift: the first Shift was part of typing, not a tap.
        let s = Modifiers::SHIFT;
        let shift_a_then_shift = [
            (0, shift(true)),
            (40, key(Key::A, true, s)),
            (45, Event::Text("A".into())),
            (80, key(Key::A, false, s)),
            (100, shift(false)),
            (180, shift(true)),
            (240, shift(false)),
        ];
        assert_eq!(run(&shift_a_then_shift), Vec::<u64>::new());

        // Typing "AB" fast (Shift held across both letters, then re-pressed) never fires.
        let typing = [
            (0, shift(true)),
            (30, key(Key::A, true, s)),
            (31, Event::Text("A".into())),
            (60, shift(false)),
            (90, shift(true)),
            (100, key(Key::B, true, s)),
            (101, Event::Text("B".into())),
            (130, shift(false)),
        ];
        assert_eq!(run(&typing), Vec::<u64>::new());

        // Slow: the second tap comes too late.
        let slow = [
            (0, shift(true)),
            (60, shift(false)),
            (600, shift(true)),
            (660, shift(false)),
        ];
        assert_eq!(run(&slow), Vec::<u64>::new());

        // Slow: Shift held down for a while is not a tap.
        let held = [
            (0, shift(true)),
            (900, shift(false)),
            (960, shift(true)),
            (1000, shift(false)),
        ];
        assert_eq!(run(&held), Vec::<u64>::new());

        // A slow pair, then a quick pair: the quick one fires.
        let slow_then_quick = [
            (0, shift(true)),
            (60, shift(false)),
            (700, shift(true)),
            (760, shift(false)),
            (900, shift(true)),
            (950, shift(false)),
        ];
        assert_eq!(run(&slow_then_quick), vec![950]);

        // Ctrl+Shift in between (a shortcut) spoils it.
        let with_ctrl = [
            (0, shift(true)),
            (60, shift(false)),
            (
                100,
                Event::ModifiersChanged(Modifiers::CTRL | Modifiers::SHIFT),
            ),
            (150, shift(false)),
            (200, shift(true)),
            (250, shift(false)),
        ];
        assert_eq!(run(&with_ctrl), Vec::<u64>::new());

        // Three taps fire once; a fourth and fifth make a new pair.
        let many = [
            (0, shift(true)),
            (40, shift(false)),
            (80, shift(true)),
            (120, shift(false)),
            (160, shift(true)),
            (200, shift(false)),
            (240, shift(true)),
            (280, shift(false)),
        ];
        assert_eq!(run(&many), vec![120, 280]);

        // Shift+wheel (horizontal scroll) twice is scrolling, not a double tap.
        let wheel = || Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::SHIFT,
            source: egui::MouseWheelSource::Wheel,
        };
        let shift_scroll = [
            (0, shift(true)),
            (40, wheel()),
            (80, shift(false)),
            (150, shift(true)),
            (190, wheel()),
            (230, shift(false)),
        ];
        assert_eq!(run(&shift_scroll), Vec::<u64>::new());

        // Composing text (an input method) between taps spoils them too.
        let ime = [
            (0, shift(true)),
            (60, shift(false)),
            (
                100,
                Event::Ime(egui::ImeEvent::Preedit {
                    text: "a".into(),
                    active_range_chars: None,
                }),
            ),
            (150, shift(true)),
            (210, shift(false)),
        ];
        assert_eq!(run(&ime), Vec::<u64>::new());
    }

    /// Super+Shift+<key> chords (a tiling window manager's): egui drops Super on Linux and the
    /// compositor keeps the key, so void sees only Shift going down and up, bracketed by
    /// modifier reports that change nothing egui shows (Super going down / up). Those reports
    /// end a tap, so two chords in a row don't open the palette.
    #[test]
    fn super_shift_chords_are_not_taps() {
        let none = || Event::ModifiersChanged(Modifiers::NONE);
        let chord = |t: u64| {
            vec![
                (t, none()),            // Super down: nothing egui shows changes
                (t + 30, shift(true)),  // Shift down; the key goes to the compositor
                (t + 90, shift(false)), // Shift up
                (t + 120, none()),      // Super up
            ]
        };
        let two: Vec<_> = chord(0).into_iter().chain(chord(200)).collect();
        assert_eq!(run(&two), Vec::<u64>::new());

        // Released in the other order (Super first, while Shift is still down).
        let other_order = |t: u64| {
            vec![
                (t, none()),
                (t + 30, shift(true)),
                (t + 60, shift(true)), // Super up while Shift is held
                (t + 90, shift(false)),
            ]
        };
        let two: Vec<_> = other_order(0).into_iter().chain(other_order(200)).collect();
        assert_eq!(run(&two), Vec::<u64>::new());

        // Plain taps still fire.
        let tap_tap = [
            (0, shift(true)),
            (60, shift(false)),
            (150, shift(true)),
            (210, shift(false)),
        ];
        assert_eq!(run(&tap_tap), vec![210]);
    }
}
