//! Translations from the service's internal error types into the HTTP errors
//! the API surfaces.
//!
//! Database failures are all internal errors: the message goes into the
//! internal log rather than out to the caller, since it says more about the
//! service's storage than about the request.

use dropshot::ClientErrorStatusCode;

use crate::{auth, db, keys, oxide, relay};

impl From<auth::AuthError> for dropshot::HttpError {
    fn from(value: auth::AuthError) -> Self {
        let message = value.to_string();
        match value {
            // The caller has not established who they are.
            auth::AuthError::NoAuthToken | auth::AuthError::TokenRejected => {
                dropshot::HttpError::for_client_error(
                    None,
                    ClientErrorStatusCode::UNAUTHORIZED,
                    message,
                )
            }
            // The caller is known, but not entitled to this. Same status as
            // lacking project access: they are who they say, and it is not
            // enough.
            auth::AuthError::NoRedhawkProjectAccess
            | auth::AuthError::NotAnAdministrator(_) => {
                dropshot::HttpError::for_client_error(
                    None,
                    ClientErrorStatusCode::FORBIDDEN,
                    message,
                )
            }
            // We could not reach a verdict because Github did not cooperate.
            auth::AuthError::GithubError(_) => dropshot::HttpError {
                status_code: dropshot::ErrorStatusCode::BAD_GATEWAY,
                error_code: None,
                external_message: String::from(
                    "unable to verify credentials with github",
                ),
                internal_message: message,
                headers: None,
            },
        }
    }
}

impl From<db::ListError> for dropshot::HttpError {
    fn from(value: db::ListError) -> Self {
        dropshot::HttpError::for_internal_error(value.to_string())
    }
}

impl From<oxide::ImageError> for dropshot::HttpError {
    fn from(value: oxide::ImageError) -> Self {
        let message = value.to_string();
        match value {
            // The caller named an image that is not there, or asked for a kind
            // the rack has no image for. Either way they can act on it.
            oxide::ImageError::NoSuchImage(_)
            | oxide::ImageError::NoMatchingImage(_) => {
                dropshot::HttpError::for_bad_request(None, message)
            }
            // Something about this service or the rack behind it, not the
            // request.
            oxide::ImageError::List(_) => {
                dropshot::HttpError::for_internal_error(message)
            }
        }
    }
}

impl From<oxide::SessionError> for dropshot::HttpError {
    fn from(value: oxide::SessionError) -> Self {
        // Either way the caller did nothing wrong: the service is missing its
        // Oxide configuration, or cannot build a client from it.
        dropshot::HttpError::for_internal_error(value.to_string())
    }
}

impl From<keys::KeyError> for dropshot::HttpError {
    fn from(value: keys::KeyError) -> Self {
        // Nothing the caller did; the service could not make a key.
        dropshot::HttpError::for_internal_error(value.to_string())
    }
}

impl From<relay::RelayError> for dropshot::HttpError {
    fn from(value: relay::RelayError) -> Self {
        let message = value.to_string();
        match value {
            // The caller named something that is not theirs or not there.
            relay::RelayError::NoSuchEnvironment => {
                dropshot::HttpError::for_not_found(None, message)
            }
            // The environment is real but not ready. Not an error on anyone's
            // part — an environment spends its first minute like this — so it
            // is worth a status a client can wait on rather than give up at.
            relay::RelayError::NoInstance { .. }
            | relay::RelayError::NoAddress { .. } => {
                dropshot::HttpError::for_unavail(None, message)
            }
            // The instance said no, or could not be reached. Either way the
            // caller's request was fine and the detail is in the log.
            relay::RelayError::Agent { .. } => dropshot::HttpError {
                status_code: dropshot::ErrorStatusCode::BAD_GATEWAY,
                error_code: None,
                external_message: message.clone(),
                internal_message: message,
                headers: None,
            },
            relay::RelayError::Db(_) | relay::RelayError::Client(_) => {
                dropshot::HttpError::for_internal_error(message)
            }
        }
    }
}

impl From<db::CreateError> for dropshot::HttpError {
    fn from(value: db::CreateError) -> Self {
        match value {
            db::CreateError::EnvironmentAlreadyExists => {
                dropshot::HttpError::for_client_error(
                    None,
                    ClientErrorStatusCode::CONFLICT,
                    value.to_string(),
                )
            }
            db::CreateError::Encode(_)
            | db::CreateError::Transaction(_)
            | db::CreateError::Table(_)
            | db::CreateError::Storage(_)
            | db::CreateError::Commit(_) => {
                dropshot::HttpError::for_internal_error(value.to_string())
            }
        }
    }
}

impl From<db::DeleteError> for dropshot::HttpError {
    fn from(value: db::DeleteError) -> Self {
        match value {
            db::DeleteError::NoSuchEnvironment => {
                dropshot::HttpError::for_not_found(None, value.to_string())
            }
            db::DeleteError::Transaction(_)
            | db::DeleteError::Table(_)
            | db::DeleteError::Storage(_)
            | db::DeleteError::Commit(_) => {
                dropshot::HttpError::for_internal_error(value.to_string())
            }
        }
    }
}

impl From<db::GetError> for dropshot::HttpError {
    fn from(value: db::GetError) -> Self {
        match value {
            db::GetError::NoSuchEnvironment => {
                dropshot::HttpError::for_not_found(None, value.to_string())
            }
            db::GetError::Transaction(_)
            | db::GetError::Table(_)
            | db::GetError::Storage(_)
            | db::GetError::Decode(_) => {
                dropshot::HttpError::for_internal_error(value.to_string())
            }
        }
    }
}
