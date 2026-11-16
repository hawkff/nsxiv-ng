/// Gamma and contrast as one table that each color channel maps through. It is built with
/// Imlib2's color modifier formulas, truncation included, so opaque frames match nsxiv's:
/// gamma first, then contrast. Unlike Imlib2, which runs alpha through the table too, alpha
/// stays as decoded: gamma and contrast change colors, never what an image covers.
pub struct Colors {
    table: [u8; 256],
}

impl Colors {
    /// The table for the viewer's gamma and contrast values, or None when both are 1, the
    /// viewer's 0 steps, which leave pixels as they are.
    pub fn new(gamma: f64, contrast: f64) -> Option<Colors> {
        if gamma == 1.0 && contrast == 1.0 {
            return None;
        }
        let mut table: [u8; 256] = std::array::from_fn(|i| i as u8);
        if gamma != 1.0 {
            let exponent = 1.0 / gamma.max(0.01);
            for entry in &mut table {
                let level = (f64::from(*entry) / 255.0).powf(exponent) * 255.0;
                *entry = (level as i32).clamp(0, 255) as u8;
            }
        }
        if contrast != 1.0 {
            for entry in &mut table {
                let level = (((f64::from(*entry) - 127.0) * contrast) as i32).saturating_add(127);
                *entry = level.clamp(0, 255) as u8;
            }
        }
        Some(Colors { table })
    }

    /// Maps the color channels of straight-alpha RGBA pixels, and keeps alpha.
    pub fn apply(&self, rgba: &mut [u8]) {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            for channel in &mut pixel[..3] {
                *channel = self.table[usize::from(*channel)];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Colors;

    #[test]
    fn alpha_stays_as_decoded() {
        let pixels: Vec<u8> = (0..=255).flat_map(|a| [a, 255 - a, 128, a]).collect();
        let alpha = |rgba: &[u8]| rgba.iter().skip(3).step_by(4).copied().collect::<Vec<_>>();
        // gamma and contrast eight steps each way, alone and together
        for (gamma, contrast) in [
            (3.25, 1.0),
            (0.75, 1.0),
            (1.0, 1.75),
            (1.0, 0.75),
            (3.25, 0.75),
        ] {
            let mut frame = pixels.clone();
            Colors::new(gamma, contrast).unwrap().apply(&mut frame);
            assert_eq!(alpha(&frame), alpha(&pixels), "{gamma} {contrast}");
        }
    }

    #[test]
    fn gamma_comes_before_contrast() {
        // gamma 3.25 takes 64 to 166 and contrast 0.75 then to 156, where the other order would
        // end at 178; 200 ends at 208 rather than 229, and 0 at 32 either way
        let mut levels = vec![64, 200, 0, 255];
        Colors::new(3.25, 0.75).unwrap().apply(&mut levels);
        assert_eq!(levels, [156, 208, 32, 255]);
    }

    #[test]
    fn a_contrast_past_the_viewer_s_range_saturates() {
        let mut levels = vec![0, 128, 255, 255];
        Colors::new(1.0, f64::INFINITY).unwrap().apply(&mut levels);
        assert_eq!(levels, [0, 255, 255, 255]);
    }
}
