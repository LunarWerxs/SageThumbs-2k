//! Test-only writers shared by streaming export tests.

use std::io::{self, Seek, SeekFrom, Write};

/// A sink that accepts a fixed number of complete writes, then returns an
/// injected I/O error.
#[derive(Debug)]
pub(crate) struct FailingWriter {
    writes_until_failure: usize,
    position: u64,
    len: u64,
}

impl FailingWriter {
    pub(crate) fn after(writes_until_failure: usize) -> Self {
        Self {
            writes_until_failure,
            position: 0,
            len: 0,
        }
    }
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.writes_until_failure == 0 {
            return Err(io::Error::other("injected sink failure"));
        }

        self.writes_until_failure -= 1;
        let written = bytes.len();
        self.position = self.position.saturating_add(written as u64);
        self.len = self.len.max(self.position);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for FailingWriter {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let position = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
        };
        self.position = u64::try_from(position).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid injected writer seek")
        })?;
        Ok(self.position)
    }
}

/// A sink that counts output without retaining any bytes.
#[cfg(feature = "pdf")]
#[derive(Default)]
pub(crate) struct CountingWriter {
    position: u64,
    len: u64,
}

#[cfg(feature = "pdf")]
impl CountingWriter {
    pub(crate) fn bytes_written(&self) -> u64 {
        self.len
    }
}

#[cfg(feature = "pdf")]
impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.position = self.position.saturating_add(bytes.len() as u64);
        self.len = self.len.max(self.position);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "pdf")]
impl Seek for CountingWriter {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let position = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
        };
        self.position = u64::try_from(position).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid counting writer seek")
        })?;
        Ok(self.position)
    }
}
