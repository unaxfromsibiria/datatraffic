use bytes::Bytes;

/// A unit of data exchanged over the channel: a byte stream presented as a file.
#[derive(Debug, Clone)]
pub struct DataFile {
    /// Logical name / label for the payload (the "file name").
    pub name: String,
    /// Short note attached to the payload.
    pub note: String,
    /// Raw content bytes.
    pub data: Bytes,
}

#[derive(Debug)]
pub enum TransportError {
    TooShort,
    BadUtf8,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "not enough data"),
            Self::BadUtf8 => write!(f, "text is not valid UTF-8"),
        }
    }
}

impl std::error::Error for TransportError {}
