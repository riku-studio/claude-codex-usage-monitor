//! Grayscale GDI coverage from black/white canvases, in DIB 0x00RRGGBB order.
pub fn transparent_pixel(black: u32, white: u32) -> u32 {
    let mut background = 0u32;
    let mut max_color = 0u32;
    for shift in [0, 8, 16] {
        let b = (black >> shift) & 255;
        let w = (white >> shift) & 255;
        background = background.max(w.saturating_sub(b));
        max_color = max_color.max(b);
    }
    let alpha = (255 - background).max(max_color).max(1);
    (alpha << 24) | (black & 0xffffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_glyph_edges_never_contain_white_or_colored_pixels() {
        for coverage in 0..=255u32 {
            let white_canvas = (255 - coverage) * 0x010101;
            let pixel = transparent_pixel(0, white_canvas);
            assert_eq!(pixel & 0xffffff, 0);
            assert_eq!(pixel >> 24, coverage.max(1));
        }
    }

    #[test]
    fn bars_keep_their_colors_and_background_remains_transparent() {
        assert_eq!(transparent_pixel(0xd97757, 0xd97757), 0xffd97757);
        assert_eq!(transparent_pixel(0, 0xffffff), 0x01000000);
    }
}
