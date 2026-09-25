//! ISO9660 只读文件提取器（网络装机 P0：从 live-server ISO 提取 casper 引导件）
//!
//! 背景：P0 需要 `casper/vmlinuz` + `casper/initrd`（供 iPXE `kernel`/`initrd` 行
//! HTTP 引导），宿主机常见提取工具（7z/bsdtar/xorriso）不一定存在且安装需 root——
//! 本模块用**纯 Rust 只读 ISO9660 解析**替代外部工具：给定 `Read + Seek` 的 ISO
//! 镜像与内部路径（如 `casper/vmlinuz`），把文件数据流式拷贝到 `Write`。
//!
//! 实现范围（刻意最小，够提取 Ubuntu live-server 的引导件即可）：
//! - 主卷描述符（PVD，type=1，LBA 16 起）→ 根目录记录；
//! - 目录记录遍历（extent LBA / 数据长度，双端序取小端）；
//! - 文件名匹配：ISO9660 基础标识符**大小写不敏感** + 剥离 `;版本号` 后比对
//!   （`VMLINUZ.;1` ≡ `vmlinuz`）——Ubuntu ISO 附带 Rock Ridge/Joliet，但引导件
//!   名（casper/vmlinuz/initrd）经基础标识符即可稳定命中，不必解析 SUSP 记录；
//! - 多区段（>4GB 单文件）不支持：vmlinuz/initrd 远小于 4GB，命中即报错防御。
//!
//! 安全：路径分量拒绝空段、`.`、`..` 与绝对路径形态（穿越防御，调用方传入的
//! 内部路径是常量，这里是纵深防御第二层）。
//!
//! 纯逻辑（对泛型 `Read+Seek`/`Write` 工作，单测用 `Cursor<Vec<u8>>` 合成微型 ISO）。

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{ProvisionError, ProvisionResult};

/// ISO9660 扇区大小（字节）。
const SECTOR_SIZE: u64 = 2048;

/// 主卷描述符（PVD）起始 LBA（ISO9660 规范固定 16）。
const PVD_LBA: u64 = 16;

/// PVD 内根目录记录的偏移（规范固定 156）。
const PVD_ROOT_DIR_RECORD_OFFSET: usize = 156;

/// 拷贝缓冲区（256KiB，与流式直传通道同量级）。
const COPY_BUF: usize = 256 * 1024;

// ----------------------------------------------------------------------------
// 目录记录模型
// ----------------------------------------------------------------------------

/// 一条目录记录（文件或子目录）的关键字段。
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// 数据起始 LBA（小端解析值）。
    pub extent_lba: u64,
    /// 数据长度（字节，小端解析值）。
    pub data_len: u64,
    /// 是否目录（file flags bit 1）。
    pub is_dir: bool,
    /// 是否多区段文件（file flags bit 3；本模块不支持，命中报错）。
    pub multi_extent: bool,
    /// 基础标识符（原样，含可能的 `;1` 版本后缀）。
    pub identifier: String,
}

impl DirEntry {
    /// 基础标识符规范化：剥 `;版本号`、剥结尾 `.`、转小写——用于与目标名比对。
    fn normalized_name(&self) -> String {
        let id = self.identifier.as_str();
        let id = match id.find(';') {
            Some(i) => &id[..i],
            None => id,
        };
        let id = id.strip_prefix('\\').unwrap_or(id); //转义过的字符（罕见）不去深究
        let id = id.strip_suffix('.').unwrap_or(id);
        id.to_ascii_lowercase()
    }
}

// ----------------------------------------------------------------------------
// 提取入口
// ----------------------------------------------------------------------------

/// 从 ISO 镜像提取 `inner_path`（如 `casper/vmlinuz`，`/` 分隔，大小写不敏感）
/// 指向的文件数据，写入 `out`；返回拷贝字节数。
///
/// 错误：路径非法 / 卷描述符缺失（非 ISO）/ 路径不存在 / 中途是目录 /
/// 多区段文件 → [`ProvisionError::InvalidConfig`]（输入形态问题）或
/// `Internal`（底层 I/O）。
pub fn extract_file<R: Read + Seek, W: Write>(
    iso: &mut R,
    inner_path: &str,
    out: &mut W,
) -> ProvisionResult<u64> {
    let components = split_inner_path(inner_path)?;
    let mut current = read_root_entry(iso)?;
    // 逐层下钻目录（最后一层是文件）
    for (i, comp) in components.iter().enumerate() {
        let is_last = i == components.len() - 1;
        let entries = read_dir_entries(iso, &current)?;
        let found = entries
            .iter()
            .find(|e| !is_reserved_id(&e.identifier) && e.normalized_name() == *comp)
            .ok_or_else(|| {
                ProvisionError::InvalidConfig(format!(
                    "ISO 内路径不存在: {inner_path}（在段 '{comp}' 处未命中）"
                ))
            })?;
        if !is_last && !found.is_dir {
            return Err(ProvisionError::InvalidConfig(format!(
                "ISO 内路径中段 '{comp}' 不是目录"
            )));
        }
        if is_last && found.is_dir {
            return Err(ProvisionError::InvalidConfig(format!(
                "ISO 内路径 '{inner_path}' 是目录而非文件"
            )));
        }
        current = found.clone();
    }
    if current.multi_extent {
        return Err(ProvisionError::InvalidConfig(format!(
            "ISO 内文件 '{inner_path}' 为多区段记录（>4GB），本提取器不支持"
        )));
    }
    copy_extent(iso, &current, out)
}

/// 列出 ISO 根下某内部目录的直接子项（诊断/测试用；路径大小写不敏感）。
pub fn list_dir<R: Read + Seek>(iso: &mut R, inner_path: &str) -> ProvisionResult<Vec<String>> {
    let mut current = read_root_entry(iso)?;
    if inner_path != "/" && !inner_path.is_empty() {
        for comp in split_inner_path(inner_path)? {
            let entries = read_dir_entries(iso, &current)?;
            current = entries
                .iter()
                .find(|e| !is_reserved_id(&e.identifier) && e.normalized_name() == comp)
                .cloned()
                .ok_or_else(|| {
                    ProvisionError::InvalidConfig(format!("ISO 内目录不存在: {inner_path}"))
                })?;
        }
    }
    if !current.is_dir {
        return Err(ProvisionError::InvalidConfig(format!(
            "ISO 内路径 '{inner_path}' 不是目录"
        )));
    }
    Ok(read_dir_entries(iso, &current)?
        .into_iter()
        .filter(|e| !is_reserved_id(&e.identifier))
        .map(|e| e.identifier)
        .collect())
}

// ----------------------------------------------------------------------------
// 内部：解析
// ----------------------------------------------------------------------------

/// 拆分并校验内部路径：拒绝空段 / `.` / `..`（穿越防御）。空路径 → Err。
fn split_inner_path(path: &str) -> ProvisionResult<Vec<String>> {
    let comps: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if comps.is_empty() {
        return Err(ProvisionError::InvalidConfig(format!(
            "ISO 内路径为空: '{path}'"
        )));
    }
    if comps.iter().any(|c| c == "." || c == "..") {
        return Err(ProvisionError::InvalidConfig(format!(
            "ISO 内路径含非法段（./..）: '{path}'"
        )));
    }
    Ok(comps)
}

/// ISO9660 保留标识符（0x00=自身 / 0x01=父目录）。
fn is_reserved_id(identifier: &str) -> bool {
    identifier == "\0" || identifier == "\u{1}"
}

/// 读 PVD → 根目录记录。
fn read_root_entry<R: Read + Seek>(iso: &mut R) -> ProvisionResult<DirEntry> {
    // 卷描述符区自 LBA 16 起顺序扫描到终止符（type=255）；取第一个 type=1（PVD）
    let mut lba = PVD_LBA;
    loop {
        if lba > PVD_LBA + 32 {
            return Err(ProvisionError::InvalidConfig(
                "未找到 ISO9660 主卷描述符（不是 ISO 镜像?）".to_string(),
            ));
        }
        let mut sector = [0u8; SECTOR_SIZE as usize];
        read_exact_at(iso, lba * SECTOR_SIZE, &mut sector)?;
        if &sector[1..6] != b"CD001" {
            return Err(ProvisionError::InvalidConfig(
                "卷描述符缺 CD001 标记（不是 ISO9660 镜像）".to_string(),
            ));
        }
        let vd_type = sector[0];
        if vd_type == 255 {
            return Err(ProvisionError::InvalidConfig(
                "未找到主卷描述符（PVD）——不是 ISO9660 镜像".to_string(),
            ));
        }
        if vd_type == 1 {
            let rec = &sector[PVD_ROOT_DIR_RECORD_OFFSET..];
            return parse_dir_record(rec);
        }
        lba += 1;
    }
}

/// 读一个目录 extent 的全部记录（跳过 0x00 自身 / 0x01 父目录）。
fn read_dir_entries<R: Read + Seek>(iso: &mut R, dir: &DirEntry) -> ProvisionResult<Vec<DirEntry>> {
    let mut raw = vec![0u8; dir.data_len as usize];
    read_exact_at(iso, dir.extent_lba * SECTOR_SIZE, &mut raw)?;
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < raw.len() {
        let rec_len = raw[off] as usize;
        if rec_len == 0 {
            // 记录长度 0：跳到下一个扇区边界（目录记录不跨扇区）
            let next = (off / SECTOR_SIZE as usize + 1) * SECTOR_SIZE as usize;
            if next >= raw.len() {
                break;
            }
            off = next;
            continue;
        }
        if off + rec_len > raw.len() {
            return Err(ProvisionError::Internal(
                "目录记录越界（ISO 损坏?）".to_string(),
            ));
        }
        out.push(parse_dir_record(&raw[off..off + rec_len])?);
        off += rec_len;
    }
    Ok(out)
}

/// 解析一条目录记录（record 首字节即长度，调用方保证切片足够）。
///
/// 布局（ECMA-119 目录记录，含 `[1]` 的扩展属性记录长度字节——按真实
/// Ubuntu live-server ISO 实测校准）：`[0]`记录长 `[1]`XAR 长 `[2:6]`extent LE
/// `[6:10]`extent BE `[10:14]`数据长 LE `[14:18]`BE `[18:25]`时间 `[25]`flags
/// `[26]`unit `[27]`gap `[28:32]`卷序 `[32]`标识符长 `[33..]`标识符（其后为
/// SUSP/Rock Ridge 系统区，本模块不解析）。
///
/// 注：保留记录（自身/父目录）恰好 34 字节、标识符长 1——长度下界按
/// "33 + 标识符长"动态校验，而非固定值。
fn parse_dir_record(rec: &[u8]) -> ProvisionResult<DirEntry> {
    if rec.is_empty() {
        return Err(ProvisionError::Internal("空目录记录".to_string()));
    }
    let len = rec[0] as usize;
    if len < 34 || len > rec.len() {
        return Err(ProvisionError::Internal(format!("目录记录长度非法: {len}")));
    }
    let extent_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as u64;
    let data_len = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]) as u64;
    let flags = rec[25];
    let id_len = rec[32] as usize;
    if 33 + id_len > len {
        return Err(ProvisionError::Internal("标识符长度越界".to_string()));
    }
    let identifier = String::from_utf8_lossy(&rec[33..33 + id_len]).to_string();
    Ok(DirEntry {
        extent_lba,
        data_len,
        is_dir: flags & 0x02 != 0,
        multi_extent: flags & 0x08 != 0,
        identifier,
    })
}

/// 从 `offset` 精确读满 `buf`（Read::read_exact + 显式 seek）。
fn read_exact_at<R: Read + Seek>(iso: &mut R, offset: u64, buf: &mut [u8]) -> ProvisionResult<()> {
    iso.seek(SeekFrom::Start(offset))
        .map_err(|e| ProvisionError::Internal(format!("ISO seek 失败: {e}")))?;
    iso.read_exact(buf)
        .map_err(|e| ProvisionError::Internal(format!("ISO read 失败: {e}")))
}

/// 把一条文件记录的数据区流式拷贝到 `out`（256KiB 分块，返回字节数）。
fn copy_extent<R: Read + Seek, W: Write>(
    iso: &mut R,
    entry: &DirEntry,
    out: &mut W,
) -> ProvisionResult<u64> {
    iso.seek(SeekFrom::Start(entry.extent_lba * SECTOR_SIZE))
        .map_err(|e| ProvisionError::Internal(format!("ISO seek 失败: {e}")))?;
    let mut remaining = entry.data_len;
    let mut buf = vec![0u8; COPY_BUF];
    let mut copied: u64 = 0;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        iso.read_exact(&mut buf[..want])
            .map_err(|e| ProvisionError::Internal(format!("ISO read 失败: {e}")))?;
        out.write_all(&buf[..want])
            .map_err(|e| ProvisionError::Internal(format!("写出提取文件失败: {e}")))?;
        remaining -= want as u64;
        copied += want as u64;
    }
    Ok(copied)
}

// ----------------------------------------------------------------------------
// 单元测试（合成微型 ISO + 真实解析路径）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 合成一个最小 ISO9660 镜像：`/<dir>/<file>` 两级，内容已知。
    /// 布局（LBA）：
    /// - 16: PVD（根目录记录指向 LBA 20）
    /// - 17: 终止卷描述符（type 255）
    /// - 20: 根目录（CASPER 目录记录 → LBA 21）
    /// - 21: CASPER 目录（VMLINUZ.;1 → LBA 23、INITRD.;1 → LBA 24）
    /// - 23/24: 文件数据
    fn build_mini_iso() -> Vec<u8> {
        const DIR_LBA_ROOT: u64 = 20;
        const DIR_LBA_CASPER: u64 = 21;
        const FILE_LBA_VMLINUZ: u64 = 23;
        const FILE_LBA_INITRD: u64 = 24;
        let vmlinuz = b"KERNEL-BYTES-0123456789".to_vec();
        let initrd = b"INITRD-BYTES-abcdefghij".to_vec();

        let mut img = vec![0u8; 26 * SECTOR_SIZE as usize];

        // 卷描述符辅助
        fn vd(img: &mut [u8], lba: u64, vtype: u8) {
            let base = (lba * SECTOR_SIZE) as usize;
            img[base] = vtype;
            img[base + 1..base + 6].copy_from_slice(b"CD001");
            img[base + 6] = 1; // version
        }
        vd(&mut img, 16, 1); // PVD
        vd(&mut img, 17, 255); // terminator

        // 目录记录辅助（基础标识符 + 目录标志）——布局对齐 ECMA-119/解析器：
        // [0]=长度 [1]=XAR 长 [2..6]=extent LE [10..14]=数据长 LE [25]=flags
        // [32]=id 长 [33..]=id
        fn dir_record(name: &str, lba: u64, data_len: u64, is_dir: bool) -> Vec<u8> {
            let mut r = vec![0u8; 38 + name.len()];
            let id = name.as_bytes();
            r[33..33 + id.len()].copy_from_slice(id);
            r[2..6].copy_from_slice(&(lba as u32).to_le_bytes());
            r[10..14].copy_from_slice(&(data_len as u32).to_le_bytes());
            r[25] = if is_dir { 0x02 } else { 0x00 };
            r[32] = id.len() as u8;
            r[0] = r.len() as u8;
            r
        }
        fn put(img: &mut [u8], lba: u64, records: &[Vec<u8>]) -> usize {
            let base = (lba * SECTOR_SIZE) as usize;
            let mut off = 0;
            // 自身/父目录占位（保留 id \0/\1）
            for reserved in [0u8, 1u8] {
                let mut r = vec![0u8; 34];
                r[0] = 34; // 记录长度
                r[32] = 1; // 标识符长度 1
                r[33] = reserved; // 标识符字节（0x00=自身 / 0x01=父目录）
                img[base + off..base + off + 34].copy_from_slice(&r);
                off += 34;
            }
            for rec in records {
                img[base + off..base + off + rec.len()].copy_from_slice(rec);
                off += rec.len();
            }
            off
        }

        // 根目录：CASPER 目录记录（名 "CASPER"，数据长度=下一层目录字节数）
        let casper_dir_size = 34 * 2 + 38 + "VMLINUZ.;1".len() + 38 + "INITRD.;1".len();
        let root_size = 34 * 2 + 38 + "CASPER".len();
        let root_used = put(
            &mut img,
            DIR_LBA_ROOT,
            &[dir_record(
                "CASPER",
                DIR_LBA_CASPER,
                casper_dir_size as u64,
                true,
            )],
        );
        debug_assert_eq!(root_used, root_size);

        // CASPER 目录：两个文件记录
        put(
            &mut img,
            DIR_LBA_CASPER,
            &[
                dir_record("VMLINUZ.;1", FILE_LBA_VMLINUZ, vmlinuz.len() as u64, false),
                dir_record("INITRD.;1", FILE_LBA_INITRD, initrd.len() as u64, false),
            ],
        );

        // PVD 根目录记录（offset 156；标识符为单字节 0x00 的保留式，长度 34）
        let pvd = (16 * SECTOR_SIZE) as usize;
        let mut root_rec = dir_record("x", DIR_LBA_ROOT, root_size as u64, true);
        root_rec[0] = 34;
        root_rec[32] = 1;
        root_rec[33] = 0;
        root_rec.truncate(34);
        img[pvd + PVD_ROOT_DIR_RECORD_OFFSET..pvd + PVD_ROOT_DIR_RECORD_OFFSET + 34]
            .copy_from_slice(&root_rec);

        // 文件数据
        let v = (FILE_LBA_VMLINUZ * SECTOR_SIZE) as usize;
        img[v..v + vmlinuz.len()].copy_from_slice(&vmlinuz);
        let i = (FILE_LBA_INITRD * SECTOR_SIZE) as usize;
        img[i..i + initrd.len()].copy_from_slice(&initrd);
        img
    }

    #[test]
    fn extract_vmlinuz_case_insensitive_with_version_suffix() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let mut out = Vec::new();
        let n = extract_file(&mut cur, "casper/vmlinuz", &mut out).unwrap();
        assert_eq!(n, 23);
        assert_eq!(out, b"KERNEL-BYTES-0123456789".to_vec());
    }

    #[test]
    fn extract_initrd_leading_slash_tolerated() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let mut out = Vec::new();
        let n = extract_file(&mut cur, "/CASPER/INITRD", &mut out).unwrap();
        assert_eq!(n, 23);
        assert_eq!(out, b"INITRD-BYTES-abcdefghij".to_vec());
    }

    #[test]
    fn extract_missing_file_err() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let mut out = Vec::new();
        let err = extract_file(&mut cur, "casper/nope.img", &mut out).unwrap_err();
        assert!(err.to_string().contains("不存在"));
    }

    #[test]
    fn extract_directory_target_err() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let mut out = Vec::new();
        let err = extract_file(&mut cur, "casper", &mut out).unwrap_err();
        assert!(err.to_string().contains("目录"));
    }

    #[test]
    fn extract_traversal_rejected() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let mut out = Vec::new();
        assert!(extract_file(&mut cur, "../etc/passwd", &mut out).is_err());
        assert!(extract_file(&mut cur, "casper/../../x", &mut out).is_err());
        assert!(extract_file(&mut cur, "", &mut out).is_err());
        assert!(extract_file(&mut cur, "/", &mut out).is_err());
    }

    #[test]
    fn non_iso_input_rejected() {
        let mut cur = Cursor::new(vec![0u8; 4096]);
        let mut out = Vec::new();
        let err = extract_file(&mut cur, "casper/vmlinuz", &mut out).unwrap_err();
        assert!(err.to_string().contains("ISO"));
    }

    #[test]
    fn list_dir_returns_identifiers() {
        let img = build_mini_iso();
        let mut cur = Cursor::new(img);
        let names = list_dir(&mut cur, "casper").unwrap();
        assert_eq!(
            names,
            vec!["VMLINUZ.;1".to_string(), "INITRD.;1".to_string()]
        );
        let root = list_dir(&mut cur, "/").unwrap();
        assert_eq!(root, vec!["CASPER".to_string()]);
    }

    #[test]
    fn normalized_name_strips_version_and_lowercases() {
        let e = DirEntry {
            extent_lba: 1,
            data_len: 2,
            is_dir: false,
            multi_extent: false,
            identifier: "VMLINUZ.;1".to_string(),
        };
        assert_eq!(e.normalized_name(), "vmlinuz");
    }
}
