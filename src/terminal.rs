//! Terminal emulation.
//!
//! [`Terminal`] wraps avt's [`Vt`], which interprets the recorded byte stream
//! into a grid of text cells and decoded sixel images. A [`Snapshot`] captures
//! the visible lines, cursor, and placed images at a point in time; the
//! renderer turns a snapshot into pixels.
//!
//! Sixel capture, decoding, placement, and lifetime (scrolling, erasure, buffer
//! switching) all live in avt and are surfaced here via [`avt::Vt::images`].

use avt::{Image, Vt};

pub struct Terminal {
    vt: Vt,
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

        Terminal { vt }
    }

    pub fn feed_str(&mut self, data: &str) {
        self.vt.feed_str(data);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.vt.view().cloned().collect(),
            cursor: self.vt.cursor().into(),
            images: self.vt.images().to_vec(),
        }
    }
}

/// A terminal state at a point in time: text cells, the cursor, and any sixel
/// images placed in the active buffer.
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
    fn surfaces_a_sixel_image_at_the_cursor() {
        let mut term = Terminal::new(SIZE);
        term.feed_str("ab");
        term.feed_str(RED_SIXEL);

        let snapshot = term.snapshot();

        assert_eq!(snapshot.images.len(), 1);
        // avt anchors the image where the cursor sat (column 2, after "ab").
        assert_eq!((snapshot.images[0].col, snapshot.images[0].row), (2, 0));
        assert_eq!(snapshot.images[0].pixels()[0], red());
        // The DCS is invisible to the text grid.
        assert_eq!(snapshot.lines[0].text().trim_end(), "ab");
    }

    #[test]
    fn clearing_the_display_drops_images() {
        let mut term = Terminal::new(SIZE);
        term.feed_str(RED_SIXEL);
        assert_eq!(term.snapshot().images.len(), 1);

        term.feed_str("\u{1b}[2J");
        assert!(term.snapshot().images.is_empty());
    }
}
