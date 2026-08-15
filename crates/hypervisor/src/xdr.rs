use crate::error::ApiError;

const ALIGNMENT: usize = 4;
const MAX_VALUE_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn fixed_opaque(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
        let padding = (ALIGNMENT - value.len() % ALIGNMENT) % ALIGNMENT;
        self.bytes.resize(self.bytes.len() + padding, 0);
    }

    pub(crate) fn string(&mut self, value: &str) -> Result<(), ApiError> {
        let len = u32::try_from(value.len())
            .map_err(|_| ApiError::Protocol("XDR string is too large".to_string()))?;
        self.u32(len);
        self.fixed_opaque(value.as_bytes());
        Ok(())
    }

    pub(crate) fn optional_string(&mut self, value: Option<&str>) -> Result<(), ApiError> {
        match value {
            Some(value) => {
                self.u32(1);
                self.string(value)?;
            }
            None => self.u32(0),
        }
        Ok(())
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ApiError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| ApiError::Protocol("XDR offset overflow".to_string()))?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| {
            ApiError::Protocol(format!(
                "truncated XDR value at byte {}: need {len} bytes, have {}",
                self.offset,
                self.bytes.len().saturating_sub(self.offset)
            ))
        })?;
        self.offset = end;
        Ok(value)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ApiError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("slice has four bytes");
        Ok(u32::from_be_bytes(bytes))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ApiError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("slice has four bytes");
        Ok(i32::from_be_bytes(bytes))
    }

    pub(crate) fn fixed_opaque<const N: usize>(&mut self) -> Result<[u8; N], ApiError> {
        let value: [u8; N] = self
            .take(N)?
            .try_into()
            .expect("slice has requested length");
        let padding = (ALIGNMENT - N % ALIGNMENT) % ALIGNMENT;
        self.take(padding)?;
        Ok(value)
    }

    fn opaque(&mut self) -> Result<&'a [u8], ApiError> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| ApiError::Protocol("XDR length does not fit usize".to_string()))?;
        if len > MAX_VALUE_LENGTH {
            return Err(ApiError::Protocol(format!(
                "XDR value length {len} exceeds {MAX_VALUE_LENGTH} bytes"
            )));
        }
        let value = self.take(len)?;
        let padding = (ALIGNMENT - len % ALIGNMENT) % ALIGNMENT;
        self.take(padding)?;
        Ok(value)
    }

    pub(crate) fn string(&mut self) -> Result<String, ApiError> {
        String::from_utf8(self.opaque()?.to_vec())
            .map_err(|error| ApiError::Protocol(format!("XDR string is not UTF-8: {error}")))
    }

    pub(crate) fn optional_string(&mut self) -> Result<Option<String>, ApiError> {
        match self.u32()? {
            0 => Ok(None),
            1 => self.string().map(Some),
            value => Err(ApiError::Protocol(format!(
                "invalid XDR optional-value discriminant {value}"
            ))),
        }
    }

    pub(crate) fn array_len(&mut self) -> Result<usize, ApiError> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| ApiError::Protocol("XDR array length does not fit usize".to_string()))?;
        if len > MAX_VALUE_LENGTH / ALIGNMENT {
            return Err(ApiError::Protocol(format!(
                "XDR array length {len} is too large"
            )));
        }
        Ok(len)
    }

    pub(crate) fn finish(self) -> Result<(), ApiError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ApiError::Protocol(format!(
                "{} trailing bytes after XDR value",
                self.bytes.len() - self.offset
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_string_and_optional_values_round_trip() {
        let mut writer = Writer::new();
        writer.u32(0x1234_5678);
        writer.i32(-7);
        writer.string("xdr").unwrap();
        writer.string("τ=2π").unwrap();
        writer.optional_string(Some("qemu:///system")).unwrap();
        writer.optional_string(None).unwrap();

        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 0x1234_5678);
        assert_eq!(reader.i32().unwrap(), -7);
        assert_eq!(reader.string().unwrap(), "xdr");
        assert_eq!(reader.string().unwrap(), "τ=2π");
        assert_eq!(
            reader.optional_string().unwrap().as_deref(),
            Some("qemu:///system")
        );
        assert_eq!(reader.optional_string().unwrap(), None);
        reader.finish().unwrap();
    }

    #[test]
    fn strings_use_big_endian_lengths_and_four_byte_padding() {
        let mut writer = Writer::new();
        writer.string("xdr").unwrap();
        assert_eq!(writer.into_inner(), [0, 0, 0, 3, b'x', b'd', b'r', 0]);
    }

    #[test]
    fn truncated_value_is_rejected() {
        let mut reader = Reader::new(&[0, 0, 0, 4, b'x', b'd']);
        assert!(reader.string().is_err());
    }

    #[test]
    fn invalid_optional_discriminant_is_rejected() {
        let mut reader = Reader::new(&[0, 0, 0, 2]);
        assert!(reader.optional_string().is_err());
    }
}
