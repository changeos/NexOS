//! EXIF 元数据解析（纯逻辑，无外部依赖）。
//!
//! 范围：从 JPEG 字节流（含 APP1 EXIF 段）解析关键字段——
//! 拍摄时间、像素宽高、GPS（lat/lon/alt）、Orientation、Make/Model、ISO、
//! 曝光时间、焦距。覆盖 Exif 标准 IFD0 / ExifIFD / GPS IFD 的常用 Rational/SRational/Short/Long/ASCII。
//!
//! 设计取舍：本 crate 不引入 kamadak-exif 等依赖（workspace 未注册），自实现一个**最小够用**
//! 的 TIFF/EXIF 遍历器；只解析 JPEG 包装（FF D8 ... FF E1），不解析 TIFF 包装的 RAW。
//! 失败路径返回 `None`/部分字段——不抛异常，因为 EXIF 缺失是常态。

use os_core::DateTime;

use crate::media::{ExifData, GpsCoord};

/// JPEG SOI 标记
const SOI: u8 = 0xD8;
/// APP1 标记
const APP1: u8 = 0xE1;

/// 从 JPEG 字节流解析 EXIF；非 JPEG 或无 EXIF 返回 `None`。
///
/// 输入需为完整 JPEG（以 `FF D8` 起始）；HEIF/RAW 不支持。
pub fn parse_exif(jpeg: &[u8]) -> Option<ExifData> {
    // 1) 定位 APP1 段并校验 "Exif\0\0" 头
    let app1 = find_app1_exif(jpeg)?;
    // app1 指向 "Exif\0\0" 之后的 TIFF 头起点
    let tiff = &app1[6..];

    let p = TiffParser::new(tiff)?;
    let ifd0_off = p.read_u32(4)? as usize;
    let mut out = ExifData::default();

    // IFD0：宽高（部分相机放在这里）、Make/Model、Orientation、Software
    walk_ifd(&p, ifd0_off, &mut |tag, val| match tag {
        0x010F => out.make = val.as_ascii(),
        0x0110 => out.model = val.as_ascii(),
        0x0112 => out.orientation = val.as_u16(),
        0x011A => out.focal_length = val.as_rational_f32(),
        0x0131 => out.software = val.as_ascii(),
        0x0100 => {
            if let Some(w) = val.as_u32() {
                out.width = out.width.or(Some(w));
            }
        }
        0x0101 => {
            if let Some(h) = val.as_u32() {
                out.height = out.height.or(Some(h));
            }
        }
        _ => {}
    });

    // ExifIFD（偏移 0x8769）：DateTimeOriginal、ISO、ExposureTime、像素宽高（ExifImageWidth/Height）
    if let Some(exif_off) = p.find_tag_in(ifd0_off, 0x8769).and_then(|v| v.as_u32()) {
        walk_ifd(&p, exif_off as usize, &mut |tag, val| match tag {
            0x9003 => out.taken_at = val.as_ascii().and_then(parse_exif_datetime),
            0x9004 => {
                if out.taken_at.is_none() {
                    out.taken_at = val.as_ascii().and_then(parse_exif_datetime);
                }
            }
            0x8827 => out.iso = val.as_u16().map(|v| v as u32),
            0x829A => out.exposure_time = val.as_rational_pair(),
            0x920A => {
                if out.focal_length.is_none() {
                    out.focal_length = val.as_rational_f32();
                }
            }
            0xA002 => {
                if let Some(w) = val.as_u32() {
                    out.width = Some(w);
                }
            }
            0xA003 => {
                if let Some(h) = val.as_u32() {
                    out.height = Some(h);
                }
            }
            _ => {}
        });
    }

    // GPS IFD（偏移 0x8825）
    if let Some(gps_off) = p.find_tag_in(ifd0_off, 0x8825).and_then(|v| v.as_u32()) {
        let mut lat_ref = None::<char>;
        let mut lon_ref = None::<char>;
        let mut lat = None::<f64>;
        let mut lon = None::<f64>;
        let mut alt_ref = None::<i8>;
        let mut alt = None::<f64>;
        walk_ifd(&p, gps_off as usize, &mut |tag, val| match tag {
            0x0001 => lat_ref = val.as_ascii().and_then(|s| s.chars().next()),
            0x0002 => lat = val.as_rational_triple_dms(),
            0x0003 => lon_ref = val.as_ascii().and_then(|s| s.chars().next()),
            0x0004 => lon = val.as_rational_triple_dms(),
            0x0005 => alt_ref = val.as_u16().map(|v| v as i8),
            0x0006 => alt = val.as_rational_f64(),
            _ => {}
        });
        if let (Some(lat), Some(lon)) = (lat, lon) {
            let lat = if matches!(lat_ref, Some('S' | 's')) {
                -lat
            } else {
                lat
            };
            let lon = if matches!(lon_ref, Some('W' | 'w')) {
                -lon
            } else {
                lon
            };
            let altitude = alt.map(|a| if matches!(alt_ref, Some(1)) { -a } else { a });
            out.gps = Some(GpsCoord { lat, lon, altitude });
        }
    }

    Some(out)
}

/// 在 JPEG 字节流中定位 APP1 EXIF 段（返回包含 `Exif\0\0` 头起点的切片）。
fn find_app1_exif(jpeg: &[u8]) -> Option<&[u8]> {
    // JPEG 必须以 FF D8 起始
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != SOI {
        return None;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            // 段间填充字节或对齐——跳过到下一个 FF
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        // SOS（FF DA）后为图像数据，扫描结束
        if marker == 0xDA {
            return None;
        }
        // 长度字段含自身 2 字节
        if i + 4 > jpeg.len() {
            return None;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            return None;
        }
        let seg = &jpeg[i + 4..i + 2 + len];
        if marker == APP1 && seg.len() >= 6 && &seg[0..4] == b"Exif" && seg[4] == 0 && seg[5] == 0 {
            return Some(seg);
        }
        i += 2 + len;
    }
    None
}

/// 解析 EXIF 日期字符串 `YYYY:MM:DD HH:MM:SS` 为 UTC DateTime。
fn parse_exif_datetime(s: String) -> Option<DateTime> {
    use chrono::NaiveDateTime;
    let s = s.trim_end_matches('\0');
    let nt = NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S").ok()?;
    DateTime::from_naive_utc_and_offset(nt, chrono::Utc).into()
}

// ----------------------------------------------------------------------------
// TIFF 遍历器（最小实现：仅支持 EXIF 常用类型）
// ----------------------------------------------------------------------------

/// TIFF 字段值（已转换为常用 Rust 类型）。
#[derive(Debug, Clone)]
enum Value {
    /// ASCII（去尾 \0）
    Ascii(String),
    /// 16-bit
    Short(u16),
    /// 32-bit
    Long(u32),
    /// 无符号有理数 (numerator, denominator)
    Rational(u32, u32),
    /// 三连有理数（GPS DMS：度/分/秒）
    RationalTriple(f64, f64, f64),
}

impl Value {
    fn as_ascii(&self) -> Option<String> {
        match self {
            Value::Ascii(s) => Some(s.clone()),
            _ => None,
        }
    }
    fn as_u16(&self) -> Option<u16> {
        match self {
            Value::Short(v) => Some(*v),
            Value::Long(v) => u16::try_from(*v).ok(),
            _ => None,
        }
    }
    fn as_u32(&self) -> Option<u32> {
        match self {
            Value::Long(v) => Some(*v),
            Value::Short(v) => Some(*v as u32),
            _ => None,
        }
    }
    fn as_rational_pair(&self) -> Option<(u32, u32)> {
        match self {
            Value::Rational(n, d) => Some((*n, *d)),
            _ => None,
        }
    }
    fn as_rational_f32(&self) -> Option<f32> {
        match self {
            Value::Rational(n, d) => {
                if *d == 0 {
                    None
                } else {
                    Some(*n as f32 / *d as f32)
                }
            }
            _ => None,
        }
    }
    fn as_rational_f64(&self) -> Option<f64> {
        match self {
            Value::Rational(n, d) => {
                if *d == 0 {
                    None
                } else {
                    Some(*n as f64 / *d as f64)
                }
            }
            _ => None,
        }
    }
    /// GPS DMS（度/分/秒三连有理数）转十进制度。
    fn as_rational_triple_dms(&self) -> Option<f64> {
        match self {
            Value::RationalTriple(d, m, s) => Some(d + m / 60.0 + s / 3600.0),
            _ => None,
        }
    }
}

/// TIFF 头解析器。所有偏移相对 TIFF 头起点（即 `data[0]`）。
struct TiffParser<'a> {
    data: &'a [u8],
    le: bool, // true = little-endian
}

impl<'a> TiffParser<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        let le = match (data[0], data[1]) {
            (0x49, 0x49) => true,  // "II"
            (0x4D, 0x4D) => false, // "MM"
            _ => return None,
        };
        // magic 42（0x002A）
        let magic = if le {
            u16::from_le_bytes([data[2], data[3]])
        } else {
            u16::from_be_bytes([data[2], data[3]])
        };
        if magic != 42 {
            return None;
        }
        Some(Self { data, le })
    }

    fn read_u16(&self, off: usize) -> Option<u16> {
        let b = self.data.get(off..off + 2)?;
        Some(if self.le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn read_u32(&self, off: usize) -> Option<u32> {
        let b = self.data.get(off..off + 4)?;
        Some(if self.le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    /// 在指定 IFD 中查找单个 tag，返回其值（首条命中）。
    fn find_tag_in(&self, ifd_off: usize, want_tag: u16) -> Option<Value> {
        let mut hit = None;
        walk_ifd(self, ifd_off, &mut |tag, val| {
            if tag == want_tag && hit.is_none() {
                hit = Some(val);
            }
        });
        hit
    }
}

/// 遍历一个 IFD 的所有 entry，对每个 entry 调用 `f(tag, value)`。
/// `p` 不持有可变状态，传引用仅为复用 endianness 与 data 切片。
fn walk_ifd(p: &TiffParser<'_>, ifd_off: usize, f: &mut impl FnMut(u16, Value)) {
    let count = match p.read_u16(ifd_off) {
        Some(c) => c as usize,
        None => return,
    };
    let base = ifd_off + 2;
    for i in 0..count {
        let entry = base + i * 12;
        if entry + 12 > p.data.len() {
            break;
        }
        let tag = match p.read_u16(entry) {
            Some(t) => t,
            None => continue,
        };
        let typ = match p.read_u16(entry + 2) {
            Some(t) => t,
            None => continue,
        };
        let cnt = match p.read_u32(entry + 4) {
            Some(c) => c as usize,
            None => continue,
        };
        if let Some(val) = read_value(p, typ, cnt, entry + 8) {
            f(tag, val);
        }
    }
}

/// 读取单条 IFD entry 的值。
/// `value_field` 是 entry 内 4 字节值区；若数据超过 4 字节，则其中存的是相对 TIFF 头的偏移。
fn read_value(p: &TiffParser<'_>, typ: u16, count: usize, value_field: usize) -> Option<Value> {
    // 数据总字节数
    let unit = type_size(typ)?;
    let total = unit.checked_mul(count)?;
    // 数据位置：<=4 字节内联在 value_field；否则 value_field 是偏移
    let data_off = if total <= 4 {
        value_field
    } else {
        p.read_u32(value_field)? as usize
    };

    match typ {
        // ASCII
        2 => {
            let bytes = p.data.get(data_off..data_off + count)?;
            let mut s = String::from_utf8_lossy(bytes).to_string();
            // 去尾 \0
            while s.ends_with('\0') {
                s.pop();
            }
            Some(Value::Ascii(s.trim().to_string()))
        }
        // SHORT (u16)
        3 => {
            if count == 1 {
                p.read_u16(data_off).map(Value::Short)
            } else {
                None
            }
        }
        // LONG (u32)
        4 => {
            if count == 1 {
                p.read_u32(data_off).map(Value::Long)
            } else {
                None
            }
        }
        // RATIONAL (u32/u32)
        5 => {
            if count == 1 {
                let n = p.read_u32(data_off)?;
                let d = p.read_u32(data_off + 4)?;
                Some(Value::Rational(n, d))
            } else if count == 3 {
                // GPS DMS：3 个 rational
                let d = p.read_u32(data_off)? as f64;
                let dd = p.read_u32(data_off + 4)? as f64;
                let m = p.read_u32(data_off + 8)? as f64;
                let md = p.read_u32(data_off + 12)? as f64;
                let s = p.read_u32(data_off + 16)? as f64;
                let sd = p.read_u32(data_off + 20)? as f64;
                let d = if dd == 0.0 { 0.0 } else { d / dd };
                let m = if md == 0.0 { 0.0 } else { m / md };
                let s = if sd == 0.0 { 0.0 } else { s / sd };
                Some(Value::RationalTriple(d, m, s))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn type_size(typ: u16) -> Option<usize> {
    Some(match typ {
        1 => 1,  // BYTE
        2 => 1,  // ASCII
        3 => 2,  // SHORT
        4 => 4,  // LONG
        5 => 8,  // RATIONAL
        7 => 1,  // UNDEFINED
        9 => 4,  // SLONG
        10 => 8, // SRATIONAL
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 JPEG+EXIF 字节流：SOI + APP1(Exif\0\0 + TIFF) + EOI。
    /// IFD0 含 Orientation=6、Make="TestCam"；ExifIFD 含 ISO=200；GPS 含 lat。
    fn build_minimal_exif_jpeg() -> Vec<u8> {
        // 我们直接手写 TIFF 字节，再包裹进 APP1。
        // TIFF 布局（little-endian）：
        //   [0..4]   "II" + 0x002A
        //   [4..8]   IFD0 偏移 = 8
        //   [8..]    IFD0：count(2) + 2 entries(24) + nextIFD(4)
        //   entry: tag(2) type(2) count(4) value/offset(4)
        //     Orientation: tag=0x0112 type=3(SHORT) count=1 value=6
        //     ExifIFD ptr: tag=0x8769 type=4(LONG) count=1 value=<exif_off>
        //   [Make 偏移处]: ASCII "TestCam\0"（8 字节）
        //   ExifIFD: 1 entry ISO=0x8827 type=3 value=200
        let mut tiff: Vec<u8> = Vec::new();
        // 头
        tiff.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // II + 42
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @ 8

        // IFD0 @ 8
        let ifd0 = tiff.len();
        assert_eq!(ifd0, 8);
        // 先放 Make ASCII 在 IFD0 之后，再回填偏移——这里简化：固定布局
        // IFD0: count=3 (Orientation, ExifIFD ptr, Make)
        tiff.extend_from_slice(&3u16.to_le_bytes());
        // entries (3 * 12)
        let entries_start = tiff.len();
        tiff.extend_from_slice(&[0u8; 3 * 12]);
        // next IFD = 0
        tiff.extend_from_slice(&0u32.to_le_bytes());

        // Make ASCII 放在当前末尾
        let make_str = b"TestCam\0";
        let make_off = tiff.len();
        tiff.extend_from_slice(make_str);

        // ExifIFD 放在 make 之后
        let exif_off = tiff.len();
        tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        tiff.extend_from_slice(&[0u8; 12]); // placeholder
                                            // ISO entry: tag=0x8827 type=3 count=1 value=200
        let exif_entry = exif_off + 2;
        let _ = exif_entry;

        // 回填 IFD0 entries
        // Orientation: tag=0x0112 type=3 count=1 value=6（内联）
        tiff[entries_start..entries_start + 12].copy_from_slice(&build_entry(
            0x0112,
            3,
            1,
            6u32.to_le_bytes().to_vec(),
        ));
        // Make: tag=0x010F type=2 count=8 offset=make_off
        tiff[entries_start + 12..entries_start + 24].copy_from_slice(&build_entry(
            0x010F,
            2,
            make_str.len() as u32,
            (make_off as u32).to_le_bytes().to_vec(),
        ));
        // ExifIFD ptr: tag=0x8769 type=4 count=1 value=exif_off
        tiff[entries_start + 24..entries_start + 36].copy_from_slice(&build_entry(
            0x8769,
            4,
            1,
            (exif_off as u32).to_le_bytes().to_vec(),
        ));

        // 回填 ExifIFD entry（ISO）
        let off = exif_off + 2;
        tiff[off..off + 12].copy_from_slice(&build_entry(
            0x8827,
            3,
            1,
            200u32.to_le_bytes().to_vec(),
        ));

        // 包裹进 JPEG APP1
        let mut jpeg = vec![0xFF, SOI];
        jpeg.push(0xFF);
        jpeg.push(APP1);
        let payload_len = 2 + 6 + tiff.len(); // "Exif\0\0" + tiff
        jpeg.extend_from_slice(&(payload_len as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    fn build_entry(tag: u16, typ: u16, count: u32, value: Vec<u8>) -> Vec<u8> {
        let mut e = Vec::with_capacity(12);
        e.extend_from_slice(&tag.to_le_bytes());
        e.extend_from_slice(&typ.to_le_bytes());
        e.extend_from_slice(&count.to_le_bytes());
        // value 区固定 4 字节
        let mut v = value;
        while v.len() < 4 {
            v.push(0);
        }
        e.extend_from_slice(&v[..4]);
        e
    }

    #[test]
    fn parses_orientation_make_iso() {
        let jpeg = build_minimal_exif_jpeg();
        let exif = parse_exif(&jpeg).expect("EXIF 应被解析");
        assert_eq!(exif.orientation, Some(6));
        assert_eq!(exif.make.as_deref(), Some("TestCam"));
        assert_eq!(exif.iso, Some(200));
    }

    #[test]
    fn rejects_non_jpeg() {
        assert!(parse_exif(b"not a jpeg").is_none());
        assert!(parse_exif(&[]).is_none());
    }

    #[test]
    fn rejects_jpeg_without_exif() {
        // SOI + APP1(非 Exif) + EOI
        let mut jpeg = vec![0xFF, SOI, 0xFF, APP1];
        let payload = b"XMP\0some xmp data";
        jpeg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(payload);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        assert!(parse_exif(&jpeg).is_none());
    }

    #[test]
    fn parses_exif_datetime_string() {
        let dt = parse_exif_datetime("2024:03:15 10:20:30".to_string());
        assert!(dt.is_some());
        assert_eq!(dt.unwrap().format("%Y-%m-%d").to_string(), "2024-03-15");
    }

    #[test]
    fn rejects_bad_datetime() {
        assert!(parse_exif_datetime("garbage".to_string()).is_none());
    }

    #[test]
    fn gps_distance_haversine_known() {
        // 北京天安门 ≈ (39.9087, 116.3975)
        // 故宫午门 ≈ (39.9163, 116.3972)
        let a = GpsCoord::new(39.9087, 116.3975);
        let b = GpsCoord::new(39.9163, 116.3972);
        let d = a.distance_meters(&b);
        // 约 850m，允许 10% 误差
        assert!(d > 700.0 && d < 1000.0, "distance={d}");
        // 自身距离 0
        assert!((a.distance_meters(&a)).abs() < 1.0);
    }

    #[test]
    fn gps_validity() {
        assert!(GpsCoord::new(0.0, 0.0).is_valid());
        assert!(GpsCoord::new(90.0, 180.0).is_valid());
        assert!(!GpsCoord::new(91.0, 0.0).is_valid());
        assert!(!GpsCoord::new(0.0, 181.0).is_valid());
    }

    #[test]
    fn parses_gps_dms() {
        // 构造带 GPS IFD 的 JPEG：
        //   lat = (39/1, 54/1, 31/1) DMS, N  → 39.90861
        //   lon = (116/1, 23/1, 51/1) DMS, E → 116.39750
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @8
                                                     // IFD0: 1 entry (GPS ptr) + next=0
        tiff.extend_from_slice(&1u16.to_le_bytes());
        let entries_start = tiff.len();
        tiff.extend_from_slice(&[0u8; 12]);
        tiff.extend_from_slice(&0u32.to_le_bytes());

        // GPS IFD：4 entries (lat_ref, lat, lon_ref, lon)
        let gps_off = tiff.len();
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&[0u8; 4 * 12]);
        // lat/lon rational triple 数据
        let lat_off = tiff.len();
        for &(n, d) in &[(39u32, 1u32), (54, 1), (31, 1)] {
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&d.to_le_bytes());
        }
        let lon_off = tiff.len();
        for &(n, d) in &[(116u32, 1u32), (23, 1), (51, 1)] {
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&d.to_le_bytes());
        }
        // 回填 IFD0 GPS ptr
        tiff[entries_start..entries_start + 12].copy_from_slice(&build_entry(
            0x8825,
            4,
            1,
            (gps_off as u32).to_le_bytes().to_vec(),
        ));
        // 回填 GPS entries（按 tag 升序：1,2,3,4）
        tiff[gps_off + 2..gps_off + 14].copy_from_slice(&build_entry(0x0001, 2, 2, vec![b'N', 0]));
        tiff[gps_off + 14..gps_off + 26].copy_from_slice(&build_entry(
            0x0002,
            5,
            3,
            (lat_off as u32).to_le_bytes().to_vec(),
        ));
        tiff[gps_off + 26..gps_off + 38].copy_from_slice(&build_entry(0x0003, 2, 2, vec![b'E', 0]));
        tiff[gps_off + 38..gps_off + 50].copy_from_slice(&build_entry(
            0x0004,
            5,
            3,
            (lon_off as u32).to_le_bytes().to_vec(),
        ));

        // 包裹
        let mut jpeg = vec![0xFF, SOI, 0xFF, APP1];
        let payload_len = 2 + 6 + tiff.len();
        jpeg.extend_from_slice(&(payload_len as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let exif = parse_exif(&jpeg).expect("EXIF 应被解析");
        let gps = exif.gps.expect("应有 GPS");
        assert!((gps.lat - 39.90861).abs() < 1e-3, "lat={}", gps.lat);
        assert!((gps.lon - 116.39750).abs() < 1e-3, "lon={}", gps.lon);
        assert!(gps.is_valid());
        assert_eq!(gps.altitude, None);
    }
}
