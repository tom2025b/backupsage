//! Perceptual hash `sage-dct-v1` — FROZEN.
//!
//! The 64-bit DCT hash stored in every index. Any change to this algorithm
//! (resize kernel, grayscale weights, DCT windowing, median rule) silently
//! invalidates every hash ever written, so the recipe is pinned by golden
//! tests below and versioned in index meta as `phash_algo`. Do not "improve"
//! it; a new recipe must ship as `sage-dct-v2` alongside a re-index story.
//!
//! Recipe (mirrors the semantics of Python `imagehash.phash`, not its bits —
//! bit-for-bit parity is impossible across image stacks and is a non-goal):
//! grayscale → 32×32 Lanczos3 resize → 2D DCT-II → top-left 8×8 block
//! (DC included, as imagehash does) → median threshold → 64 bits.

use image::DynamicImage;

pub const PHASH_ALGO: &str = "sage-dct-v1";

const N: usize = 32;
const BLOCK: usize = 8;

/// Hamming distance between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Trivial hashes come from flat / near-monochrome images (a flat image
/// yields the DC-only hash 0x8000…); near-dup search excludes them because
/// they would weld thousands of unrelated images (black frames, white scans)
/// into one giant group. Popcount extremes catch all-0/all-1, DC-only and
/// its inverse.
pub fn is_trivial(h: u64) -> bool {
    h.count_ones() <= 1 || h.count_ones() >= 63
}

/// Compute the 64-bit `sage-dct-v1` hash of a decoded image.
pub fn phash(img: &DynamicImage) -> u64 {
    // Grayscale first so the (deterministic) luma conversion happens at full
    // resolution, then Lanczos3 down to 32×32.
    let small = image::imageops::resize(
        &img.to_luma8(),
        N as u32,
        N as u32,
        image::imageops::FilterType::Lanczos3,
    );

    let mut pixels = [[0f64; N]; N];
    for (x, y, p) in small.enumerate_pixels() {
        pixels[y as usize][x as usize] = p.0[0] as f64;
    }

    let dct = dct2_2d(&pixels);

    // Top-left 8×8 low-frequency block, DC included.
    let mut coefs = [0f64; BLOCK * BLOCK];
    for (i, c) in coefs.iter_mut().enumerate() {
        *c = dct[i / BLOCK][i % BLOCK];
    }

    // Median = mean of the two middle sorted values (64 is even).
    let mut sorted = coefs;
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("DCT of finite pixels is finite"));
    let median = (sorted[BLOCK * BLOCK / 2 - 1] + sorted[BLOCK * BLOCK / 2]) / 2.0;

    // Scale-relative epsilon: on flat images the 63 non-DC coefficients are
    // float noise around zero, and a bare `> median` would set random bits.
    // Real photos have |coef - median| orders of magnitude above this.
    let scale = coefs.iter().fold(0f64, |m, c| m.max(c.abs()));
    let eps = 1e-9 * (scale + 1.0);

    let mut hash = 0u64;
    for (i, &c) in coefs.iter().enumerate() {
        if c > median + eps {
            hash |= 1 << (63 - i);
        }
    }
    hash
}

/// Separable 2D DCT-II over an N×N block using a precomputed cosine table:
/// `table[k][n] = cos(PI / N * (n + 0.5) * k)`. Unnormalised — the median
/// threshold makes scale factors irrelevant.
fn dct2_2d(input: &[[f64; N]; N]) -> [[f64; N]; N] {
    let table = cosine_table();

    // Rows.
    let mut rows = [[0f64; N]; N];
    for y in 0..N {
        for (k, row_k) in table.iter().enumerate() {
            let mut sum = 0.0;
            for n in 0..N {
                sum += input[y][n] * row_k[n];
            }
            rows[y][k] = sum;
        }
    }
    // Columns.
    let mut out = [[0f64; N]; N];
    for x in 0..N {
        for (k, col_k) in table.iter().enumerate() {
            let mut sum = 0.0;
            for n in 0..N {
                sum += rows[n][x] * col_k[n];
            }
            out[k][x] = sum;
        }
    }
    out
}

fn cosine_table() -> [[f64; N]; N] {
    let mut t = [[0f64; N]; N];
    for (k, row) in t.iter_mut().enumerate() {
        for (n, v) in row.iter_mut().enumerate() {
            *v = (std::f64::consts::PI / N as f64 * (n as f64 + 0.5) * k as f64).cos();
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma, Rgb};

    /// Deterministic synthetic photo-like image: smooth gradients + a few
    /// hard shapes, so the DCT spectrum is non-trivial.
    fn synthetic(seed: u64, w: u32, h: u32) -> DynamicImage {
        let img = ImageBuffer::from_fn(w, h, |x, y| {
            let s = seed as u32;
            let r = ((x * 7 + y * 3 + s * 11) % 256) as u8;
            let g = ((x * x / (w / 4).max(1) + y * 2 + s * 5) % 256) as u8;
            let b = if (x / 16 + y / 16 + s).is_multiple_of(2) {
                200
            } else {
                40
            };
            Rgb([r, g, b])
        });
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn identical_images_distance_zero() {
        let a = synthetic(1, 320, 240);
        let b = synthetic(1, 320, 240);
        assert_eq!(hamming(phash(&a), phash(&b)), 0);
    }

    #[test]
    fn brightness_shift_stays_close() {
        let a = synthetic(2, 320, 240);
        let brightened = DynamicImage::ImageRgb8(ImageBuffer::from_fn(320, 240, |x, y| {
            let p = a.to_rgb8().get_pixel(x, y).0;
            Rgb([
                p[0].saturating_add(10),
                p[1].saturating_add(10),
                p[2].saturating_add(10),
            ])
        }));
        let d = hamming(phash(&a), phash(&brightened));
        assert!(d <= 6, "brightness shift moved hash by {d} bits");
    }

    #[test]
    fn resized_copy_stays_close() {
        // The classic near-duplicate: same photo exported at another size.
        let a = synthetic(3, 640, 480);
        let smaller = a.resize_exact(320, 240, image::imageops::FilterType::Triangle);
        let d = hamming(phash(&a), phash(&smaller));
        assert!(d <= 6, "resized copy moved hash by {d} bits");
    }

    #[test]
    fn different_images_are_far_apart() {
        let d = hamming(
            phash(&synthetic(4, 320, 240)),
            phash(&synthetic(9, 320, 240)),
        );
        assert!(d >= 16, "distinct images only {d} bits apart");
    }

    #[test]
    fn solid_color_is_trivial() {
        let flat = DynamicImage::ImageLuma8(ImageBuffer::from_pixel(64, 64, Luma([128u8])));
        assert!(is_trivial(phash(&flat)));
    }

    /// FROZEN golden vectors. If either assertion ever fails, the algorithm
    /// changed and every stored hash is invalid — that is a release-blocking
    /// event, not a test to update casually. (Values captured from the first
    /// released implementation of sage-dct-v1.)
    #[test]
    fn golden_vectors_frozen() {
        let g1 = phash(&synthetic(1, 320, 240));
        let g2 = phash(&synthetic(7, 512, 384));
        assert_eq!(
            (g1, g2),
            (GOLDEN_1, GOLDEN_2),
            "sage-dct-v1 output changed: got ({g1:#018x}, {g2:#018x})"
        );
    }
    const GOLDEN_1: u64 = 0xf103f1033efcfc01;
    const GOLDEN_2: u64 = 0xdde055c057c003ff;
}
