//! Turning the sync engine's errors into the ones the API surfaces.
//!
//! The split that matters is between a caller who sent something wrong and an
//! instance that is in no state to help. A manifest naming a path outside the
//! tree, or content that does not hash to the digest it was sent under, is the
//! caller's mistake and worth saying so precisely — retrying the same request
//! will fail the same way. A directory that cannot be written is not, and the
//! detail belongs in the log rather than in the response.
//!
//! Free functions rather than `From` impls: both the engine's errors and
//! dropshot's are foreign to this crate, so there is no impl to write.

use dropshot::{ClientErrorStatusCode, HttpError};

use vw_sync::{ApplyError, StoreError};

pub(crate) fn apply_error(value: ApplyError) -> HttpError {
    let message = value.to_string();
    match value {
        // The manifest itself is wrong.
        ApplyError::UnsafePath(_) => HttpError::for_bad_request(None, message),
        // The manifest is fine, but it was committed before everything it
        // names had been delivered. The message names the digest, so a caller
        // that skipped a blob can tell which one.
        ApplyError::MissingContent { .. } => HttpError::for_client_error(
            None,
            ClientErrorStatusCode::CONFLICT,
            message,
        ),
        ApplyError::Store(e) => store_error(e),
        // Something about this instance.
        ApplyError::Scan(..)
        | ApplyError::CreateDir(..)
        | ApplyError::Write(..)
        | ApplyError::Remove(..) => HttpError::for_internal_error(message),
    }
}

pub(crate) fn store_error(value: StoreError) -> HttpError {
    let message = value.to_string();
    match value {
        // Both are the caller's to fix, and both are worth refusing loudly: a
        // digest that is not a digest is trying to become a path it should
        // not, and content that does not match its digest would be handed back
        // later under a name that lies about it.
        StoreError::MalformedDigest(_) | StoreError::DigestMismatch(_) => {
            HttpError::for_bad_request(None, message)
        }
        StoreError::CreateDir(..)
        | StoreError::Write(..)
        | StoreError::Read(..)
        | StoreError::Empty(..) => HttpError::for_internal_error(message),
    }
}
