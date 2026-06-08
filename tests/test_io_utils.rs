use xet_server::format::io_utils::{read_u32_le, read_u64_le, write_u32_le, write_u64_le};
use std::io::Cursor;

#[test]
fn test_read_write_u32_le() {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, 0x12345678).unwrap();

    let mut cursor = Cursor::new(&buf);
    let value = read_u32_le(&mut cursor).unwrap();

    assert_eq!(value, 0x12345678);
}

#[test]
fn test_read_write_u64_le() {
    let mut buf = Vec::new();
    write_u64_le(&mut buf, 0x123456789ABCDEF0).unwrap();

    let mut cursor = Cursor::new(&buf);
    let value = read_u64_le(&mut cursor).unwrap();

    assert_eq!(value, 0x123456789ABCDEF0);
}

#[test]
fn test_little_endian_byte_order() {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, 0x12345678).unwrap();

    // Little-endian: least significant byte first
    assert_eq!(buf, vec![0x78, 0x56, 0x34, 0x12]);
}

#[test]
fn test_multiple_reads() {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, 0x11111111).unwrap();
    write_u64_le(&mut buf, 0x2222222233333333).unwrap();

    let mut cursor = Cursor::new(&buf);
    assert_eq!(read_u32_le(&mut cursor).unwrap(), 0x11111111);
    assert_eq!(read_u64_le(&mut cursor).unwrap(), 0x2222222233333333);
}