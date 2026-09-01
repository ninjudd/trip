//! The interactive session chooser.
//!
//! Deliberately owns no terminal. It is fed bytes and hands back the bytes to
//! paint, because it has two callers with incompatible ideas about who reads
//! stdin: `trip enter`, which can block on the fd, and an attached client,
//! where a thread already holds stdin and forwards into a channel. Two readers
//! of one fd is not a fixable arrangement, so the chooser stops being a reader.

/// What the user decided. Anything else is still in progress.
pub enum Outcome {
    Pick(usize),
    /// Esc, `q`, or Ctrl-C — go back to wherever the chooser was opened from.
    Cancel,
    /// The detach key, pressed a second time. Only ever produced when the
    /// caller supplies one.
    Detach,
}

/// Where the escape parser sits between bytes. An arrow arrives as three bytes
/// that can be split across reads, and a lone Esc is the same first byte, so
/// the two are told apart by what follows — or by silence, via [`Chooser::tick`].
///
/// The parser consumes *whole* sequences, not just the ones it acts on. The
/// terminal keeps whatever modes the session's program enabled, so a mouse
/// click can arrive as `ESC [ < 0 ; 1 2 ; 5 M` — and a parser that only knew
/// arrows would read the `1` in the middle as a jump-to-row.
#[derive(PartialEq)]
enum Esc {
    None,
    /// Saw ESC.
    Pending,
    /// Inside `ESC [`; bytes accumulate until a final byte (0x40-0x7E).
    Csi(Vec<u8>),
    /// Saw `ESC O`; the next byte is the whole sequence (SS3 arrows).
    Ss3,
}

pub struct Chooser {
    rows: Vec<String>,
    selected: usize,
    /// First row of the visible window.
    top: usize,
    /// How many rows fit. Markers are painted outside it.
    viewport: usize,
    /// Columns available. Rows are truncated to fit, because a row the
    /// terminal auto-wraps occupies two physical lines while the in-place
    /// redraw counts one, and the cursor arithmetic never recovers.
    width: usize,
    detach_key: Option<u8>,
    esc: Esc,
    /// Inside a bracketed paste. Pasted bytes select nothing; a paste is not
    /// the user answering the chooser.
    in_paste: bool,
    /// Lines the last render painted, so the next one can redraw in place.
    drawn: usize,
}

impl Chooser {
    pub fn new(
        rows: Vec<String>,
        selected: usize,
        (viewport, width): (usize, usize),
        detach_key: Option<u8>,
    ) -> Self {
        let viewport = viewport.max(1);
        let selected = if rows.is_empty() {
            0
        } else {
            selected.min(rows.len() - 1)
        };
        let mut c = Chooser {
            rows,
            selected,
            top: 0,
            viewport,
            width: width.max(20),
            detach_key,
            esc: Esc::None,
            in_paste: false,
            drawn: 0,
        };
        c.scroll_to_selected();
        c
    }

    /// True while a half-read escape sequence is outstanding, so the caller
    /// knows to apply the idle timeout that resolves it.
    pub fn pending_escape(&self) -> bool {
        self.esc != Esc::None
    }

    /// Consume input, stopping at the first decision. Bytes after a decision
    /// are dropped, which is what the old reader did by returning one key per
    /// call.
    pub fn feed(&mut self, data: &[u8]) -> Option<Outcome> {
        for &b in data {
            if let Some(outcome) = self.byte(b) {
                return Some(outcome);
            }
        }
        None
    }

    /// Resolve a pending escape after an idle interval. A lone Esc is only
    /// knowable by the silence after it.
    pub fn tick(&mut self) -> Option<Outcome> {
        match self.esc {
            Esc::Pending => {
                self.esc = Esc::None;
                Some(Outcome::Cancel)
            }
            // A truncated sequence. Forget it rather than stay wedged.
            Esc::Csi(_) | Esc::Ss3 => {
                self.esc = Esc::None;
                None
            }
            Esc::None => None,
        }
    }

    fn byte(&mut self, b: u8) -> Option<Outcome> {
        // The detach key outranks the parser. It is the way out, and it should
        // not depend on whether a stray Esc happens to be half-read. Not
        // inside a paste, though: pasted bytes are content, never commands.
        if self.detach_key == Some(b) && !self.in_paste {
            self.esc = Esc::None;
            return Some(Outcome::Detach);
        }

        match std::mem::replace(&mut self.esc, Esc::None) {
            Esc::Pending => {
                match b {
                    b'[' => self.esc = Esc::Csi(Vec::new()),
                    b'O' => self.esc = Esc::Ss3,
                    // Esc twice: the first one was standalone after all, and
                    // the second says so without waiting for the timeout.
                    0x1b if !self.in_paste => return Some(Outcome::Cancel),
                    // Alt-<key> and anything else: drop both bytes, as the
                    // old reader's `Key::Other` did.
                    _ => {}
                }
                None
            }
            Esc::Csi(mut buf) => {
                if (0x40..=0x7e).contains(&b) {
                    // The final byte. Act on the few sequences that mean
                    // something here; swallowing the rest whole is the point.
                    match (buf.as_slice(), b) {
                        ([], b'A') if !self.in_paste => self.up(),
                        ([], b'B') if !self.in_paste => self.down(),
                        (b"200", b'~') => self.in_paste = true,
                        (b"201", b'~') => self.in_paste = false,
                        _ => {}
                    }
                } else if buf.len() < 24 {
                    buf.push(b);
                    self.esc = Esc::Csi(buf);
                }
                // Over-long sequences are abandoned rather than accumulated:
                // nothing this parser acts on is anywhere near that size.
                None
            }
            Esc::Ss3 => {
                if !self.in_paste {
                    match b {
                        b'A' => self.up(),
                        b'B' => self.down(),
                        _ => {}
                    }
                }
                None
            }
            Esc::None if self.in_paste => {
                if b == 0x1b {
                    self.esc = Esc::Pending;
                }
                None
            }
            Esc::None => match b {
                b'\r' | b'\n' => (!self.rows.is_empty()).then_some(Outcome::Pick(self.selected)),
                0x03 | b'q' => Some(Outcome::Cancel),
                b'k' => {
                    self.up();
                    None
                }
                b'j' => {
                    self.down();
                    None
                }
                d @ b'1'..=b'9' => {
                    // Numbered as rendered, so the digit means the row the
                    // user can actually count to.
                    let nth = (d - b'0') as usize;
                    let index = self.top + nth - 1;
                    (index < self.window_end()).then_some(Outcome::Pick(index))
                }
                0x1b => {
                    self.esc = Esc::Pending;
                    None
                }
                _ => None,
            },
        }
    }

    fn up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.rows.len() - 1
        } else {
            self.selected - 1
        };
        self.scroll_to_selected();
    }

    fn down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
        self.scroll_to_selected();
    }

    fn window_end(&self) -> usize {
        (self.top + self.viewport).min(self.rows.len())
    }

    fn scroll_to_selected(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.viewport {
            self.top = self.selected + 1 - self.viewport;
        }
        let max_top = self.rows.len().saturating_sub(self.viewport);
        self.top = self.top.min(max_top);
    }

    /// Refit to a new window. The caller is repainting from scratch, so the
    /// in-place redraw offset is dropped with it.
    pub fn resize(&mut self, (viewport, width): (usize, usize)) {
        self.viewport = viewport.max(1);
        self.width = width.max(20);
        self.drawn = 0;
        self.scroll_to_selected();
    }

    /// The bytes that paint the list. The first call draws; every later call
    /// redraws in place over what it painted last.
    pub fn render(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.drawn > 0 {
            out.extend_from_slice(format!("\r\x1b[{}A", self.drawn).as_bytes());
        }

        let (start, end) = (self.top, self.window_end());
        let mut lines = 0;

        if start > 0 {
            line(&mut out, "  ⋯");
            lines += 1;
        }
        for i in start..end {
            let nth = i - start + 1;
            // Only the reachable rows are numbered; a number that no key
            // selects is a lie about what the digit does.
            let label = if nth <= 9 {
                format!("{}) ", nth)
            } else {
                "   ".to_string()
            };
            // "> " plus the label is five columns; the last column is left
            // unused so the terminal never auto-wraps the row.
            let room = self.width.saturating_sub(6);
            let row: String = self.rows[i].chars().take(room).collect();
            if i == self.selected {
                line(&mut out, &format!("> \x1b[7m{}{}\x1b[0m", label, row));
            } else {
                line(&mut out, &format!("  {}{}", label, row));
            }
            lines += 1;
        }
        if end < self.rows.len() {
            line(&mut out, "  ⋯");
            lines += 1;
        }

        // A shorter render than last time — the markers come and go as the
        // window reaches an end — would leave the tail of the old one on
        // screen.
        if lines < self.drawn {
            let surplus = self.drawn - lines;
            for _ in 0..surplus {
                out.extend_from_slice(b"\x1b[2K\n");
            }
            out.extend_from_slice(format!("\x1b[{}A", surplus).as_bytes());
        }

        self.drawn = lines;
        out
    }
}

fn line(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(b"\x1b[2K");
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chooser(n: usize, viewport: usize) -> Chooser {
        let rows = (0..n).map(|i| format!("row{}", i)).collect();
        Chooser::new(rows, 0, (viewport, 100), Some(0x1c))
    }

    fn pick(outcome: Option<Outcome>) -> Option<usize> {
        match outcome {
            Some(Outcome::Pick(i)) => Some(i),
            _ => None,
        }
    }

    #[test]
    fn enter_picks_the_selection() {
        let mut c = chooser(3, 10);
        assert!(c.feed(b"j").is_none());
        assert_eq!(pick(c.feed(b"\r")), Some(1));
    }

    #[test]
    fn j_and_k_move_and_wrap() {
        let mut c = chooser(3, 10);
        c.feed(b"k");
        assert_eq!(pick(c.feed(b"\r")), Some(2));
        let mut c = chooser(3, 10);
        c.feed(b"jjj");
        assert_eq!(pick(c.feed(b"\r")), Some(0));
    }

    #[test]
    fn arrows_move() {
        let mut c = chooser(3, 10);
        c.feed(b"\x1b[B");
        assert_eq!(pick(c.feed(b"\r")), Some(1));
        c.feed(b"\x1bOA");
        assert_eq!(pick(c.feed(b"\r")), Some(0));
    }

    #[test]
    fn an_arrow_split_across_reads_still_moves() {
        // The terminal usually writes the three bytes at once, but a slow pipe
        // or a full buffer can split them anywhere.
        let mut c = chooser(3, 10);
        c.feed(b"\x1b");
        c.feed(b"[");
        c.feed(b"B");
        assert_eq!(pick(c.feed(b"\r")), Some(1));
    }

    #[test]
    fn a_lone_escape_cancels_only_on_the_idle_tick() {
        let mut c = chooser(3, 10);
        assert!(c.feed(b"\x1b").is_none());
        assert!(c.pending_escape());
        assert!(matches!(c.tick(), Some(Outcome::Cancel)));
        assert!(!c.pending_escape());
    }

    #[test]
    fn escape_twice_cancels_without_waiting() {
        let mut c = chooser(3, 10);
        c.feed(b"\x1b");
        assert!(matches!(c.feed(b"\x1b"), Some(Outcome::Cancel)));
    }

    #[test]
    fn a_truncated_arrow_does_not_wedge_the_parser() {
        let mut c = chooser(3, 10);
        c.feed(b"\x1b[");
        assert!(c.pending_escape());
        assert!(c.tick().is_none());
        assert!(!c.pending_escape());
        // Still usable afterwards.
        assert_eq!(pick(c.feed(b"\r")), Some(0));
    }

    #[test]
    fn alt_key_is_swallowed_rather_than_cancelling() {
        let mut c = chooser(3, 10);
        assert!(c.feed(b"\x1bj").is_none());
        // The `j` went with the Esc, so the selection did not move.
        assert_eq!(pick(c.feed(b"\r")), Some(0));
    }

    #[test]
    fn q_and_ctrl_c_cancel() {
        let mut c = chooser(3, 10);
        assert!(matches!(c.feed(b"q"), Some(Outcome::Cancel)));
        let mut c = chooser(3, 10);
        assert!(matches!(c.feed(b"\x03"), Some(Outcome::Cancel)));
    }

    #[test]
    fn digits_pick_directly() {
        let mut c = chooser(5, 10);
        assert_eq!(pick(c.feed(b"3")), Some(2));
    }

    #[test]
    fn a_digit_past_the_end_of_the_list_does_nothing() {
        let mut c = chooser(3, 10);
        assert!(c.feed(b"7").is_none());
    }

    #[test]
    fn the_detach_key_outranks_a_half_read_escape() {
        let mut c = chooser(3, 10);
        c.feed(b"\x1b");
        assert!(matches!(c.feed(b"\x1c"), Some(Outcome::Detach)));
        assert!(!c.pending_escape());
    }

    #[test]
    fn no_detach_key_means_the_byte_is_ignored() {
        let rows = vec!["a".to_string(), "b".to_string()];
        let mut c = Chooser::new(rows, 0, (10, 100), None);
        assert!(c.feed(b"\x1c").is_none());
    }

    #[test]
    fn digits_number_the_visible_rows_not_the_whole_list() {
        // Scrolled down, `1` is the first row on screen rather than the first
        // row of the list.
        let mut c = chooser(20, 5);
        for _ in 0..7 {
            c.feed(b"j");
        }
        assert_eq!(c.top, 3);
        assert_eq!(pick(c.feed(b"1")), Some(3));
    }

    #[test]
    fn the_window_follows_the_selection_downward() {
        let mut c = chooser(10, 4);
        assert_eq!((c.top, c.window_end()), (0, 4));
        for _ in 0..4 {
            c.feed(b"j");
        }
        assert_eq!(c.selected, 4);
        assert_eq!((c.top, c.window_end()), (1, 5));
    }

    #[test]
    fn the_window_follows_the_selection_upward() {
        let mut c = chooser(10, 4);
        for _ in 0..6 {
            c.feed(b"j");
        }
        assert_eq!(c.top, 3);
        for _ in 0..5 {
            c.feed(b"k");
        }
        assert_eq!(c.selected, 1);
        assert_eq!(c.top, 1);
    }

    #[test]
    fn wrapping_up_jumps_the_window_to_the_end() {
        let mut c = chooser(10, 4);
        c.feed(b"k");
        assert_eq!(c.selected, 9);
        assert_eq!((c.top, c.window_end()), (6, 10));
    }

    #[test]
    fn wrapping_down_jumps_the_window_back_to_the_start() {
        let mut c = chooser(10, 4);
        c.feed(b"k");
        c.feed(b"j");
        assert_eq!(c.selected, 0);
        assert_eq!((c.top, c.window_end()), (0, 4));
    }

    #[test]
    fn a_list_shorter_than_the_window_never_scrolls() {
        let mut c = chooser(3, 10);
        for _ in 0..5 {
            c.feed(b"j");
        }
        assert_eq!(c.top, 0);
        assert_eq!(c.window_end(), 3);
    }

    #[test]
    fn an_initial_selection_below_the_fold_scrolls_into_view() {
        let rows: Vec<String> = (0..20).map(|i| format!("row{}", i)).collect();
        let c = Chooser::new(rows, 15, (5, 100), None);
        assert!(c.top <= 15 && 15 < c.window_end());
    }

    #[test]
    fn markers_mark_only_the_truncated_ends() {
        let mut c = chooser(10, 4);
        let first = String::from_utf8(c.render()).unwrap();
        assert!(!first.starts_with("\x1b[2K  ⋯"), "no marker above the top");
        assert!(first.contains("⋯"), "marker below");

        c.feed(b"k"); // wrap to the last row
        let last = String::from_utf8(c.render()).unwrap();
        assert_eq!(last.matches('⋯').count(), 1, "only the top is truncated");
    }

    #[test]
    fn a_shorter_redraw_erases_the_lines_it_no_longer_paints() {
        // Scrolled to the middle both markers show; back at the top only one
        // does, and the line the other occupied has to be cleared.
        let mut c = chooser(10, 4);
        for _ in 0..5 {
            c.feed(b"j");
        }
        let middle = c.render();
        assert_eq!(String::from_utf8_lossy(&middle).matches('⋯').count(), 2);
        for _ in 0..5 {
            c.feed(b"k");
        }
        let top = String::from_utf8(c.render()).unwrap();
        assert_eq!(top.matches('⋯').count(), 1);
        // One surplus line cleared, then the cursor put back.
        assert!(top.contains("\x1b[1A"), "cursor restored after the erase");
    }

    #[test]
    fn the_selected_row_is_the_highlighted_one() {
        let mut c = chooser(3, 10);
        c.feed(b"j");
        let painted = String::from_utf8(c.render()).unwrap();
        assert!(painted.contains("> \x1b[7m2) row1\x1b[0m"));
        assert!(painted.contains("  1) row0"));
    }

    #[test]
    fn rows_past_the_ninth_are_unnumbered_because_no_digit_reaches_them() {
        let mut c = chooser(12, 12);
        let painted = String::from_utf8(c.render()).unwrap();
        assert!(painted.contains("9) row8"));
        assert!(painted.contains("   row9"));
    }

    #[test]
    fn an_empty_list_cannot_be_picked() {
        let mut c = Chooser::new(vec![], 0, (10, 100), None);
        assert!(c.feed(b"\r").is_none());
        assert!(matches!(c.feed(b"q"), Some(Outcome::Cancel)));
    }

    #[test]
    fn a_mouse_click_selects_nothing() {
        // The terminal keeps whatever mouse mode the session's app enabled,
        // and an SGR report carries digits — which used to read as a jump.
        let mut c = chooser(5, 10);
        assert!(c.feed(b"\x1b[<0;12;5M").is_none());
        assert_eq!(pick(c.feed(b"\r")), Some(0), "selection did not move");
    }

    #[test]
    fn pasted_text_selects_nothing() {
        let mut c = chooser(5, 10);
        assert!(c.feed(b"\x1b[200~q3\r\x1c\x1b[201~").is_none());
        // The paste ended; the chooser is still live and keys work again.
        assert_eq!(pick(c.feed(b"3")), Some(2));
    }

    #[test]
    fn the_detach_key_inside_a_paste_is_content() {
        let mut c = chooser(3, 10);
        c.feed(b"\x1b[200~");
        assert!(c.feed(b"\x1c").is_none());
        c.feed(b"\x1b[201~");
        assert!(matches!(c.feed(b"\x1c"), Some(Outcome::Detach)));
    }

    #[test]
    fn rows_are_truncated_to_the_window_width() {
        let rows = vec!["x".repeat(200)];
        let mut c = Chooser::new(rows, 0, (10, 40), None);
        let painted = String::from_utf8(c.render()).unwrap();
        let body = painted
            .lines()
            .next()
            .unwrap()
            .trim_end_matches("\x1b[0m");
        // "\x1b[2K> \x1b[7m1) " + 34 chars of row = 39 columns painted.
        assert!(body.chars().filter(|c| *c == 'x').count() == 34, "{:?}", body);
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // A multibyte title must not be split mid-character.
        let rows = vec!["⋯".repeat(60)];
        let mut c = Chooser::new(rows, 0, (10, 40), None);
        assert!(String::from_utf8(c.render()).is_ok());
    }
}
