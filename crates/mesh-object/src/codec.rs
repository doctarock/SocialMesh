//! Minimal canonical binary codec.
//!
//! Every value has exactly one encoding: big-endian fixed-width integers,
//! `u32` length prefixes for variable data, `0x00`/`0x01` presence flags for
//! optional values, and fields in a fixed order. Decoders reject trailing
//! bytes, over-long fields and non-canonical flags.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("field `{field}` exceeds limit of {max} bytes/items")]
    TooLong { field: &'static str, max: usize },
    #[error("field `{0}` is not valid UTF-8")]
    InvalidUtf8(&'static str),
    #[error("{0} trailing bytes after value")]
    TrailingBytes(usize),
    #[error("invalid presence flag {0}")]
    BadFlag(u8),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.raw(&v.to_be_bytes())
    }
    /// `u32` length prefix followed by the bytes. Callers enforce limits first.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        let len = u32::try_from(b.len()).expect("field longer than u32::MAX");
        self.u32(len).raw(b)
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
    pub fn flag(&mut self, present: bool) -> &mut Self {
        self.u8(present as u8)
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::UnexpectedEof)?;
        if end > self.buf.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.fixed()?))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.fixed()?))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.fixed()?))
    }
    pub fn bytes(&mut self, field: &'static str, max: usize) -> Result<&'a [u8], DecodeError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(DecodeError::TooLong { field, max });
        }
        self.take(len)
    }
    pub fn str(&mut self, field: &'static str, max: usize) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes(field, max)?).map_err(|_| DecodeError::InvalidUtf8(field))
    }
    pub fn flag(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(DecodeError::BadFlag(b)),
        }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn finish(self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(DecodeError::TrailingBytes(n)),
        }
    }
}
