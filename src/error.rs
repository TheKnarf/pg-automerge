//! Raising Postgres errors from core errors.

use pg_automerge_core::Error;
use pgrx::prelude::*;

/// Raise a core error as a Postgres ERROR (never returns).
pub(crate) fn raise(err: Error) -> ! {
    let code = match err {
        Error::InvalidInput(_) => PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION,
        Error::InvalidParameter(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        Error::Internal(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    };
    pgrx::pg_sys::panic::ErrorReport::new(code, err.to_string(), pgrx::function_name!())
        .report(PgLogLevel::ERROR);
    unreachable!("ereport(ERROR) does not return")
}

pub(crate) trait OrRaise<T> {
    fn or_raise(self) -> T;
}

impl<T> OrRaise<T> for Result<T, Error> {
    fn or_raise(self) -> T {
        self.unwrap_or_else(|e| raise(e))
    }
}

/// Raise an ERROR with `code` (never returns).
pub(crate) fn fail(code: PgSqlErrorCode, msg: String) -> ! {
    pgrx::pg_sys::panic::ErrorReport::new(code, msg, pgrx::function_name!())
        .report(PgLogLevel::ERROR);
    unreachable!("ereport(ERROR) does not return")
}
