use std::error::Error as StdError;
use std::fmt;

/// Wraps a send failure whose delivery outcome is unknown (request may have
/// already reached Telegram without an observable response).
#[derive(Debug)]
pub struct UncertainError {
    inner: Box<dyn StdError + Send + Sync>,
}

impl UncertainError {
    pub fn new(err: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self { inner: err.into() }
    }

    pub fn into_inner(self) -> Box<dyn StdError + Send + Sync> {
        self.inner
    }
}

impl fmt::Display for UncertainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl StdError for UncertainError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.inner.as_ref())
    }
}

/// Marks an optional error as having an uncertain delivery outcome.
pub fn unknown<E>(err: Option<E>) -> Option<UncertainError>
where
    E: StdError + Send + Sync + 'static,
{
    err.map(|e| UncertainError::new(e))
}

/// Checks whether `err` or any error in its `source()` chain is an `UncertainError`.
pub fn is_uncertain(err: &(dyn StdError + 'static)) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(item) = current {
        if item.is::<UncertainError>() {
            return true;
        }
        current = item.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[derive(Debug)]
    struct WrapperError {
        msg: &'static str,
        cause: Box<dyn StdError + Send + Sync>,
    }

    impl fmt::Display for WrapperError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl StdError for WrapperError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(self.cause.as_ref())
        }
    }

    #[test]
    fn unknown_keeps_message_and_chain() {
        assert!(unknown::<io::Error>(None).is_none());
        let plain = io::Error::other("x");
        assert!(!is_uncertain(&plain));

        let inner = io::Error::new(io::ErrorKind::TimedOut, "context canceled");
        let wrapped_err = unknown(Some(inner)).expect("wrapped error");
        assert!(is_uncertain(&wrapped_err));
        assert_eq!(wrapped_err.to_string(), "context canceled");
        let src = wrapped_err.source().expect("source");
        assert_eq!(src.to_string(), "context canceled");

        let outer = WrapperError {
            msg: "outer",
            cause: Box::new(wrapped_err),
        };
        assert!(is_uncertain(&outer));
    }
}
