use image::{ImageBuffer, Rgba, RgbaImage};

use crate::model::{Layout, PixelSize, Point, Rect};

pub const MARKER_INSET: f64 = 0.10;
pub const MARKER_SIDE: f64 = 0.026;
const GLYPHS: [[&str; 7]; 7] = [
    [
        "01110", "10001", "10001", "11111", "10001", "10001", "10001",
    ],
    [
        "11110", "10001", "10001", "11110", "10001", "10001", "11110",
    ],
    [
        "01111", "10000", "10000", "10000", "10000", "10000", "01111",
    ],
    [
        "11110", "10001", "10001", "10001", "10001", "10001", "11110",
    ],
    [
        "11111", "10000", "10000", "11110", "10000", "10000", "11111",
    ],
    [
        "11111", "10000", "10000", "11110", "10000", "10000", "10000",
    ],
    [
        "01111", "10000", "10000", "10111", "10001", "10001", "01110",
    ],
];

#[derive(Clone, Debug)]
pub struct MarkerAnchor {
    pub normalized_center: Point,
    pub normalized_side: f64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct TilePattern {
    pub letter: char,
    pub source_rect: Rect,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct PatternFrame {
    pub image: RgbaImage,
    pub size: PixelSize,
    pub tiles: Vec<TilePattern>,
}

impl PatternFrame {
    pub fn bgra_top_down(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.image.as_raw().len());
        for pixel in self.image.pixels() {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        bytes
    }
}

pub fn render_pattern(size: PixelSize, layout: Layout) -> Result<PatternFrame, String> {
    if size.width == 0 || size.height == 0 {
        return Err("pattern size is empty".into());
    }
    if layout.card_count() == 0 || layout.card_count() > 7 {
        return Err("pattern supports between one and seven cards".into());
    }
    if !size.width.is_multiple_of(layout.cols as u32)
        || !size.height.is_multiple_of(layout.rows as u32)
    {
        return Err("screen cannot be split into equal mosaic tiles".into());
    }

    let mut image: RgbaImage =
        ImageBuffer::from_pixel(size.width, size.height, Rgba([0, 0, 0, 255]));
    let tile_width = size.width / layout.cols as u32;
    let tile_height = size.height / layout.rows as u32;
    let mut tiles = Vec::with_capacity(layout.card_count());

    for row in 0..layout.rows {
        for col in 0..layout.cols {
            let index = layout.tile_index(row, col);
            let origin_x = (col as u32 * tile_width) as f64;
            let origin_y = (row as u32 * tile_height) as f64;
            let source_rect = Rect::new(origin_x, origin_y, tile_width as f64, tile_height as f64);
            let marker_anchors = marker_anchors();

            for anchor in &marker_anchors {
                let center = Point::new(
                    origin_x + anchor.normalized_center.x * tile_width as f64,
                    origin_y + anchor.normalized_center.y * tile_height as f64,
                );
                draw_square_marker(
                    &mut image,
                    center,
                    anchor.normalized_side * tile_width.min(tile_height) as f64,
                );
            }

            let glyph_rect = glyph_rect_for_tile(origin_x, origin_y, tile_width, tile_height);
            draw_block_glyph(&mut image, glyph_rect, index);
            tiles.push(TilePattern {
                letter: letter_for_index(index),
                source_rect,
            });
        }
    }

    Ok(PatternFrame { image, size, tiles })
}

pub fn marker_anchors() -> [MarkerAnchor; 4] {
    let positions = [
        Point::new(MARKER_INSET, MARKER_INSET),
        Point::new(1.0 - MARKER_INSET, MARKER_INSET),
        Point::new(MARKER_INSET, 1.0 - MARKER_INSET),
        Point::new(1.0 - MARKER_INSET, 1.0 - MARKER_INSET),
    ];
    positions.map(|normalized_center| MarkerAnchor {
        normalized_center,
        normalized_side: MARKER_SIDE,
    })
}

pub fn glyph_rect_for_tile(
    origin_x: f64,
    origin_y: f64,
    tile_width: u32,
    tile_height: u32,
) -> Rect {
    let max_width = tile_width as f64 * 0.66;
    let max_height = tile_height as f64 * 0.66;
    let width = max_width.min(max_height * 5.0 / 7.0);
    let height = width * 7.0 / 5.0;
    Rect::new(
        origin_x + (tile_width as f64 - width) * 0.5,
        origin_y + (tile_height as f64 - height) * 0.5,
        width,
        height,
    )
}

pub fn glyph_template(index: usize) -> &'static [&'static str; 7] {
    &GLYPHS[index.min(GLYPHS.len() - 1)]
}

pub fn letter_for_index(index: usize) -> char {
    char::from(b'A' + (index.min(6) as u8))
}

pub fn index_for_letter(letter: char) -> Option<usize> {
    let uppercase = letter.to_ascii_uppercase();
    if ('A'..='G').contains(&uppercase) {
        Some((uppercase as u8 - b'A') as usize)
    } else {
        None
    }
}

fn draw_square_marker(image: &mut RgbaImage, center: Point, side: f64) {
    let half = (side * 0.5).round().max(1.0) as i32;
    let cx = center.x.round() as i32;
    let cy = center.y.round() as i32;
    for y in cy - half..=cy + half {
        for x in cx - half..=cx + half {
            put_white(image, x, y);
        }
    }
}

fn draw_block_glyph(image: &mut RgbaImage, rect: Rect, glyph_index: usize) {
    let glyph = glyph_template(glyph_index);
    let pitch_x = rect.width / 5.0;
    let pitch_y = rect.height / 7.0;
    let gap_x = (pitch_x * 0.08).max(1.0);
    let gap_y = (pitch_y * 0.08).max(1.0);

    for (row, line) in glyph.iter().enumerate() {
        for (col, bit) in line.bytes().enumerate() {
            if bit != b'1' {
                continue;
            }
            let left = rect.x + col as f64 * pitch_x + gap_x * 0.5;
            let top = rect.y + row as f64 * pitch_y + gap_y * 0.5;
            let right = rect.x + (col + 1) as f64 * pitch_x - gap_x * 0.5;
            let bottom = rect.y + (row + 1) as f64 * pitch_y - gap_y * 0.5;
            fill_rect(image, left, top, right, bottom);
        }
    }
}

fn fill_rect(image: &mut RgbaImage, left: f64, top: f64, right: f64, bottom: f64) {
    let x0 = left.round().max(0.0) as i32;
    let y0 = top.round().max(0.0) as i32;
    let x1 = right.round().max(0.0) as i32;
    let y1 = bottom.round().max(0.0) as i32;
    for y in y0..y1 {
        for x in x0..x1 {
            put_white(image, x, y);
        }
    }
}

fn put_white(image: &mut RgbaImage, x: i32, y: i32) {
    if x >= 0 && y >= 0 && x < image.width() as i32 && y < image.height() as i32 {
        image.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 255]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_letter_in_mosaic_order() {
        let pattern = render_pattern(PixelSize::new(800, 600), Layout::new(2, 2)).unwrap();
        assert_eq!(
            pattern
                .tiles
                .iter()
                .map(|tile| tile.letter)
                .collect::<String>(),
            "ABCD"
        );
        assert_eq!(pattern.tiles[3].source_rect.x, 400.0);
        assert_eq!(pattern.tiles[3].source_rect.y, 300.0);
    }

    #[test]
    fn rejects_non_integral_layout() {
        let result = render_pattern(PixelSize::new(1001, 700), Layout::new(2, 2));
        assert!(matches!(result, Err(error) if error.contains("equal mosaic")));
    }

    #[test]
    fn bgra_conversion_is_channel_swapped() {
        let image = ImageBuffer::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let frame = PatternFrame {
            image,
            size: PixelSize::new(1, 1),
            tiles: vec![],
        };
        assert_eq!(frame.bgra_top_down(), vec![30, 20, 10, 255]);
    }
}
