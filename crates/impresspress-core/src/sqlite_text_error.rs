//! A failed SQLite statement, classified from the only thing two of our
//! adapters get back: the error's text.
//!
//! `wafer_core`'s `DatabaseService` contract names a write that duplicates a
//! primary or unique key [`DatabaseError::AlreadyExists`], so a caller can tell
//! "that key is taken" from a fault; the database handler turns it into
//! `ErrorCode::AlreadyExists` (a 409). The native backends read the driver's
//! structured code (`SQLITE_CONSTRAINT_UNIQUE`/`_PRIMARYKEY`, SQLSTATE
//! `23505`). Cloudflare D1 and the browser's sql.js have no structured code to
//! read — a `worker::Error` and a JS exception carry a message and nothing
//! else — so both adapters classify that message here, with one predicate,
//! rather than each keeping its own copy.
//!
//! SQLite spells both kinds of key violation the same way:
//! `UNIQUE constraint failed: <table>.<column>` — for a `UNIQUE` index, and
//! for a primary key too, whether it is a rowid alias or not. Every other
//! failure (a `NOT NULL` or `CHECK` violation, a missing table, a syntax
//! error) stays [`DatabaseError::Internal`], as it does on the native
//! backends.

use wafer_core::interfaces::database::service::DatabaseError;

/// The text SQLite puts in every primary- or unique-key violation.
const UNIQUE_VIOLATION: &str = "UNIQUE constraint failed";

/// `message` — the text of a failed statement — as a [`DatabaseError`]:
/// [`DatabaseError::AlreadyExists`] for a primary- or unique-key violation,
/// [`DatabaseError::Internal`] for anything else.
pub fn statement_error(message: String) -> DatabaseError {
    if message.contains(UNIQUE_VIOLATION) {
        DatabaseError::AlreadyExists(message)
    } else {
        DatabaseError::Internal(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The texts D1 and sql.js hand back for a taken key: D1 prefixes its own
    /// tag and appends the result code, sql.js passes SQLite's message
    /// through as is. Both are a taken key.
    #[test]
    fn a_unique_or_primary_key_violation_is_already_exists() {
        for text in [
            "D1_ERROR: UNIQUE constraint failed: impresspress__admin__roles.name: \
             SQLITE_CONSTRAINT",
            "UNIQUE constraint failed: t.id",
            "sql exec: JsValue(Error: UNIQUE constraint failed: t.a, t.b)",
        ] {
            assert!(
                matches!(
                    statement_error(text.into()),
                    DatabaseError::AlreadyExists(_)
                ),
                "{text}"
            );
        }
    }

    /// Every other failure is a fault, exactly as on the native backends.
    #[test]
    fn any_other_failure_stays_internal() {
        for text in [
            "NOT NULL constraint failed: t.name",
            "CHECK constraint failed: positive",
            "FOREIGN KEY constraint failed",
            "no such table: t",
        ] {
            assert!(
                matches!(statement_error(text.into()), DatabaseError::Internal(_)),
                "{text}"
            );
        }
    }
}
