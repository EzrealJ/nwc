//! DIB（Windows 位图）与 PNG 互转的纯逻辑（nwc-win 剪贴板使用；放 core 便于在任意平台跑测试）。
//!
//! 头布局（字节偏移；DWORD=4B、WORD=2B）：
//! `biSize@0 biWidth@4 biHeight@8 biPlanes@12(WORD) biBitCount@14(WORD) biCompression@16
//! biSizeImage@20 biClrUsed@32`；V4/V5 头追加掩码 `Red@40 Green@44 Blue@48 Alpha@52`、
//! `CSType@56 Endpoints@60..96 Gamma@96/100/104`；V5 再追加 `Intent@108 ProfileData@112 ProfileSize@116`。
//!
//! ⚠ biPlanes/biBitCount 是 **WORD**：曾按 DWORD 从 16/20/36 读，字段整体错位两字节，
//! 32bpp BI_BITFIELDS 的截图（Win+Shift+S）被误判为「3bpp 未知压缩」而拒收。
//! 另支持：头长以 biSize 为准（40/108/124 均可，CF_DIBV5 通道里常混有 40/108 字节头）；
//! V5 头后个别写入者附加 12 字节掩码三元组（以 biSizeImage 反推像素起点兼容）。

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

/// 位掩码提取并归一到 0..255
fn mask_extract(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let count = mask.count_ones();
    let raw = (v & mask) >> shift;
    if count >= 8 {
        (raw >> (count - 8)) as u8
    } else {
        ((raw.saturating_mul(255)) / ((1u32 << count) - 1).max(1)) as u8
    }
}

const COMP_NAMES: [(u32, &str); 6] = [
    (0, "BI_RGB"),
    (1, "BI_RLE8"),
    (2, "BI_RLE4"),
    (3, "BI_BITFIELDS"),
    (4, "BI_JPEG"),
    (5, "BI_PNG"),
];

/// DIB 头关键参数（诊断用）
pub fn describe(dib: &[u8]) -> String {
    if dib.len() < 4 {
        return "空数据".into();
    }
    let hdr = rd32(dib, 0).unwrap_or(0);
    let w = rd32(dib, 4).unwrap_or(0);
    let h = rd32(dib, 8).unwrap_or(0) as i32;
    let bpp = rd16(dib, 14).unwrap_or(0);
    let comp = rd32(dib, 16).unwrap_or(u32::MAX);
    let comp_name = COMP_NAMES.iter().find(|(v, _)| *v == comp).map(|(_, n)| *n).unwrap_or("未知");
    format!("头{hdr}B {w}x{} {bpp}bpp {comp_name} 共{}B", h.unsigned_abs(), dib.len())
}

/// DIB → PNG。支持 24/32bpp 的 BI_RGB / BI_BITFIELDS；自底向上/自顶向下。
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hdr_len = rd32(dib, 0)? as usize;
    if !(40..=dib.len()).contains(&hdr_len) {
        return None;
    }
    let width = rd32(dib, 4)? as i32;
    let width = width.unsigned_abs() as usize;
    let height_raw = rd32(dib, 8)? as i32;
    let (height, top_down) = if height_raw < 0 {
        (height_raw.unsigned_abs() as usize, true)
    } else {
        (height_raw as usize, false)
    };
    let planes = rd16(dib, 12)? as u32;
    let bpp = rd16(dib, 14)? as usize;
    let compression = rd32(dib, 16)?;
    let clr_used = rd32(dib, 32)?.min(256) as usize;
    if planes != 1 || (bpp != 24 && bpp != 32) || (compression != 0 && compression != 3) || width == 0 || height == 0 {
        return None;
    }
    // 掩码：BI_BITFIELDS 时 V4/V5 头在头内 40..52；40 字节 INFO 头则跟在头后（40/44/48）
    let (rm, gm, bm, am) = if compression == 3 {
        let (r, g, b, a) = if hdr_len >= 56 {
            (rd32(dib, 40)?, rd32(dib, 44)?, rd32(dib, 48)?, rd32(dib, 52).unwrap_or(0))
        } else {
            (rd32(dib, 40)?, rd32(dib, 44)?, rd32(dib, 48)?, 0)
        };
        if r | g | b == 0 {
            (0x00FF0000, 0x0000FF00, 0x000000FF, a) // 掩码全 0：回退默认 BGRA
        } else {
            (r, g, b, a)
        }
    } else if bpp == 32 {
        (0x00FF0000, 0x0000FF00, 0x000000FF, 0)
    } else {
        (0, 0, 0, 0) // 24bpp 固定 BGR 序
    };
    let mut px_off = hdr_len
        + if compression == 3 && hdr_len == 40 { 12 } else { 0 }
        + clr_used * 4;
    // V5 ICC profile 段（bV5ProfileData 自 DIB 起点偏移）可能占据像素前区域
    if hdr_len >= 124 {
        let p_off = rd32(dib, 112).unwrap_or(0) as usize;
        let p_size = rd32(dib, 116).unwrap_or(0) as usize;
        if p_size > 0 && p_off + p_size > px_off {
            px_off = p_off + p_size;
        }
    }
    let stride = (width * bpp / 8 + 3) & !3usize;
    let need = stride * height;
    // 个别写入者在 V5 头后附加 12 字节掩码三元组：以 biSizeImage 反推真实像素起点
    // （仅接受 [px_off, px_off+16] 内的修正，防 biSizeImage 为垃圾值）
    let size_img = rd32(dib, 20).unwrap_or(0) as usize;
    if size_img > 0 && size_img <= dib.len() {
        let implied = dib.len() - size_img;
        if implied >= px_off && implied <= px_off + 16 {
            px_off = implied;
        }
    }
    let px = dib.get(px_off..)?;
    if px.len() < need {
        return None;
    }
    let mut rgba: Vec<u8> = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let y = if top_down { row } else { height - 1 - row };
        let line = &px[y * stride..y * stride + width * bpp / 8];
        for x in 0..width {
            let p = &line[x * bpp / 8..x * bpp / 8 + bpp / 8];
            if bpp == 24 {
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            } else {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                let a = if am != 0 { mask_extract(v, am) } else { 255 };
                rgba.extend_from_slice(&[mask_extract(v, rm), mask_extract(v, gm), mask_extract(v, bm), a]);
            }
        }
    }
    encode_png(width as u32, height as u32, &rgba)
}

/// PNG → BITMAPV5HEADER(124B) + 自底向上 BGRA 像素。
/// 压缩用 BI_BITFIELDS + AlphaMask：BI_RGB 的第 4 字节按 Windows 惯例是 padding
/// （多数消费者不当 alpha 用），声明掩码才能保留透明度。
pub fn png_to_dibv5(png_bytes: &[u8]) -> Option<Vec<u8>> {
    let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = &buf[..info.buffer_size()];
    let stride = (w * 4 + 3) & !3usize;
    let mut dib = vec![0u8; 124 + stride * h];
    dib[0..4].copy_from_slice(&124u32.to_le_bytes()); // bV5Size
    dib[4..8].copy_from_slice(&(w as u32).to_le_bytes());
    dib[8..12].copy_from_slice(&(h as u32).to_le_bytes()); // 正数 = 自底向上
    dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // Planes
    dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // BitCount
    dib[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    dib[20..24].copy_from_slice(&((stride * h) as u32).to_le_bytes()); // SizeImage
    dib[32..36].copy_from_slice(&0u32.to_le_bytes()); // ClrUsed
    dib[40..44].copy_from_slice(&0x00FF0000u32.to_le_bytes()); // RedMask
    dib[44..48].copy_from_slice(&0x0000FF00u32.to_le_bytes()); // GreenMask
    dib[48..52].copy_from_slice(&0x000000FFu32.to_le_bytes()); // BlueMask
    dib[52..56].copy_from_slice(&0xFF000000u32.to_le_bytes()); // AlphaMask
    dib[56..60].copy_from_slice(&0x73524742u32.to_le_bytes()); // CSType = LCS_sRGB
    dib[108..112].copy_from_slice(&4u32.to_le_bytes()); // Intent = LCS_GM_IMAGES
    for row in 0..h {
        let src = &rgba[row * w * 4..(row + 1) * w * 4];
        let dst = &mut dib[124 + (h - 1 - row) * stride..124 + (h - 1 - row) * stride + w * 4];
        for x in 0..w {
            dst[x * 4] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4];
            dst[x * 4 + 3] = src[x * 4 + 3];
        }
    }
    Some(dib)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_rgba(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        (info.width, info.height, buf[..info.buffer_size()].to_vec())
    }

    // ---- 测试像素（RGBA 逻辑值）----
    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    /// 半透明黄
    const YELLOW: [u8; 4] = [255, 255, 0, 128];

    /// RGBA → BGRA（DIB 内存字节序）
    fn bgra(p: [u8; 4]) -> [u8; 4] {
        [p[2], p[1], p[0], p[3]]
    }

    fn px2(a: [u8; 4], b: [u8; 4]) -> [u8; 8] {
        let (a, b) = (bgra(a), bgra(b));
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&a);
        out[4..].copy_from_slice(&b);
        out
    }

    /// 顶行 [红, 绿]、底行 [蓝, 黄] 的 PNG（RGBA 行序即图像行序）
    fn sample_png() -> Vec<u8> {
        encode_png(2, 2, &[RED, GREEN, BLUE, YELLOW].concat()).unwrap()
    }

    #[test]
    fn v5_bitfields_32bpp() {
        // Win+Shift&S 场景：V5 头 + BI_BITFIELDS + 头后附加 12 字节掩码三元组（真实脏布局）
        let size_img = 2 * 4 * 2;
        let mut dib = vec![0u8; 124 + 12 + size_img];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2u32.to_le_bytes());
        dib[8..12].copy_from_slice(&2u32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        dib[20..24].copy_from_slice(&(size_img as u32).to_le_bytes());
        dib[40..44].copy_from_slice(&0xFF0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x00FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000FFu32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xFF000000u32.to_le_bytes());
        // 附加掩码三元组（脏布局，靠 biSizeImage 反推像素起点）
        dib[124..128].copy_from_slice(&0xFF0000u32.to_le_bytes());
        dib[128..132].copy_from_slice(&0x00FF00u32.to_le_bytes());
        dib[132..136].copy_from_slice(&0x0000FFu32.to_le_bytes());
        // 自底向上：物理行0 = 图像底行
        dib[136..144].copy_from_slice(&px2(BLUE, YELLOW));
        dib[144..152].copy_from_slice(&px2(RED, GREEN));

        let png = dib_to_png(&dib).expect("V5+BITFIELDS 应解码成功");
        let (w, h, rgba) = png_rgba(&png);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[0..8], &[RED, GREEN].concat());
        assert_eq!(&rgba[8..16], &[BLUE, YELLOW].concat());
    }

    #[test]
    fn info_header_bitfields() {
        // 40 字节 INFO 头 + 掩码三元组（CF_DIB 常见形态）
        let size_img = 2 * 4 * 2;
        let mut dib = vec![0u8; 40 + 12 + size_img];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2u32.to_le_bytes());
        dib[8..12].copy_from_slice(&2u32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        dib[20..24].copy_from_slice(&(size_img as u32).to_le_bytes());
        dib[40..44].copy_from_slice(&0xFF0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x00FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000FFu32.to_le_bytes());
        dib[52..60].copy_from_slice(&px2(BLUE, YELLOW));
        dib[60..68].copy_from_slice(&px2(RED, GREEN));

        let png = dib_to_png(&dib).expect("INFO+BITFIELDS 应解码成功");
        let (_, _, rgba) = png_rgba(&png);
        assert_eq!(&rgba[0..8], &[RED, GREEN].concat());
    }

    #[test]
    fn info_header_rgb_24bpp_top_down() {
        // 24bpp BI_RGB、height 为负（自顶向下）、stride 补齐；内存 BGR 序
        let mut dib = vec![0u8; 40 + 8 * 2]; // 2px×3B=6B/行，stride 补到 8
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2u32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-2i32).to_le_bytes()); // 自顶向下
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        // 物理行0 = 顶行：BGR(红)=00 00 FF、BGR(绿)=00 FF 00；行1：BGR(蓝)、黑
        dib[40..46].copy_from_slice(&[0, 0, 255, 0, 255, 0]);
        dib[48..54].copy_from_slice(&[255, 0, 0, 0, 0, 0]);

        let png = dib_to_png(&dib).expect("24bpp 自顶向下应解码成功");
        let (_, _, rgba) = png_rgba(&png);
        assert_eq!(&rgba[0..8], &[255, 0, 0, 255, 0, 255, 0, 255]); // 顶行 = 红、绿（alpha 恒 255）
        assert_eq!(&rgba[8..16], &[0, 0, 255, 255, 0, 0, 0, 255]); // 底行 = 蓝、黑
    }

    #[test]
    fn png_dib_roundtrip() {
        // 写入方向：PNG → DIBV5 关键字段应在标准位置，且能被自家读取端无损解回
        let dib = png_to_dibv5(&sample_png()).unwrap();
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 124);
        assert_eq!(u16::from_le_bytes(dib[12..14].try_into().unwrap()), 1); // Planes
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32); // BitCount
        assert_eq!(u32::from_le_bytes(dib[16..20].try_into().unwrap()), 3); // BI_BITFIELDS
        assert_eq!(u32::from_le_bytes(dib[20..24].try_into().unwrap()), 16); // SizeImage = stride 8×2 行
        assert_eq!(u32::from_le_bytes(dib[40..44].try_into().unwrap()), 0x00FF0000); // RedMask@40（修正后的位置）
        assert_eq!(u32::from_le_bytes(dib[56..60].try_into().unwrap()), 0x73524742); // CSType@56

        let back = dib_to_png(&dib).expect("自家写入的 DIB 应能解回");
        let (_, _, rgba) = png_rgba(&back);
        assert_eq!(rgba.as_slice(), &[RED, GREEN, BLUE, YELLOW].concat());
    }

    #[test]
    fn rejects_unsupported() {
        // 16bpp / RLE 压缩应被拒绝（有诊断日志提示）
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2u32.to_le_bytes());
        dib[8..12].copy_from_slice(&2u32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&16u16.to_le_bytes());
        assert!(dib_to_png(&dib).is_none());

        let mut rle = vec![0u8; 40 + 16];
        rle[0..4].copy_from_slice(&40u32.to_le_bytes());
        rle[4..8].copy_from_slice(&2u32.to_le_bytes());
        rle[8..12].copy_from_slice(&2u32.to_le_bytes());
        rle[12..14].copy_from_slice(&1u16.to_le_bytes());
        rle[14..16].copy_from_slice(&8u16.to_le_bytes());
        rle[16..20].copy_from_slice(&1u32.to_le_bytes()); // BI_RLE8
        assert!(dib_to_png(&rle).is_none());
    }
}
