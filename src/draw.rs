//! Frame drawing with an image-aware diff (ported from mopytui).
//!
//! ratatui-image puts a whole inline image (an iTerm2 `OSC 1337` escape with
//! the base64 PNG, easily 100 kB+) into the *symbol* of one buffer cell.
//! `Buffer::diff` then treats that cell as a glyph that is as many columns
//! wide as the string has characters, and re-sends every following cell of the
//! frame ("invalidated by a preceding multi-width character") — including the
//! next image's full PNG. With several images in the scrollback every image
//! after the first was re-transmitted on each redraw (every keystroke, IRC
//! event and typing tick), which iTerm2 shows as heavy flicker and
//! half-painted images.
//!
//! [`Drawer`] replaces `Terminal::draw`/`flush` with the same algorithm, except
//! that a cell carrying an escape sequence counts as one column wide.
//! Unchanged images and text are then left alone.

use std::io;

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

/// Display width of a cell symbol for diffing purposes. Image escapes (they
/// contain ESC) occupy exactly the one cell they are anchored to.
fn symbol_width(symbol: &str) -> usize {
    if symbol.contains('\x1b') {
        1
    } else {
        symbol.width()
    }
}

/// Same as `Buffer::diff` (ratatui 0.29), but with [`symbol_width`].
pub fn diff<'a>(previous: &Buffer, next: &'a Buffer) -> Vec<(u16, u16, &'a Cell)> {
    let mut updates: Vec<(u16, u16, &Cell)> = Vec::new();
    // Cells invalidated by drawing/replacing preceding multi-width characters.
    let mut invalidated: usize = 0;
    // Cells to skip because a preceding multi-width character covers them.
    let mut to_skip: usize = 0;
    for (i, (current, prev)) in next.content.iter().zip(previous.content.iter()).enumerate() {
        let cur_w = symbol_width(current.symbol());
        if !current.skip && (current != prev || invalidated > 0) && to_skip == 0 {
            let (x, y) = next.pos_of(i);
            updates.push((x, y, current));
        }
        to_skip = cur_w.saturating_sub(1);
        let affected_width = cur_w.max(symbol_width(prev.symbol()));
        invalidated = affected_width.max(invalidated).saturating_sub(1);
    }
    updates
}

/// Draws frames through [`diff`] instead of ratatui's own flush.
#[derive(Default)]
pub struct Drawer {
    /// The buffer as last sent to the terminal (what the screen shows).
    prev: Option<Buffer>,
}

impl Drawer {
    /// `render` returns where to show the cursor (`None` hides it).
    pub fn draw<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        render: impl FnOnce(&mut Frame) -> Option<(u16, u16)>,
    ) -> io::Result<()> {
        // Resizes clear the screen and reset ratatui's buffers; mirror that.
        terminal.autoresize()?;
        let cursor = {
            let mut frame = terminal.get_frame();
            render(&mut frame)
        };
        let cur = terminal.current_buffer_mut().clone();
        let blank;
        let prev = match &self.prev {
            Some(p) if p.area == cur.area => p,
            _ => {
                blank = Buffer::empty(cur.area);
                &blank
            }
        };
        let updates = diff(prev, &cur);
        terminal.backend_mut().draw(updates.into_iter())?;
        match cursor {
            Some(pos) => {
                terminal.show_cursor()?;
                terminal.set_cursor_position(pos)?;
            }
            None => terminal.hide_cursor()?,
        }
        terminal.swap_buffers();
        terminal.backend_mut().flush()?;
        self.prev = Some(cur);
        Ok(())
    }

    /// Clears the screen; the next frame is then sent in full.
    pub fn clear<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<()> {
        self.prev = None;
        terminal.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    /// A fake inline-image cell: a long base64-ish payload wrapped in escapes.
    fn image_symbol() -> String {
        format!("\x1b]1337;File=inline=1:{}\x07", "A".repeat(50_000))
    }

    fn two_images_buffer() -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        let sym = image_symbol();
        for (x, y) in [(2u16, 1u16), (22, 1)] {
            buf[(x, y)].set_symbol(&sym);
            // The rest of the image area is skipped, like ratatui-image does.
            for dy in 0..4 {
                for dx in 0..10 {
                    if (dx, dy) != (0, 0) {
                        buf[(x + dx, y + dy)].set_skip(true);
                    }
                }
            }
        }
        buf.set_string(0, 8, "unchanged text", Style::default());
        buf
    }

    #[test]
    fn unchanged_frame_sends_nothing_even_with_several_images() {
        let a = two_images_buffer();
        let b = two_images_buffer();
        assert!(diff(&a, &b).is_empty());
        // ratatui's own diff re-sends the second image and every later cell.
        assert!(a.diff(&b).len() > 100, "premise: stock diff is noisy");
    }

    #[test]
    fn changed_text_below_an_image_is_sent_alone() {
        let a = two_images_buffer();
        let mut b = two_images_buffer();
        b.set_string(0, 8, "CHANGED", Style::default());
        let updates = diff(&a, &b);
        assert!(updates.iter().all(|(_, _, c)| !c.symbol().contains('\x1b')));
        assert!(!updates.is_empty() && updates.len() <= 8);
    }

    #[test]
    fn a_changed_image_is_resent() {
        let a = two_images_buffer();
        let mut b = two_images_buffer();
        b[(22, 1)].set_symbol(&format!("\x1b]1337;{}\x07", "B".repeat(100)));
        let updates = diff(&a, &b);
        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].0, updates[0].1), (22, 1));
    }

    #[test]
    fn wide_characters_still_invalidate_their_neighbours() {
        let mut a = Buffer::empty(Rect::new(0, 0, 6, 1));
        a.set_string(0, 0, "日本", Style::default());
        let mut b = Buffer::empty(Rect::new(0, 0, 6, 1));
        b.set_string(0, 0, "ab", Style::default());
        assert_eq!(diff(&a, &b).len(), a.diff(&b).len());
    }
}
