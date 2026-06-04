//! Terminal emulation with sixel graphics tracking.
//!
//! [`Terminal`] wraps avt's [`Vt`] — which interprets the recorded byte stream
//! into a grid of text cells — and adds a graphics layer for sixel images. avt
//! recognizes the DCS envelope sixel rides in but discards its payload, so the
//! byte stream is scanned here: each sixel DCS is decoded and anchored at the
//! cursor position avt reports just before the sequence.
//!
//! Image lifetime is approximated rather than fully emulated, since the cell
//! grid that would track scrolling and erasure lives inside avt. An image
//! replaces any earlier image sharing its anchor (so a TUI repainting the same
//! slot each frame stays clean), and all images are dropped when the display is
//! cleared or the terminal is reset.

use std::sync::Arc;

use avt::Vt;

use crate::sixel;

/// A decoded sixel image anchored to a terminal cell. The pixel buffer is
/// shared so cloning a [`Snapshot`] stays cheap.
#[derive(Clone, PartialEq)]
pub struct Image {
    /// Anchor cell column (top-left of the image).
    pub col: usize,
    /// Anchor cell row, in view coordinates.
    pub row: usize,
    pub data: Arc<sixel::Image>,
}

/// A sixel DCS still being accumulated, possibly across several output events.
struct Pending {
    col: usize,
    row: usize,
    body: String,
}

pub struct Terminal {
    vt: Vt,
    images: Vec<Image>,
    pending: Option<Pending>,
    /// A lone `ESC` left at the end of the previous feed, withheld so a DCS
    /// introducer (`ESC P`) or terminator (`ESC \`) split across output events
    /// is still recognized when the next feed supplies the second byte.
    trailing_esc: bool,
}

pub fn build(terminal_size: (usize, usize)) -> Terminal {
    Terminal::new(terminal_size)
}

impl Terminal {
    pub fn new(terminal_size: (usize, usize)) -> Self {
        let vt = Vt::builder()
            .size(terminal_size.0, terminal_size.1)
            .scrollback_limit(0)
            .build();

        Terminal {
            vt,
            images: Vec::new(),
            pending: None,
            trailing_esc: false,
        }
    }

    /// Feed terminal output to the emulator, extracting any sixel images.
    ///
    /// The full byte stream — sixel DCS included — is forwarded to avt so its
    /// parser state stays consistent; avt simply ignores the DCS payload. The
    /// stream is walked in order so screen clears and images interleave
    /// correctly.
    pub fn feed_str(&mut self, data: &str) {
        // Reattach an `ESC` carried over from the previous feed so a two-byte
        // DCS introducer/terminator split at the event boundary is seen whole.
        let reattached;
        let mut data = data;

        if self.trailing_esc {
            self.trailing_esc = false;
            reattached = format!("\u{1b}{data}");
            data = &reattached;
        }

        // A lone trailing `ESC` can only be completed by the next feed, so hold
        // it back from both avt and the scanner until then. Reattaching it
        // above restores the exact byte order avt sees.
        if let Some(stripped) = data.strip_suffix('\u{1b}') {
            self.trailing_esc = true;
            data = stripped;
        }

        let mut rest = data;

        loop {
            if self.pending.is_some() {
                match find_st(rest) {
                    Some((body_end, seq_end)) => {
                        self.vt.feed_str(&rest[..seq_end]);
                        let mut pending = self.pending.take().unwrap();
                        pending.body.push_str(&rest[..body_end]);
                        self.finish_sixel(pending);
                        rest = &rest[seq_end..];
                    }
                    None => {
                        self.vt.feed_str(rest);
                        self.pending.as_mut().unwrap().body.push_str(rest);
                        return;
                    }
                }

                continue;
            }

            match find_dcs(rest) {
                Some((start, body_start)) => {
                    let before = &rest[..start];
                    self.vt.feed_str(before);

                    if clears_display(before) {
                        self.images.clear();
                    }

                    // The image anchors where the cursor sits as the DCS
                    // arrives; avt leaves the cursor untouched by the payload.
                    let cursor = self.vt.cursor();
                    self.vt.feed_str(&rest[start..body_start]);
                    self.pending = Some(Pending {
                        col: cursor.col,
                        row: cursor.row,
                        body: String::new(),
                    });

                    rest = &rest[body_start..];
                }
                None => {
                    self.vt.feed_str(rest);

                    if clears_display(rest) {
                        self.images.clear();
                    }

                    return;
                }
            }
        }
    }

    fn finish_sixel(&mut self, pending: Pending) {
        let Some(image) = sixel::decode(&pending.body) else {
            return;
        };

        self.add_image(pending.col, pending.row, Arc::new(image));
    }

    fn add_image(&mut self, col: usize, row: usize, data: Arc<sixel::Image>) {
        self.images.retain(|img| (img.col, img.row) != (col, row));
        self.images.push(Image { col, row, data });
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.vt.view().cloned().collect(),
            cursor: self.vt.cursor().into(),
            images: self.images.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Snapshot {
    pub lines: Vec<avt::Line>,
    pub cursor: Option<(usize, usize)>,
    pub images: Vec<Image>,
}

impl Snapshot {
    pub fn same_visual(&self, other: &Snapshot) -> bool {
        self.lines == other.lines && self.cursor == other.cursor && self.images == other.images
    }
}

/// Find the next sixel DCS introducer, returning its start and the index where
/// the body (macro parameters and data) begins. Both the 7-bit `ESC P` and the
/// 8-bit `\u{90}` forms are recognized.
fn find_dcs(s: &str) -> Option<(usize, usize)> {
    let seven_bit = s.find("\u{1b}P").map(|i| (i, i + 2));
    let eight_bit = s.find('\u{90}').map(|i| (i, i + '\u{90}'.len_utf8()));

    match (seven_bit, eight_bit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Find the string terminator ending a DCS, returning the index where the body
/// ends and the index just past the terminator. Recognizes the 7-bit `ESC \`
/// and the 8-bit `\u{9c}` forms.
fn find_st(s: &str) -> Option<(usize, usize)> {
    let seven_bit = s.find("\u{1b}\\").map(|i| (i, i + 2));
    let eight_bit = s.find('\u{9c}').map(|i| (i, i + '\u{9c}'.len_utf8()));

    match (seven_bit, eight_bit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether a chunk contains a sequence that clears the whole display: erase-in-
/// display all (`CSI 2 J`) or with scrollback (`CSI 3 J`), or a full reset
/// (`ESC c`).
fn clears_display(s: &str) -> bool {
    s.contains("\u{1b}[2J") || s.contains("\u{1b}[3J") || s.contains("\u{1b}c")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (usize, usize) = (10, 4);

    // A 1x6 opaque red sixel image.
    const RED_SIXEL: &str = "\u{1b}Pq#0;2;100;0;0~\u{1b}\\";

    fn red() -> rgb::RGBA8 {
        rgb::RGBA8::new(255, 0, 0, 255)
    }

    #[test]
    fn extracts_a_sixel_image_at_the_cursor() {
        let mut term = Terminal::new(SIZE);
        term.feed_str("ab");
        term.feed_str(RED_SIXEL);

        let snapshot = term.snapshot();

        assert_eq!(snapshot.images.len(), 1);
        let image = &snapshot.images[0];
        // Cursor sat at column 2 (after "ab") on row 0.
        assert_eq!((image.col, image.row), (2, 0));
        assert_eq!(image.data.pixels[0], red());
    }

    #[test]
    fn surrounding_text_still_lands_in_the_grid() {
        let mut term = Terminal::new(SIZE);
        term.feed_str("a");
        term.feed_str(RED_SIXEL);
        term.feed_str("b");

        let snapshot = term.snapshot();

        // The DCS is invisible to the text grid; "a" and "b" sit adjacently.
        assert_eq!(snapshot.lines[0].text().trim_end(), "ab");
    }

    #[test]
    fn reassembles_a_sixel_split_across_feeds() {
        let mut term = Terminal::new(SIZE);
        term.feed_str("\u{1b}Pq#0;2;100;0;0");
        // No image yet — the DCS is still open.
        assert!(term.snapshot().images.is_empty());

        term.feed_str("~\u{1b}\\");

        let snapshot = term.snapshot();
        assert_eq!(snapshot.images.len(), 1);
        assert_eq!(snapshot.images[0].data.pixels[0], red());
    }

    #[test]
    fn reassembles_a_sixel_split_at_the_st_terminator() {
        // The ST terminator `ESC \` is split across events: the `ESC` ends one
        // chunk, the `\` begins the next.
        let mut term = Terminal::new(SIZE);
        term.feed_str("\u{1b}Pq#0;2;100;0;0~\u{1b}");
        assert!(term.snapshot().images.is_empty());

        term.feed_str("\\");

        let snapshot = term.snapshot();
        assert_eq!(snapshot.images.len(), 1);
        assert_eq!(snapshot.images[0].data.pixels[0], red());
    }

    #[test]
    fn detects_a_sixel_introducer_split_across_feeds() {
        // The `ESC P` introducer is split across events: the `ESC` ends one
        // chunk, the `P` begins the next.
        let mut term = Terminal::new(SIZE);
        term.feed_str("ab\u{1b}");
        term.feed_str("Pq#0;2;100;0;0~\u{1b}\\");

        let snapshot = term.snapshot();
        assert_eq!(snapshot.images.len(), 1);
        // The cursor sat at column 2 (after "ab") when the DCS arrived.
        assert_eq!((snapshot.images[0].col, snapshot.images[0].row), (2, 0));
        assert_eq!(snapshot.images[0].data.pixels[0], red());
    }

    #[test]
    fn repaint_at_the_same_anchor_replaces_the_image() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        // Re-home the cursor and emit a green image at the same spot.
        term.feed_str("\u{1b}[H\u{1b}Pq#0;2;0;100;0~\u{1b}\\");

        let snapshot = term.snapshot();

        assert_eq!(snapshot.images.len(), 1);
        assert_eq!(
            snapshot.images[0].data.pixels[0],
            rgb::RGBA8::new(0, 255, 0, 255)
        );
    }

    #[test]
    fn distinct_anchors_accumulate() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        term.feed_str("\u{1b}[2;1H");
        term.feed_str(RED_SIXEL);

        assert_eq!(term.snapshot().images.len(), 2);
    }

    #[test]
    fn clearing_the_display_drops_images() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        assert_eq!(term.snapshot().images.len(), 1);

        term.feed_str("\u{1b}[2J");
        assert!(term.snapshot().images.is_empty());
    }

    #[test]
    fn reset_drops_images() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        term.feed_str("\u{1b}c");

        assert!(term.snapshot().images.is_empty());
    }

    #[test]
    fn image_emitted_after_clear_in_same_chunk_survives() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        // Clear precedes a fresh image within one feed: the clear drops the
        // first image, the second is kept.
        term.feed_str(&format!("\u{1b}[2J{RED_SIXEL}"));

        assert_eq!(term.snapshot().images.len(), 1);
    }
}
