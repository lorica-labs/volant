// SPDX-License-Identifier: GPL-3.0-or-later

//! The sums of the source and of `dest`, read in blocks.

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;

use super::Stop;
use crate::natives::common::check;
use crate::natives::runtime::Clock;

/// `module.sha1(path)` and `module.md5(path)`, read once.
///
/// The deadline and the cancel are looked at every 16 blocks (1 MiB), as the Python module would
/// be killed at the task's `timeout` in the middle of a large file.
pub(super) fn digests(path: &str, clock: Clock) -> Result<(String, String), Stop> {
    let mut md5 = Md5::new();
    let sha1 = read_blocks(path, clock, |block| md5.update(block))?;
    Ok((sha1, md5.hex()))
}

pub(super) fn sha1_of(path: &str, clock: Clock) -> Result<String, Stop> {
    read_blocks(path, clock, |_| {})
}

/// The SHA-1 of the file at `path`, each block also given to `each`.
fn read_blocks(path: &str, clock: Clock, mut each: impl FnMut(&[u8])) -> Result<String, Stop> {
    let mut file = fs::File::open(path).map_err(|err| format!("reading {path}: {err}"))?;
    let mut sha1 = sha1_smol::Sha1::new();
    let mut block = vec![0; 64 * 1024];
    for round in 0_u64.. {
        if round % 16 == 0 {
            check(clock)?;
        }
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => {
                sha1.update(&block[..n]);
                each(&block[..n]);
            }
            Err(err) => return Err(format!("reading {path}: {err}").into()),
        }
    }
    Ok(sha1.digest().to_string())
}

/// MD5 (RFC 1321), for `md5sum`: no crate of the workspace has it.
pub(super) struct Md5 {
    state: [u32; 4],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

/// `floor(abs(sin(i + 1)) * 2^32)`.
const MD5_K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// The left rotations of each round, four per round.
const MD5_S: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];

impl Md5 {
    pub(super) fn new() -> Self {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            block: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    pub(super) fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                self.compress();
                self.filled = 0;
            }
        }
    }

    fn compress(&mut self) {
        let mut m = [0_u32; 16];
        for (word, bytes) in m.iter_mut().zip(self.block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let turned = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[i])
                .wrapping_add(m[g])
                .rotate_left(MD5_S[i / 16][i % 4]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(turned);
        }
        for (word, add) in self.state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(add);
        }
    }

    pub(super) fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_le_bytes());
        let mut out = String::with_capacity(32);
        for byte in self.state.iter().flat_map(|word| word.to_le_bytes()) {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}
