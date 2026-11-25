pub const BLOCK_BYTES: usize = 16;
pub const BLOCK_TEXELS: u32 = 4;
pub const RGBA_BYTES: usize = 4;

fn rgb565(value: u16) -> [u8; 3] {
    let r = ((value >> 11) & 0x1f) as u8;
    let g = ((value >> 5) & 0x3f) as u8;
    let b = (value & 0x1f) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    ]
}

fn colour_table(c0: u16, c1: u16) -> [[u8; 4]; 4] {
    let a = rgb565(c0);
    let b = rgb565(c1);
    let mut table = [[0u8; 4]; 4];
    table[0] = [a[0], a[1], a[2], 255];
    table[1] = [b[0], b[1], b[2], 255];
    if c0 > c1 {
        for i in 0..3 {
            table[2][i] = ((2 * u16::from(a[i]) + u16::from(b[i])) / 3) as u8;
            table[3][i] = ((u16::from(a[i]) + 2 * u16::from(b[i])) / 3) as u8;
        }
        table[2][3] = 255;
        table[3][3] = 255;
    } else {
        for i in 0..3 {
            table[2][i] = ((u16::from(a[i]) + u16::from(b[i])) / 2) as u8;
        }
        table[2][3] = 255;
    }
    table
}

fn alpha_table(a0: u8, a1: u8) -> [u8; 8] {
    let mut table = [0u8; 8];
    table[0] = a0;
    table[1] = a1;
    if a0 > a1 {
        for i in 2..8u32 {
            table[i as usize] = (((8 - i) * u32::from(a0) + (i - 1) * u32::from(a1)) / 7) as u8;
        }
    } else {
        for i in 2..6u32 {
            table[i as usize] = (((6 - i) * u32::from(a0) + (i - 1) * u32::from(a1)) / 5) as u8;
        }
        table[6] = 0;
        table[7] = 255;
    }
    table
}

pub fn decode_bc3_block(block: &[u8; BLOCK_BYTES]) -> [u8; 64] {
    let alpha = alpha_table(block[0], block[1]);
    let c0 = u16::from_le_bytes([block[8], block[9]]);
    let c1 = u16::from_le_bytes([block[10], block[11]]);
    let colour = colour_table(c0, c1);
    let mut alpha_bytes = [0u8; 8];
    alpha_bytes[..6].copy_from_slice(&block[2..8]);
    let alpha_indices = u64::from_le_bytes(alpha_bytes);
    let colour_indices = u32::from_le_bytes(block[12..16].try_into().unwrap_or([0; 4]));
    let mut out = [0u8; 64];
    for pixel in 0..16usize {
        let a = alpha[((alpha_indices >> (3 * pixel)) & 7) as usize];
        let c = colour[((colour_indices >> (2 * pixel)) & 3) as usize];
        out[pixel * 4..pixel * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], a]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(
        a0: u8,
        a1: u8,
        alpha_indices: [u8; 16],
        c0: u16,
        c1: u16,
        colour_indices: [u8; 16],
    ) -> [u8; 16] {
        let mut alpha = 0u64;
        let mut colour = 0u32;
        for (pixel, (&a, &c)) in alpha_indices.iter().zip(&colour_indices).enumerate() {
            alpha |= u64::from(a & 7) << (3 * pixel);
            colour |= u32::from(c & 3) << (2 * pixel);
        }
        let mut block = [0u8; 16];
        block[0] = a0;
        block[1] = a1;
        block[2..8].copy_from_slice(&alpha.to_le_bytes()[..6]);
        block[8..10].copy_from_slice(&c0.to_le_bytes());
        block[10..12].copy_from_slice(&c1.to_le_bytes());
        block[12..16].copy_from_slice(&colour.to_le_bytes());
        block
    }

    fn solid(a0: u8, a1: u8, c0: u16, c1: u16) -> [u8; 16] {
        block(a0, a1, [0; 16], c0, c1, [0; 16])
    }

    #[test]
    fn alpha_endpoints_and_interpolation() {
        let indices = std::array::from_fn(|i| i as u8);
        let high = block(255, 0, indices, 0xffff, 0xffff, [0; 16]);
        let decoded = decode_bc3_block(&high);
        let expected = [255u8, 0, 218, 182, 145, 109, 72, 36];
        for pixel in 0..16 {
            assert_eq!(decoded[pixel * 4 + 3], expected[pixel % 8], "pixel {pixel}");
        }
        let low = block(0, 255, indices, 0xffff, 0xffff, [0; 16]);
        let decoded = decode_bc3_block(&low);
        let expected = [0u8, 255, 51, 102, 153, 204, 0, 255];
        for pixel in 0..16 {
            assert_eq!(decoded[pixel * 4 + 3], expected[pixel % 8], "pixel {pixel}");
        }
    }

    #[test]
    fn colour_endpoints_and_interpolation() {
        let indices = std::array::from_fn(|i| i as u8);
        let red_blue = block(255, 255, [0; 16], 0xf800, 0x001f, indices);
        let decoded = decode_bc3_block(&red_blue);
        let expected = [
            [255u8, 0, 0, 255],
            [0, 0, 255, 255],
            [170, 0, 85, 255],
            [85, 0, 170, 255],
        ];
        for pixel in 0..16 {
            assert_eq!(
                decoded[pixel * 4..pixel * 4 + 4],
                expected[pixel % 4],
                "pixel {pixel}"
            );
        }
        let blue_red = block(255, 255, [0; 16], 0x001f, 0xf800, indices);
        let decoded = decode_bc3_block(&blue_red);
        assert_eq!(decoded[0..4], [0, 0, 255, 255]);
        assert_eq!(decoded[4..8], [255, 0, 0, 255]);
        assert_eq!(decoded[8..12], [127, 0, 127, 255]);
        assert_eq!(decoded[12..16], [0, 0, 0, 255]);
    }

    #[test]
    fn six_bit_green_expands_to_full_range() {
        let decoded = decode_bc3_block(&solid(255, 255, 0x07e0, 0x07e0));
        assert_eq!(decoded[0..4], [0, 255, 0, 255]);
    }
}
