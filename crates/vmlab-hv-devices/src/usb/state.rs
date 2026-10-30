use crate::DeviceError;

pub(super) fn invalid() -> DeviceError {
    DeviceError::State("invalid USB snapshot".into())
}

pub(super) struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u64(&mut self, n: u64) {
        self.0.extend(n.to_le_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend(b);
    }
}

pub(super) struct Reader<'a>(pub &'a [u8]);

impl<'a> Reader<'a> {
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], DeviceError> {
        let b = self.0.get(..len).ok_or_else(invalid)?;
        self.0 = &self.0[len..];
        Ok(b)
    }
    pub fn u64(&mut self) -> Result<u64, DeviceError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| invalid())?,
        ))
    }
    pub fn u32(&mut self) -> Result<u32, DeviceError> {
        self.u64()?.try_into().map_err(|_| invalid())
    }
    pub fn bool(&mut self) -> Result<bool, DeviceError> {
        match self.u64()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid()),
        }
    }
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], DeviceError> {
        let len = usize::try_from(self.u64()?).map_err(|_| invalid())?;
        if len > max {
            return Err(invalid());
        }
        self.take(len)
    }
    pub fn end(self) -> Result<(), DeviceError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}
