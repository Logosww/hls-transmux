use std::fmt;

/// Crate-local `Result` alias, returning [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the transmuxer.
///
/// Every variant carries a human-readable message (or underlying error) that
/// implements [`std::error::Error`]. Use this to distinguish recoverable
/// unsupported-feature cases ([`Error::Unsupported`]) from malformed inputs
/// ([`Error::InvalidInput`]) and bitstream/muxing failures.
#[derive(Debug)]
pub enum Error {
    /// Filesystem or network I/O failure.
    Io(std::io::Error),
    /// HTTP transport error (request failed, bad status, etc.).
    Http(String),
    /// The input was structurally invalid or inconsistent.
    InvalidInput(String),
    /// The input used a feature this crate does not (yet) support.
    Unsupported(String),
    /// The bitstream could not be parsed (malformed TS, ISOBMFF, NAL, ADTS…).
    Bitstream(String),
    /// The MP4 muxer could not assemble the output.
    Muxing(String),
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub fn bitstream(message: impl Into<String>) -> Self {
        Self::Bitstream(message.into())
    }

    pub fn muxing(message: impl Into<String>) -> Self {
        Self::Muxing(message.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Http(message) => write!(f, "HTTP error: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported feature: {message}"),
            Self::Bitstream(message) => write!(f, "bitstream error: {message}"),
            Self::Muxing(message) => write!(f, "muxing error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
