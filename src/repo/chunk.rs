use std::io::{self, BufRead};

pub const MIN_CHUNK_SIZE: usize = 256 * 1024;
pub const AVG_CHUNK_SIZE: usize = 1024 * 1024;
pub const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024;

fn gear(byte: u8) -> u64 {
    let mut value = (byte as u64).wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

pub(crate) fn next_chunk(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::with_capacity(AVG_CHUNK_SIZE);
    let mut hash = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        let mut consumed = 0;
        let mut boundary = false;
        for byte in available {
            bytes.push(*byte);
            hash = hash.wrapping_shl(1).wrapping_add(gear(*byte));
            consumed += 1;
            let len = bytes.len();
            let mask = if len < AVG_CHUNK_SIZE {
                (1u64 << 21) - 1
            } else {
                (1u64 << 19) - 1
            };
            if len >= MAX_CHUNK_SIZE || (len >= MIN_CHUNK_SIZE && hash & mask == 0) {
                boundary = true;
                break;
            }
        }
        reader.consume(consumed);
        if boundary {
            return Ok(Some(bytes));
        }
    }
}
