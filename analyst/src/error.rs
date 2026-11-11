use thiserror::Error;

/// Why a kernel run produced no verdict.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KernelError {
    /// The kernel ran past the deadline and was interrupted.
    #[error("kernel exceeded its deadline")]
    Timeout,
    /// The kernel trapped (panic, out-of-bounds access, memory limit, ...).
    #[error("kernel trapped: {0}")]
    Trap(String),
    /// The kernel returned something that is not a valid verdict.
    #[error("bad kernel output: {0}")]
    BadOutput(String),
    /// The kernel's output is larger than the cap.
    #[error("kernel output too large")]
    TooLarge,
    /// The release text is larger than the cap; the kernel was not called.
    #[error("release text too large")]
    InputTooLarge,
}

impl KernelError {
    /// A short, stable label for metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            KernelError::Timeout => "timeout",
            KernelError::Trap(_) => "trap",
            KernelError::BadOutput(_) => "bad_output",
            KernelError::TooLarge => "too_large",
            KernelError::InputTooLarge => "input_too_large",
        }
    }
}

/// Why a kernel could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LoadError {
    /// The module is larger than the cap.
    #[error("module is {size} bytes, over the {max} byte limit")]
    TooLarge {
        /// The module's size.
        size: usize,
        /// The cap.
        max: usize,
    },
    /// The bytes are not a valid WebAssembly module.
    #[error("invalid module: {0}")]
    Invalid(String),
    /// Cranelift could not compile the module.
    #[error("compilation failed: {0}")]
    Compile(String),
    /// The module imports something; a kernel must import nothing.
    #[error("module imports {0}, a kernel must import nothing")]
    Imports(String),
    /// A required export is missing or has the wrong type.
    #[error("bad exports: {0}")]
    Exports(String),
    /// The module could not be instantiated within the limits.
    #[error("instantiation failed: {0}")]
    Instantiate(String),
    /// The kernel failed its warm-up calls.
    #[error("warm-up failed: {0}")]
    Warm(KernelError),
}
