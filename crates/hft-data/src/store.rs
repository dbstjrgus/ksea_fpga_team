//! On-disk format: a 64-byte header followed by `count` packed [`MboEvent`] records,
//! one file per symbol per session. Readers memory-map the file and replay it as a slice.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::Path;

use bytemuck::{Pod, Zeroable};
use memmap2::Mmap;

use crate::event::{MboEvent, PRICE_SCALE};

const MAGIC: [u8; 8] = *b"HFTMBO01";
const VERSION: u32 = 1;
const HEADER_LEN: usize = size_of::<Header>();

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: u32,
    pub record_size: u32,
    pub count: u64,
    /// Session date as YYYYMMDD (0 for synthetic data).
    pub date: u32,
    _pad: u32,
    /// Space-padded ticker.
    pub symbol: [u8; 8],
    pub price_scale: i64,
    _reserved: [u8; 16],
}

const _: () = assert!(HEADER_LEN == 64);

impl Header {
    pub fn symbol(&self) -> &str {
        std::str::from_utf8(&self.symbol).unwrap_or("?").trim_end()
    }
}

/// Space-pads (or truncates) a ticker to ITCH's 8-byte form.
pub fn pad_symbol(symbol: &str) -> [u8; 8] {
    let mut out = [b' '; 8];
    let bytes = symbol.as_bytes();
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub struct EventWriter {
    out: BufWriter<File>,
    header: Header,
}

impl EventWriter {
    pub fn create(path: impl AsRef<Path>, symbol: &str, date: u32) -> io::Result<Self> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let header = Header {
            magic: MAGIC,
            version: VERSION,
            record_size: size_of::<MboEvent>() as u32,
            count: 0,
            date,
            _pad: 0,
            symbol: pad_symbol(symbol),
            price_scale: PRICE_SCALE,
            _reserved: [0; 16],
        };
        out.write_all(bytemuck::bytes_of(&header))?;
        Ok(EventWriter { out, header })
    }

    #[inline]
    pub fn push(&mut self, ev: &MboEvent) -> io::Result<()> {
        self.header.count += 1;
        self.out.write_all(bytemuck::bytes_of(ev))
    }

    /// Writes the final record count into the header. Returns the count.
    pub fn finish(mut self) -> io::Result<u64> {
        self.out.seek(SeekFrom::Start(0))?;
        self.out.write_all(bytemuck::bytes_of(&self.header))?;
        self.out.flush()?;
        Ok(self.header.count)
    }
}

/// A memory-mapped event file.
pub struct EventFile {
    mmap: Mmap,
    header: Header,
}

impl EventFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the file is only read. As with any mmap reader, another process
        // truncating it while mapped is undefined behaviour we accept.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_LEN {
            return Err(invalid("file shorter than header"));
        }
        let header: Header = bytemuck::pod_read_unaligned(&mmap[..HEADER_LEN]);
        if header.magic != MAGIC {
            return Err(invalid("not an MBO event file"));
        }
        if header.version != VERSION || header.record_size as usize != size_of::<MboEvent>() {
            return Err(invalid("unsupported MBO file version"));
        }
        if mmap.len() != HEADER_LEN + header.count as usize * size_of::<MboEvent>() {
            return Err(invalid(
                "file length does not match record count (incomplete write?)",
            ));
        }
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential)?;
        Ok(EventFile { mmap, header })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn events(&self) -> &[MboEvent] {
        // The mapping is page-aligned and the header is 64 bytes, so records are aligned.
        bytemuck::cast_slice(&self.mmap[HEADER_LEN..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Side;

    #[test]
    fn round_trip() {
        let path = std::env::temp_dir().join(format!("hft-store-{}.mbo", std::process::id()));
        let events = [
            MboEvent::add(1, Side::Bid, 7, 1_000_000, 100),
            MboEvent::fill(2, Side::Bid, 7, 1_000_000, 40),
            MboEvent::status(3, b'Q'),
        ];
        let mut w = EventWriter::create(&path, "AAPL", 20240102).unwrap();
        for ev in &events {
            w.push(ev).unwrap();
        }
        assert_eq!(w.finish().unwrap(), 3);

        let f = EventFile::open(&path).unwrap();
        assert_eq!(f.header().symbol(), "AAPL");
        assert_eq!(f.header().date, 20240102);
        assert_eq!(f.events(), &events);
        std::fs::remove_file(path).unwrap();
    }
}
