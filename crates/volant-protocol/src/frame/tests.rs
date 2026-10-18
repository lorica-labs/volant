// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::io::Cursor;

use rstest::rstest;

#[test]
fn round_trips_a_payload() {
    let mut buf = Vec::new();
    write_frame(&mut buf, b"hello").unwrap();
    assert_eq!(buf, [0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']);
    let read = read_frame(Cursor::new(buf)).unwrap();
    assert_eq!(read.as_deref(), Some(&b"hello"[..]));
}

#[test]
fn clean_end_of_stream_is_none() {
    assert_eq!(read_frame(Cursor::new(Vec::<u8>::new())).unwrap(), None);
}

#[rstest]
#[case::truncated_length(vec![0, 0], io::ErrorKind::UnexpectedEof)]
#[case::truncated_payload(vec![0, 0, 0, 9, b'x'], io::ErrorKind::UnexpectedEof)]
#[case::oversized_length(vec![0xff; 4], io::ErrorKind::InvalidData)]
fn a_broken_frame_is_an_error(#[case] bytes: Vec<u8>, #[case] kind: io::ErrorKind) {
    assert_eq!(read_frame(Cursor::new(bytes)).unwrap_err().kind(), kind);
}
