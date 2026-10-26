// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{BufReader, Read, Write};

use super::{CHUNK, Reader, Writer, decode};

#[test]
fn every_byte_value_crosses_in_lines_no_longer_than_the_terminal_takes() {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let mut wire = Vec::new();
    Writer::new(&mut wire).write_all(&data).unwrap();
    assert!(wire.split(|b| *b == b'\n').all(|line| line.len() < 4095));
    assert!(wire.iter().all(u8::is_ascii));
    let mut back = Vec::new();
    Reader::new(BufReader::new(wire.as_slice()))
        .read_to_end(&mut back)
        .unwrap();
    assert_eq!(back, data);
}

#[test]
fn a_writer_may_cut_anywhere_and_a_line_ending_in_cr_lf_still_reads() {
    let mut wire = Vec::new();
    {
        let mut writer = Writer::new(&mut wire);
        for piece in [&b"ab"[..], b"c", &[0xef, 0xbf, 0xbd], &vec![9u8; CHUNK + 5]] {
            writer.write_all(piece).unwrap();
        }
    }
    let wire = String::from_utf8(wire).unwrap().replace('\n', "\r\n");
    let mut back = Vec::new();
    Reader::new(BufReader::new(wire.as_bytes()))
        .read_to_end(&mut back)
        .unwrap();
    let mut expected = b"abc\xef\xbf\xbd".to_vec();
    expected.extend(vec![9u8; CHUNK + 5]);
    assert_eq!(back, expected);
}

#[test]
fn the_end_line_ends_the_input_and_an_empty_line_does_not() {
    let mut wire = super::line(b"before");
    wire.extend_from_slice(b"\n");
    wire.extend_from_slice(super::END);
    wire.extend_from_slice(&super::line(b"after"));
    let mut back = Vec::new();
    Reader::new(BufReader::new(wire.as_slice()))
        .read_to_end(&mut back)
        .unwrap();
    assert_eq!(back, b"before");
}

#[test]
fn a_line_that_is_not_base64_is_an_error() {
    assert!(decode(b"verify: /tmp is writable by others\n").is_err());
}
