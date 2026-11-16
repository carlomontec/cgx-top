//! Low-overhead streaming byte cursor capable of switching between ASCII text lines and raw binary byte slices.

pub struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    pub fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Read the next ASCII line (up to and excluding `\n`, stripping optional `\r`)
    pub fn read_line(&mut self) -> Option<&'a [u8]> {
        if self.is_eof() {
            return None;
        }

        let slice = &self.data[self.pos..];
        if let Some(idx) = slice.iter().position(|&b| b == b'\n') {
            let line = &slice[..idx];
            self.pos += idx + 1; // Advance past '\n'
            
            // Strip trailing \r if present
            if line.ends_with(b"\r") {
                Some(&line[..line.len() - 1])
            } else {
                Some(line)
            }
        } else {
            // Remainder of the buffer without trailing newline
            self.pos = self.data.len();
            if slice.ends_with(b"\r") {
                Some(&slice[..slice.len() - 1])
            } else {
                Some(slice)
            }
        }
    }

    /// Read an exact number of binary bytes
    pub fn read_exact(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.pos + len <= self.data.len() {
            let slice = &self.data[self.pos..self.pos + len];
            self.pos += len;
            Some(slice)
        } else {
            None
        }
    }

    /// Current byte offset from the start of the buffer
    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Move the cursor to an absolute byte offset (clamped to the buffer length)
    #[inline]
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    /// Rewind the cursor back by a number of bytes
    pub fn rewind(&mut self, bytes: usize) {
        self.pos = self.pos.saturating_sub(bytes);
    }
}
