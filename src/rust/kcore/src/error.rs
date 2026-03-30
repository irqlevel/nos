/// Kernel error codes for Rust drivers.
///
/// Matches the domain of `Stdlib::Error` codes on the C++ side.
/// Use `kcore::error::Result<T>` as the return type for fallible operations
/// and the `?` operator to propagate errors up the call stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoMemory,
    InvalidValue,
    Timeout,
    Busy,
    DeviceError,
    IoError,
    NotFound,
    Again,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Error::NoMemory      => "no memory",
            Error::InvalidValue  => "invalid value",
            Error::Timeout       => "timeout",
            Error::Busy          => "busy",
            Error::DeviceError   => "device error",
            Error::IoError       => "I/O error",
            Error::NotFound      => "not found",
            Error::Again         => "try again",
        };
        f.write_str(s)
    }
}

/// Convenience `Result` alias — use `?` to propagate `Error` up the call stack.
pub type Result<T> = core::result::Result<T, Error>;
