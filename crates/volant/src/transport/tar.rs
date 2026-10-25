// SPDX-License-Identifier: GPL-3.0-or-later
//! One regular file as a POSIX ustar archive, the stream `docker cp -` and `podman cp -` unpack
//! into a directory of the target.

const BLOCK: usize = 512;

/// `bytes` under `name` (no directory part), with `mode`, owned by root, dated 1970.
pub(super) fn single_file(name: &str, mode: u32, bytes: &[u8]) -> Vec<u8> {
    let mut header = [0u8; BLOCK];
    let mut field = |at: usize, len: usize, value: &[u8]| {
        header[at..at + value.len().min(len)].copy_from_slice(&value[..value.len().min(len)]);
    };
    field(0, 100, name.as_bytes());
    field(100, 8, format!("{mode:07o}\0").as_bytes());
    field(108, 8, b"0000000\0");
    field(116, 8, b"0000000\0");
    field(124, 12, format!("{:011o}\0", bytes.len()).as_bytes());
    field(136, 12, b"00000000000\0");
    field(156, 1, b"0");
    field(257, 6, b"ustar\0");
    field(263, 2, b"00");
    // The checksum is taken with its own field read as spaces.
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let padding = (BLOCK - bytes.len() % BLOCK) % BLOCK;
    let mut archive = Vec::with_capacity(BLOCK + bytes.len() + padding + 2 * BLOCK);
    archive.extend_from_slice(&header);
    archive.extend_from_slice(bytes);
    archive.resize(archive.len() + padding + 2 * BLOCK, 0);
    archive
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::{Command, Stdio};

    use super::single_file;

    /// The system's `tar` reads the archive back to the same bytes and mode.
    #[test]
    fn tar_reads_the_archive_back() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..1300u32).map(|n| (n % 251) as u8).collect();
        let mut tar = Command::new("tar")
            .args(["-x", "-f", "-", "-C"])
            .arg(dir.path())
            .stdin(Stdio::piped())
            .spawn()
            .expect("tar runs");
        tar.stdin
            .take()
            .unwrap()
            .write_all(&single_file("volant-agent-x", 0o755, &bytes))
            .unwrap();
        assert!(tar.wait().unwrap().success());
        let file = dir.path().join("volant-agent-x");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
