//! 文件指纹：稀疏采样哈希，用于导入去重。
//!
//! 思路借鉴 booklore 的 `FileFingerprint`（见 docs/audit/2026-08-27-booklore对比-导入与阅读借鉴.md）：
//! 按 4 倍指数递增的偏移取样，每处只读 1KB，**无论文件多大最多读 12KB**。
//! 500MB 的 PDF 和 2MB 的 EPUB 花的 I/O 一样多，扫描整个书仓的代价因此与总字节数无关。
//!
//! 与 booklore 的两处不同：
//! - 它用 MD5，我们用已在依赖树里的 SHA-256（无新增依赖，且不必解释为何用弱哈希）。
//! - **哈希输入前置文件长度**。只采样不带长度时，「同前缀、尾部不同且样点恰好都落在相同内容上」
//!   的两份文件会撞车（例如同一本书的两个版本，只在末尾追加了一段）。带上长度基本消除这类误判。
//!
//! 这是**去重提示**用的指纹，不是完整性校验：稀疏采样天然可被构造碰撞。
//! 仅用于「这份文件像是已经导入过」的判断，不用于安全决策。

use sha2::{Digest, Sha256};

/// 每个采样点读取的字节数
const BLOCK: usize = 1024;

/// 采样偏移：0, 2^10, 2^12, 2^14 … 2^30（共 12 个点，最多读 12KB）
fn sample_offsets() -> impl Iterator<Item = u64> {
    std::iter::once(0u64).chain((0..=10).map(|i| 1u64 << (10 + 2 * i)))
}

/// 内存中字节的稀疏指纹（上传路径：文件已在内存里，不必再落盘读一遍）
pub fn fingerprint_bytes(bytes: &[u8]) -> String {
    let len = bytes.len() as u64;
    let mut hasher = Sha256::new();
    hasher.update(len.to_le_bytes());
    for off in sample_offsets() {
        if off >= len {
            break;
        }
        let start = off as usize;
        let end = (start + BLOCK).min(bytes.len());
        hasher.update(&bytes[start..end]);
    }
    hex(hasher.finalize().as_slice())
}

/// 磁盘文件的稀疏指纹（扫描书仓路径：不把整本读进内存）
pub fn fingerprint_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut hasher = Sha256::new();
    hasher.update(len.to_le_bytes());
    let mut buf = vec![0u8; BLOCK];
    for off in sample_offsets() {
        if off >= len {
            break;
        }
        f.seek(SeekFrom::Start(off))?;
        // 末尾采样点可能读不满一个 BLOCK；read_exact 会失败，故用循环读到 EOF 为止
        let mut filled = 0usize;
        while filled < BLOCK {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled == 0 {
            break;
        }
        hasher.update(&buf[..filled]);
    }
    Ok(hex(hasher.finalize().as_slice()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个可复现的伪随机大文件（不用 rand：测试要稳定可重放）
    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn 相同内容指纹相同_不同内容不同() {
        let a = pseudo(5 * 1024 * 1024, 42);
        let b = a.clone();
        let mut c = a.clone();
        c[0] ^= 0xff; // 首块落在偏移 0 的采样点上
        assert_eq!(fingerprint_bytes(&a), fingerprint_bytes(&b));
        assert_ne!(fingerprint_bytes(&a), fingerprint_bytes(&c));
    }

    #[test]
    fn 长度不同即使前缀相同也不同指纹() {
        // 关键用例：b 是 a 的严格前缀。只采样不带长度时两者所有样点内容一致 → 会误判为同一文件。
        let a = pseudo(4 * 1024 * 1024, 7);
        let b = a[..a.len() - 1].to_vec();
        assert_ne!(
            fingerprint_bytes(&a),
            fingerprint_bytes(&b),
            "前缀相同但长度不同必须区分，否则「追加了一段的新版本」会被当成重复文件跳过"
        );
    }

    #[test]
    fn 空文件与极小文件不panic() {
        assert_eq!(fingerprint_bytes(&[]).len(), 64);
        assert_ne!(fingerprint_bytes(&[]), fingerprint_bytes(b"a"));
        // 只有 1 字节时只有偏移 0 的采样点有效，其余 break
        assert_ne!(fingerprint_bytes(b"a"), fingerprint_bytes(b"b"));
    }

    #[test]
    fn 内存版与文件版结果一致() {
        let data = pseudo(3 * 1024 * 1024 + 777, 99);
        let dir = std::env::temp_dir().join(format!("fp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.bin");
        std::fs::write(&p, &data).unwrap();
        assert_eq!(fingerprint_bytes(&data), fingerprint_file(&p).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 大文件只读12kb() {
        // 采样点共 12 个（0 与 2^10..2^30），512MB 文件也只覆盖 12KB
        assert_eq!(sample_offsets().count(), 12);
        let max_read = 12 * BLOCK;
        assert_eq!(max_read, 12 * 1024);
        // 偏移最大值 2^30 = 1GiB：超过 1GiB 的文件尾部不再采样，靠长度前缀区分
        assert_eq!(sample_offsets().last(), Some(1 << 30));
    }

    #[test]
    fn 尾部差异在采样点内可被发现() {
        // 3MB 文件的采样点覆盖到 2^20(1MB) 与 2^22(4MB，越界)；改 1MB 处必须变指纹
        let mut a = pseudo(3 * 1024 * 1024, 5);
        let b = a.clone();
        a[1 << 20] ^= 0xff;
        assert_ne!(fingerprint_bytes(&a), fingerprint_bytes(&b));
    }
}
