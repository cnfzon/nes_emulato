//! 最小的 PNG 編碼器（只寫、不壓縮），給 `nes-test screenshot` 存畫面用。
//!
//! 只用 zlib 的「stored」（不壓縮）區塊，所以不需要任何壓縮相關的依賴；
//! 256×240 的畫面約 240KB，除錯用途完全夠用。

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
/// deflate stored 區塊的最大長度。
const MAX_STORED: usize = 65_535;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend(data);
    out.extend(&body);
    out.extend(crc32(&body).to_be_bytes());
}

/// 把 `width × height` 的 RGBA8 畫面編成 PNG。
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize);

    // 每列前面加一個 filter byte（0 = None）。
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks_exact(width as usize * 4) {
        raw.push(0);
        raw.extend(row);
    }

    // zlib：0x78 0x01 標頭 + stored 區塊 + adler32。
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(MAX_STORED).peekable();
    while let Some(block) = blocks.next() {
        let last = blocks.peek().is_none();
        zlib.push(u8::from(last));
        zlib.extend((block.len() as u16).to_le_bytes());
        zlib.extend((!(block.len() as u16)).to_le_bytes());
        zlib.extend(block);
    }
    zlib.extend(adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8-bit、RGBA、無壓縮方法變化/filter/interlace

    let mut out = SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// 把畫面放大 `scale` 倍（最近鄰）。
pub fn upscale(width: usize, height: usize, rgba: &[u8], scale: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * scale * scale * 4);
    for row in rgba.chunks_exact(width * 4) {
        let mut scaled_row = Vec::with_capacity(width * scale * 4);
        for pixel in row.as_chunks::<4>().0 {
            for _ in 0..scale {
                scaled_row.extend(pixel);
            }
        }
        for _ in 0..scale {
            out.extend(&scaled_row);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_values() {
        // PNG 規格裡 IEND chunk 的固定 CRC。
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_known_value() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn encoded_png_has_valid_structure_and_recoverable_pixels() {
        let (w, h) = (3u32, 2u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| i as u8).collect();
        let png = encode_rgba(w, h, &rgba);

        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), w);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), h);

        // 找 IDAT，拆開 stored 區塊，還原原始（含 filter byte 的）資料。
        let idat_pos = png.windows(4).position(|x| x == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[idat_pos - 4..idat_pos].try_into().unwrap()) as usize;
        let zlib = &png[idat_pos + 4..idat_pos + 4 + len];
        assert_eq!(&zlib[..2], &[0x78, 0x01]);
        let mut raw = Vec::new();
        let mut i = 2;
        loop {
            let last = zlib[i] & 1 == 1;
            let n = u16::from_le_bytes([zlib[i + 1], zlib[i + 2]]) as usize;
            raw.extend(&zlib[i + 5..i + 5 + n]);
            i += 5 + n;
            if last {
                break;
            }
        }
        let expected: Vec<u8> = rgba
            .chunks_exact(w as usize * 4)
            .flat_map(|row| std::iter::once(0).chain(row.iter().copied()))
            .collect();
        assert_eq!(raw, expected);
        assert_eq!(&zlib[i..i + 4], &adler32(&raw).to_be_bytes());
        assert!(
            png.ends_with(&[0xAE, 0x42, 0x60, 0x82]),
            "以 IEND 的 CRC 結尾"
        );
    }

    #[test]
    fn upscale_repeats_pixels() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8];
        let out = upscale(2, 1, &rgba, 2);
        assert_eq!(out.len(), 2 * 2 * 2 * 4);
        assert_eq!(&out[..8], &[1, 2, 3, 4, 1, 2, 3, 4]);
        assert_eq!(&out[16..24], &[1, 2, 3, 4, 1, 2, 3, 4], "第二列重複第一列");
    }
}
