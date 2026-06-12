/// Convert (z, x, y) to a PMTiles tile ID using the Hilbert curve.
///
/// PMTiles orders tiles by Hilbert curve index so that spatially adjacent
/// tiles are close together on disk, maximising page-cache locality.
/// This matches the reference implementation exactly.
pub fn tile_to_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    let base: u64 = (0..z as u64).map(|i| 4u64.pow(i as u32)).sum();
    let n = 1u32 << z;
    base + xy_to_hilbert(n, x, y)
}

fn xy_to_hilbert(n: u32, mut x: u32, mut y: u32) -> u64 {
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = (x & s > 0) as u32;
        let ry = (y & s > 0) as u32;
        d += (s as u64 * s as u64) * ((3 * rx) ^ ry) as u64;
        // Rotate quadrant
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z0_is_zero() {
        assert_eq!(tile_to_id(0, 0, 0), 0);
    }

    #[test]
    fn z1_ordering() {
        // z=1 base = 1; Hilbert order for 2x2 is (0,0)→0, (0,1)→1, (1,1)→2, (1,0)→3
        assert_eq!(tile_to_id(1, 0, 0), 1);
        assert_eq!(tile_to_id(1, 0, 1), 2);
        assert_eq!(tile_to_id(1, 1, 1), 3);
        assert_eq!(tile_to_id(1, 1, 0), 4);
    }
}
