//! TLS configuration for the API servers.

use camino::Utf8PathBuf;
use dropshot::ConfigTls;

use crate::ServerArgs;

/// Error conditions for assembling the TLS configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsError {
    #[error("certificate file {0} does not exist")]
    NoCertFile(Utf8PathBuf),
    #[error("key file {0} does not exist")]
    NoKeyFile(Utf8PathBuf),
}

/// The TLS configuration both API servers should run with, or `None` when the
/// service was not asked to serve HTTPS.
///
/// The certificate and key are checked for existence here rather than left to
/// dropshot, so that a typo in `--cert-file` fails at startup naming the path
/// that is wrong instead of turning up later as a handshake failure.
pub(crate) fn config(args: &ServerArgs) -> Result<Option<ConfigTls>, TlsError> {
    if !args.tls {
        return Ok(None);
    }
    if !args.cert_file.exists() {
        return Err(TlsError::NoCertFile(args.cert_file.clone()));
    }
    if !args.key_file.exists() {
        return Err(TlsError::NoKeyFile(args.key_file.clone()));
    }

    Ok(Some(ConfigTls::AsFile {
        cert_file: args.cert_file.clone().into_std_path_buf(),
        key_file: args.key_file.clone().into_std_path_buf(),
    }))
}

/// The URL scheme the servers answer on, for log messages.
pub(crate) fn scheme(args: &ServerArgs) -> &'static str {
    if args.tls {
        "https"
    } else {
        "http"
    }
}
