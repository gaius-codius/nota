//! A reader for the msgpack inside a cBpack: only the forms wordfreq's
//! files use. Arrays (fix, 16- and 32-bit lengths), strings (fix, 8-, 16-
//! and 32-bit lengths), a map of up to 15 entries and a small positive
//! integer, for the header. Anything else is refused.
//!
//! Every length is checked against the bytes left before anything is
//! allocated or skipped, so a damaged file can't make it read past the end
//! or reserve more than it holds.

use super::WordListError;

/// A position in the unpacked bytes of a cBpack.
#[derive(Debug)]
pub(super) struct Reader<'a> {
    /// The unpacked bytes.
    bytes: &'a [u8],
    /// Where the next value starts.
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// The error for finding something other than `what` here.
    pub(super) fn malformed(&self, what: &'static str) -> WordListError {
        WordListError::Malformed { at: self.at, what }
    }

    /// The length of the array that starts here, `what` naming it in the
    /// error.
    ///
    /// # Errors
    ///
    /// [`WordListError::Malformed`] if no array starts here, or it claims
    /// more elements than bytes are left (each takes at least one).
    pub(super) fn array_len(&mut self, what: &'static str) -> Result<usize, WordListError> {
        let len = match self.byte(what)? {
            b @ 0x90..=0x9f => usize::from(b & 0x0f),
            0xdc => self.uint(2, what)?,
            0xdd => self.uint(4, what)?,
            _ => return Err(self.back(what)),
        };
        if len > self.left() {
            return Err(self.malformed("an array no longer than the bytes left"));
        }
        Ok(len)
    }

    /// The string that starts here.
    ///
    /// # Errors
    ///
    /// [`WordListError::Malformed`] if no string starts here, it runs past
    /// the end, or it isn't UTF-8.
    pub(super) fn str(&mut self) -> Result<&'a str, WordListError> {
        const WHAT: &str = "a word";
        let len = match self.byte(WHAT)? {
            b @ 0xa0..=0xbf => usize::from(b & 0x1f),
            0xd9 => self.uint(1, WHAT)?,
            0xda => self.uint(2, WHAT)?,
            0xdb => self.uint(4, WHAT)?,
            _ => return Err(self.back(WHAT)),
        };
        let bytes = self.take(len, WHAT)?;
        std::str::from_utf8(bytes).map_err(|_| self.malformed("a word in UTF-8"))
    }

    /// Reads the header, `{"format": "cB", "version": 1}`, its two entries
    /// in either order.
    ///
    /// # Errors
    ///
    /// [`WordListError::Malformed`] if anything else is here.
    pub(super) fn header(&mut self) -> Result<(), WordListError> {
        const WHAT: &str = "the header {format: cB, version: 1}";
        if self.byte(WHAT)? != 0x82 {
            return Err(self.back(WHAT));
        }
        let (mut format, mut version) = (false, false);
        for _ in 0..2 {
            match self.str()? {
                "format" if !format => format = self.str()? == "cB",
                "version" if !version => version = self.byte(WHAT)? == 0x01,
                _ => return Err(self.malformed(WHAT)),
            }
        }
        if format && version {
            Ok(())
        } else {
            Err(self.malformed(WHAT))
        }
    }

    /// Checks nothing follows the outer list.
    ///
    /// # Errors
    ///
    /// [`WordListError::Malformed`] if bytes are left.
    pub(super) fn end(&self) -> Result<(), WordListError> {
        if self.left() == 0 {
            Ok(())
        } else {
            Err(self.malformed("the end of the list"))
        }
    }

    /// How many bytes are left.
    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// The next byte.
    fn byte(&mut self, what: &'static str) -> Result<u8, WordListError> {
        Ok(self.take(1, what)?[0])
    }

    /// Steps back over the byte just read, and gives the error for finding
    /// it instead of `what`.
    fn back(&mut self, what: &'static str) -> WordListError {
        self.at -= 1;
        self.malformed(what)
    }

    /// A big-endian unsigned integer of `width` bytes.
    fn uint(&mut self, width: usize, what: &'static str) -> Result<usize, WordListError> {
        let bytes = self.take(width, what)?;
        let value = bytes.iter().fold(0u64, |n, &b| n * 256 + u64::from(b));
        usize::try_from(value).map_err(|_| self.malformed(what))
    }

    /// The next `len` bytes.
    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], WordListError> {
        if len > self.left() {
            return Err(self.malformed(what));
        }
        let bytes = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(bytes)
    }
}
