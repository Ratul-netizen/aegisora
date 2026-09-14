//! Bus errors.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid subject: {0}")]
    InvalidSubject(String),

    /// The bus is gone — shut down, or the transport dropped. Distinct from "nobody is
    /// subscribed", which is not an error: a publish with no listeners succeeds.
    #[error("bus closed")]
    Closed,

    #[error("bus transport: {0}")]
    Transport(String),
}

impl From<Error> for uops_core::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidSubject(m) => Self::Invalid(m),
            other => Self::Storage(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_subject_is_the_callers_fault_and_a_dead_bus_is_not() {
        // The distinction matters at the API edge: one is a 400 the caller can fix, the
        // other is a 500 that pages someone.
        let mapped: uops_core::Error = Error::InvalidSubject("nope".into()).into();
        assert_eq!(mapped.status_code(), 400);

        let mapped: uops_core::Error = Error::Closed.into();
        assert_eq!(mapped.status_code(), 500);
    }
}
