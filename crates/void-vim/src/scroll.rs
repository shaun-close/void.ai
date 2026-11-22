//! Scrolling (`:help scroll.txt`): `zz` `zt` `zb` (and `z<CR>` `z.` `z-`), `<C-d>` `<C-u>`,
//! `<C-f>` `<C-b>`, `<C-e>` `<C-y>`. The host owns the view; the engine asks it to scroll and keeps
//! the cursor inside it.

use crate::engine::Engine;
use crate::parse::Scroll;
use crate::text::*;
use crate::{ScrollAt, VimHost};

impl Engine {
    pub(crate) fn scroll(&mut self, h: &mut dyn VimHost, s: Scroll, count: Option<usize>) -> bool {
        let r = h.rope();
        let last = last_line(r);
        let cur = h.selection().1;
        let line = line_of(r, cur);
        let (top, bottom) = h.visible_lines();
        let top = top.min(last);
        let height = bottom.saturating_sub(top) + 1;
        let n = count.unwrap_or(1);
        let page = n * height.saturating_sub(2).max(1);
        let (view, at, cursor_line, first_col) = match s {
            Scroll::Top
            | Scroll::Center
            | Scroll::Bottom
            | Scroll::TopFirst
            | Scroll::CenterFirst
            | Scroll::BottomFirst => {
                let l = count.map_or(line, |c| (c.max(1) - 1).min(last));
                let at = match s {
                    Scroll::Top | Scroll::TopFirst => ScrollAt::Top,
                    Scroll::Center | Scroll::CenterFirst => ScrollAt::Center,
                    _ => ScrollAt::Bottom,
                };
                let first = matches!(
                    s,
                    Scroll::TopFirst | Scroll::CenterFirst | Scroll::BottomFirst
                );
                (l, at, Some(l), first)
            }
            Scroll::HalfDown | Scroll::HalfUp => {
                if count.is_some() {
                    self.scroll_amount = count;
                }
                let amount = self.scroll_amount.unwrap_or((height / 2).max(1));
                if s == Scroll::HalfDown {
                    if line == last {
                        return false;
                    }
                    let view = (top + amount).min(last);
                    (view, ScrollAt::Top, Some((line + amount).min(last)), true)
                } else {
                    if line == 0 {
                        return false;
                    }
                    let view = top.saturating_sub(amount);
                    (view, ScrollAt::Top, Some(line.saturating_sub(amount)), true)
                }
            }
            Scroll::PageDown | Scroll::LineDown => {
                if top >= last {
                    return false;
                }
                let step = if s == Scroll::PageDown { page } else { n };
                let view = (top + step).min(last);
                (
                    view,
                    ScrollAt::Top,
                    (line < view).then_some(view),
                    s == Scroll::PageDown,
                )
            }
            Scroll::PageUp | Scroll::LineUp => {
                if top == 0 {
                    return false;
                }
                let step = if s == Scroll::PageUp { page } else { n };
                let view = top.saturating_sub(step);
                let view_bottom = view + height - 1;
                (
                    view,
                    ScrollAt::Top,
                    (line > view_bottom).then_some(view_bottom),
                    s == Scroll::PageUp,
                )
            }
        };
        h.scroll_to(view, at);
        if let Some(l) = cursor_line {
            let r = h.rope();
            let pos = if first_col {
                first_non_blank(r, l)
            } else if l == line {
                cur
            } else {
                (line_start(r, l) + col(r, cur)).min(line_last(r, l))
            };
            if pos != cur {
                self.set_cursor(h, pos);
            }
        }
        true
    }
}
