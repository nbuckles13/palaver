//! Pictures: one frame of video as it passes between a camera, a codec and a screen.
//!
//! A [`Picture`] is kept the way cameras and codecs keep one: brightness for every
//! pixel, and colour for every two by two of them (I420, with the BT.601 numbers video
//! uses). [`Picture::to_rgba`] makes of it what a screen wants. All of it is whole-number
//! arithmetic, so that it comes out the same on every machine.
//!
//! [`Pattern`] makes moving pictures for bots and tests.

/// The most a picture may measure either way.
pub const MOST_PIXELS_ACROSS: u32 = 8192;

/// How large a picture is, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    width: u32,
    height: u32,
}

impl Size {
    /// `None` for a picture with no pixels, or more than [`MOST_PIXELS_ACROSS`] either way.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 || width > MOST_PIXELS_ACROSS || height > MOST_PIXELS_ACROSS {
            return None;
        }
        Some(Self { width, height })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// How many pixels: one value of brightness each.
    #[must_use]
    pub const fn pixels(self) -> usize {
        (self.width as usize).saturating_mul(self.height as usize)
    }

    /// The size of each plane of colour: half as many each way, rounded up.
    #[must_use]
    pub const fn colour(self) -> (usize, usize) {
        (
            self.width.div_ceil(2) as usize,
            self.height.div_ceil(2) as usize,
        )
    }
}

/// Brightness and colour as video keeps them: 16 is black and 235 white; 128 is no
/// colour.
pub const BLACK: u8 = 16;
pub const WHITE: u8 = 235;
pub const NO_COLOUR: u8 = 128;

/// One picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    size: Size,
    /// Brightness, a row after a row.
    luma: Vec<u8>,
    /// Colour towards blue and towards red, one value for each two by two pixels.
    blue: Vec<u8>,
    red: Vec<u8>,
}

/// The planes of a picture, to be written into.
#[derive(Debug)]
pub struct Planes<'a> {
    pub luma: &'a mut [u8],
    pub blue: &'a mut [u8],
    pub red: &'a mut [u8],
}

impl Picture {
    /// A black picture.
    #[must_use]
    pub fn black(size: Size) -> Self {
        let (across, down) = size.colour();
        let colour = across.saturating_mul(down);
        Self {
            size,
            luma: vec![BLACK; size.pixels()],
            blue: vec![NO_COLOUR; colour],
            red: vec![NO_COLOUR; colour],
        }
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    #[must_use]
    pub fn luma(&self) -> &[u8] {
        &self.luma
    }

    /// The colour towards blue, and towards red.
    #[must_use]
    pub fn colour(&self) -> (&[u8], &[u8]) {
        (&self.blue, &self.red)
    }

    pub fn planes(&mut self) -> Planes<'_> {
        Planes {
            luma: &mut self.luma,
            blue: &mut self.blue,
            red: &mut self.red,
        }
    }

    /// The picture as a screen wants it: red, green, blue and 255, for every pixel, a row
    /// after a row. `out` is emptied first and keeps its room from one call to the next.
    pub fn to_rgba(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(self.size.pixels().saturating_mul(4));
        let width = self.size.width as usize;
        let (across, _) = self.size.colour();
        for (row, line) in self.luma.chunks_exact(width).enumerate() {
            let colours = (row / 2).saturating_mul(across);
            for (column, luma) in line.iter().enumerate() {
                let at = colours.saturating_add(column / 2);
                let blue = self.blue.get(at).copied().unwrap_or(NO_COLOUR);
                let red = self.red.get(at).copied().unwrap_or(NO_COLOUR);
                out.extend_from_slice(&rgba(*luma, blue, red));
            }
        }
    }
}

/// One pixel for a screen, by the BT.601 numbers in whole-number form.
fn rgba(luma: u8, blue: u8, red: u8) -> [u8; 4] {
    let bright = (i32::from(luma) - 16) * 298 + 128;
    let (blue, red) = (i32::from(blue) - 128, i32::from(red) - 128);
    let part = |value: i32| u8::try_from((value >> 8).clamp(0, 255)).unwrap_or(u8::MAX);
    [
        part(bright + 409 * red),
        part(bright - 100 * blue - 208 * red),
        part(bright + 516 * blue),
        255,
    ]
}

/// A moving picture for a bot or a test: grey bars that slide, and a block of the
/// speaker's own colour that crosses them. The same every time for the same speaker.
#[derive(Debug, Clone)]
pub struct Pattern {
    size: Size,
    speaker: u32,
    tick: u32,
}

impl Pattern {
    #[must_use]
    pub const fn new(size: Size, speaker: u32) -> Self {
        Self {
            size,
            speaker,
            tick: 0,
        }
    }

    /// The colour of `speaker`'s block, towards blue and towards red: one of sixteen
    /// that are far apart.
    #[must_use]
    pub fn colour_of(speaker: u32) -> (u8, u8) {
        let step = |n: u32| u8::try_from(40 + (n % 4) * 58).unwrap_or(NO_COLOUR);
        (step(speaker), step(speaker / 4))
    }

    /// The next picture.
    pub fn next_picture(&mut self) -> Picture {
        let mut picture = Picture::black(self.size);
        let (width, height) = (self.size.width as usize, self.size.height as usize);
        let tick = self.tick as usize;
        self.tick = self.tick.wrapping_add(1);

        // Bars a sixteenth of the width across, sliding two pixels a picture.
        let bar = (width / 16).max(1);
        let planes = picture.planes();
        for line in planes.luma.chunks_exact_mut(width) {
            for (column, luma) in line.iter_mut().enumerate() {
                let light = (column.saturating_add(tick.saturating_mul(2)) / bar) % 2 == 0;
                *luma = if light { 150 } else { 70 };
            }
        }

        // The block: a quarter of the height, crossing in about four seconds at 30 a
        // second, in the middle of the picture.
        let side = (height / 4).max(2);
        let room = width.saturating_sub(side).max(1);
        let left = tick.saturating_mul(room) / 120 % room;
        let top = height.saturating_sub(side) / 2;
        for line in planes.luma.chunks_exact_mut(width).skip(top).take(side) {
            for luma in line.iter_mut().skip(left).take(side) {
                *luma = 200;
            }
        }
        let (across, _) = self.size.colour();
        let (blue, red) = Self::colour_of(self.speaker);
        let rows = (top / 2)..(top.saturating_add(side)).div_ceil(2);
        let columns = (left / 2)..(left.saturating_add(side)).div_ceil(2);
        for (plane, colour) in [(planes.blue, blue), (planes.red, red)] {
            for line in plane
                .chunks_exact_mut(across)
                .skip(rows.start)
                .take(rows.len())
            {
                for value in line.iter_mut().skip(columns.start).take(columns.len()) {
                    *value = colour;
                }
            }
        }
        picture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Size {
        Size::new(width, height).unwrap()
    }

    #[test]
    fn a_size_has_pixels_and_is_not_too_large() {
        assert_eq!(Size::new(0, 10), None);
        assert_eq!(Size::new(10, 0), None);
        assert_eq!(Size::new(MOST_PIXELS_ACROSS + 1, 10), None);
        let odd = size(5, 3);
        assert_eq!((odd.width(), odd.height(), odd.pixels()), (5, 3, 15));
        // Colour for every two by two, with the odd edge counted.
        assert_eq!(odd.colour(), (3, 2));
    }

    #[test]
    fn black_white_and_colours_come_out_as_a_screen_wants_them() {
        let mut picture = Picture::black(size(2, 2));
        let mut out = Vec::new();
        picture.to_rgba(&mut out);
        assert_eq!(out, [0, 0, 0, 255].repeat(4));

        picture.planes().luma.fill(WHITE);
        picture.to_rgba(&mut out);
        assert_eq!(out, [255, 255, 255, 255].repeat(4));

        // Red, as video keeps it: brightness 81, blue 90, red 240.
        let planes = picture.planes();
        planes.luma.fill(81);
        planes.blue.fill(90);
        planes.red.fill(240);
        picture.to_rgba(&mut out);
        assert_eq!(out, [255, 0, 0, 255].repeat(4));
    }

    #[test]
    fn each_two_by_two_takes_its_own_colour_and_an_odd_edge_is_covered() {
        let mut picture = Picture::black(size(3, 3));
        let planes = picture.planes();
        planes.luma.fill(81);
        // Four values of colour for nine pixels; only the first is red.
        planes.blue[0] = 90;
        planes.red[0] = 240;
        let mut out = Vec::new();
        picture.to_rgba(&mut out);
        assert_eq!(out.len(), 9 * 4);
        let pixel = |row: usize, column: usize| &out[(row * 3 + column) * 4..][..3];
        for (row, column) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            assert_eq!(pixel(row, column), [255, 0, 0]);
        }
        // The odd column and the odd row are grey, from their own values.
        assert_eq!(pixel(0, 2), pixel(2, 2));
        assert_ne!(pixel(0, 2), [255, 0, 0]);
    }

    #[test]
    fn a_pattern_moves_and_is_the_same_every_time_for_the_same_speaker() {
        let mut first = Pattern::new(size(64, 36), 3);
        let mut again = Pattern::new(size(64, 36), 3);
        let (a, b) = (first.next_picture(), first.next_picture());
        assert_ne!(a, b);
        assert_eq!((again.next_picture(), again.next_picture()), (a.clone(), b));
        // Another speaker's is in another colour.
        let other = Pattern::new(size(64, 36), 4).next_picture();
        assert_ne!(other.colour(), a.colour());
        assert_ne!(Pattern::colour_of(3), Pattern::colour_of(4));
        // The block is in the speaker's colour, and nothing else is coloured.
        let (blue, red) = Pattern::colour_of(3);
        assert!(a.colour().0.iter().all(|v| *v == blue || *v == NO_COLOUR));
        assert!(a.colour().1.contains(&red));
    }

    #[test]
    fn a_pattern_can_be_as_small_as_a_pixel() {
        let mut tiny = Pattern::new(size(1, 1), 0);
        for _ in 0..200 {
            let picture = tiny.next_picture();
            assert_eq!(picture.luma().len(), 1);
        }
    }
}
