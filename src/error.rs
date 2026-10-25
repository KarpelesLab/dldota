use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    /// No usable system dynamic loader was found.
    InterpreterNotFound,
    /// The interpreter (or another ELF image) is malformed or unsupported.
    InvalidElf(&'static str),
    /// The foreign `dlopen`/`dlsym` failed; contains its `dlerror()` text.
    Dl(String),
    Io(std::io::Error),
    /// Not implemented yet.
    Unimplemented(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InterpreterNotFound => f.write_str("no system dynamic loader found"),
            Error::InvalidElf(why) => write!(f, "invalid ELF image: {why}"),
            Error::Dl(msg) => f.write_str(msg),
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Unimplemented(what) => write!(f, "not implemented: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
