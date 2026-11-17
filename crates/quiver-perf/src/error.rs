/// Why a ring or `hud.image` (header or descriptor) can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The magic isn't there yet: the creator hasn't finished. Retry later.
    NotReady,
    BadMagic(u64),
    Version(u16),
    /// A header field that fixes the layout is wrong.
    Layout(&'static str),
    /// The region is smaller than the header says it must be.
    TooSmall { need: usize, have: usize },
    /// A writer was given a read-only region.
    ReadOnly,
    /// A value failed validation (descriptor bounds, parameters).
    Invalid(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotReady => f.write_str("not initialized yet"),
            Error::BadMagic(m) => write!(f, "bad magic {m:#018x}"),
            Error::Version(v) => write!(f, "unsupported version {v}"),
            Error::Layout(what) => write!(f, "bad layout: {what}"),
            Error::TooSmall { need, have } => write!(f, "region is {have} bytes, needs {need}"),
            Error::ReadOnly => f.write_str("region is read-only"),
            Error::Invalid(what) => write!(f, "invalid: {what}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        let kind = if e == Error::NotReady { std::io::ErrorKind::WouldBlock } else { std::io::ErrorKind::InvalidData };
        std::io::Error::new(kind, e)
    }
}
