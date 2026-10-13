//! Renders the tray icon: battery percentage digits over a level bar.

use crate::device::Status;

const SIZE: usize = 32;

/// 3x5 bitmap digits, row-major, MSB = left pixel.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

type Rgba = [u8; 4];

struct Canvas(Vec<u8>);

impl Canvas {
    fn new() -> Self {
        Canvas(vec![0; SIZE * SIZE * 4])
    }
    fn put(&mut self, x: usize, y: usize, c: Rgba) {
        if x < SIZE && y < SIZE {
            let i = (y * SIZE + x) * 4;
            self.0[i..i + 4].copy_from_slice(&c);
        }
    }
    fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgba) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, c);
            }
        }
    }
    /// Draw glyph rows (3 wide) scaled by `s`, with a 1px dark outline so it
    /// reads on both light and dark taskbars.
    fn glyph(&mut self, rows: &[u8; 5], x: usize, y: usize, s: usize, c: Rgba, outline: Rgba) {
        for pass in 0..2 {
            for (ry, row) in rows.iter().enumerate() {
                for rx in 0..3 {
                    if row & (0b100 >> rx) == 0 {
                        continue;
                    }
                    let (px, py) = (x + rx * s, y + ry * s);
                    if pass == 0 {
                        self.rect(px.saturating_sub(1), py.saturating_sub(1), s + 2, s + 2, outline);
                    } else {
                        self.rect(px, py, s, s, c);
                    }
                }
            }
        }
    }
}

/// `light_taskbar`: dark glyphs with a light halo instead of the reverse.
pub fn render(status: Option<&Status>, light_taskbar: bool) -> Vec<u8> {
    let mut cv = Canvas::new();
    let (ink, halo): (Rgba, Rgba) = if light_taskbar {
        ([28, 28, 28, 255], [255, 255, 255, 170])
    } else {
        ([255, 255, 255, 255], [0, 0, 0, 200])
    };
    const GREY: Rgba = [150, 150, 150, 255];

    let pct = status.filter(|s| s.online).and_then(|s| s.percent);
    let Some(p) = pct else {
        // No mouse / asleep: two dashes.
        cv.rect(4, 13, 10, 5, halo);
        cv.rect(18, 13, 10, 5, halo);
        cv.rect(5, 14, 8, 3, GREY);
        cv.rect(19, 14, 8, 3, GREY);
        return cv.0;
    };
    let st = status.unwrap();

    let color: Rgba = if st.charging || st.full {
        [80, 200, 255, 255]
    } else if p >= 50 {
        [90, 230, 90, 255]
    } else if p >= 20 {
        [255, 200, 40, 255]
    } else {
        [255, 70, 60, 255]
    };

    let text = p.to_string();
    let digits: Vec<usize> = text.bytes().map(|b| (b - b'0') as usize).collect();
    let (scale, gap) = if digits.len() >= 3 { (3, 2) } else { (4, 3) };
    let w = digits.len() * 3 * scale + (digits.len() - 1) * gap;
    let mut x = (SIZE - w) / 2;
    for d in digits {
        cv.glyph(&DIGITS[d], x, 2, scale, ink, halo);
        x += 3 * scale + gap;
    }

    // Level bar along the bottom.
    cv.rect(1, 25, 30, 6, halo);
    cv.rect(2, 26, 28, 4, if light_taskbar { [200, 200, 200, 255] } else { [60, 60, 60, 255] });
    let fill = ((p as usize * 28) / 100).max(1);
    cv.rect(2, 26, fill, 4, color);
    cv.0
}

pub const ICON_SIZE: u32 = SIZE as u32;
