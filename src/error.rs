//! Raising Postgres errors.
//!
//! Everything goes through [`raise`]: a core [`Error`] (mapped to its
//! SQLSTATE) or a [`PgError`] built here, with an optional DETAIL and HINT.
//! Both are `#[track_caller]`, so an error's LOCATION (shown with
//! `\set VERBOSITY verbose`) is the file and line that raised it, not this
//! module.

use pg_automerge_core::Error;
use pgrx::pg_sys::panic::ErrorReport;
use pgrx::prelude::*;

/// Reported as the function name in LOCATION: `track_caller` gives the
/// caller's file and line but not its name.
const FUNCNAME: &str = "pg_automerge";

/// An ERROR to raise: SQLSTATE, a short primary message, and optional
/// DETAIL and HINT lines.
pub(crate) struct PgError {
    code: PgSqlErrorCode,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
}

impl PgError {
    pub(crate) fn new(code: PgSqlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            hint: None,
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<Error> for PgError {
    /// 22P02 for invalid input, 22000 for conflicting histories (a reused
    /// actor id), 22023 for an argument that does not fit the document,
    /// 54000 for a document an output cannot represent, 53400 for input
    /// over `pg_automerge.max_load_memory`, 0A000 for input in a form the
    /// extension does not take, XX000 for internal errors. See
    /// docs/src/pages/reference/error-codes.mdx.
    fn from(err: Error) -> Self {
        let code = match err {
            Error::InvalidInput(_) | Error::MissingDependencies(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION
            }
            Error::ConflictingChanges(_) => PgSqlErrorCode::ERRCODE_DATA_EXCEPTION,
            Error::InvalidParameter(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Error::LimitExceeded(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Error::LoadLimit(_) => PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
            Error::Unsupported(_) => PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            Error::Internal(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        };
        let mut pg = PgError::new(code, err.message());
        if let Some(detail) = err.detail() {
            pg = pg.detail(detail);
        }
        if let Some(hint) = err.hint() {
            pg = pg.hint(hint);
        }
        pg
    }
}

/// Raise `err` as a Postgres ERROR (never returns).
#[track_caller]
pub(crate) fn raise(err: impl Into<PgError>) -> ! {
    let err = err.into();
    let mut report = ErrorReport::new(err.code, err.message, FUNCNAME);
    if let Some(detail) = err.detail {
        report = report.set_detail(detail);
    }
    if let Some(hint) = err.hint {
        report = report.set_hint(hint);
    }
    report.report(PgLogLevel::ERROR);
    unreachable!("ereport(ERROR) does not return")
}

/// `.or_raise()`: the value, or the error raised.
pub(crate) trait OrRaise<T> {
    #[track_caller]
    fn or_raise(self) -> T;
}

impl<T> OrRaise<T> for Result<T, Error> {
    #[track_caller]
    fn or_raise(self) -> T {
        // A `match`, not `unwrap_or_else`: a closure would hide the caller.
        match self {
            Ok(value) => value,
            Err(err) => raise(err),
        }
    }
}

/// The ERROR for a NULL element in the array argument `name` (22004).
#[track_caller]
pub(crate) fn null_element(name: &str) -> ! {
    raise(PgError::new(
        PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
        format!("{name} must not contain NULL"),
    ))
}
