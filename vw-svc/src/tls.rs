//! TLS configuration for the API servers, and keeping it current.
//!
//! In production the certificate comes from Let's Encrypt, which means it is
//! replaced every couple of months by a `certbot renew` that runs from a timer
//! with nobody watching. Restarting to pick one up would be the easy answer
//! and the wrong one: this service relays the connections a build runs over,
//! so a restart ends somebody's synthesis run, REPL session or download partway
//! through, for a certificate that had two weeks left on it.
//!
//! So the certificate files are watched instead, and the running servers are
//! handed the replacement between handshakes. Nothing on either side of an
//! established connection notices, and there is nothing to run but certbot.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use dropshot::{ConfigTls, HttpServer, ServerContext};
use slog::{error, info, warn, Logger};
use slog_error_chain::InlineErrorChain;
use tokio::sync::watch;

use crate::ServerArgs;

/// How often the certificate files are checked for replacement.
///
/// A renewal happens twice a year and nothing is waiting on it, so this is set
/// by what it costs rather than by how soon it must be noticed: two `stat`
/// calls, and a parse only when they say something changed.
const POLL: Duration = Duration::from_secs(60);

/// Error conditions for assembling the TLS configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsError {
    #[error("certificate file {0} does not exist")]
    NoCertFile(Utf8PathBuf),
    #[error("key file {0} does not exist")]
    NoKeyFile(Utf8PathBuf),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("parsing {0}")]
    Parse(Utf8PathBuf, #[source] std::io::Error),
    #[error("{0} contains no certificate")]
    NoCertificates(Utf8PathBuf),
    #[error("{0} contains no private key")]
    NoPrivateKey(Utf8PathBuf),
    #[error(
        "{cert_file} and {key_file} are not a certificate and key this can serve"
    )]
    Unusable {
        cert_file: Utf8PathBuf,
        key_file: Utf8PathBuf,
        #[source]
        source: rustls::Error,
    },
}

/// The TLS the API servers run with, and a channel carrying its replacements.
///
/// Cloned per server: both serve the same certificate, and each holds its own
/// acceptor that has to be told about a new one separately.
#[derive(Clone)]
pub(crate) struct Tls {
    /// What a server starts with.
    initial: ConfigTls,
    /// Certificates that replaced it, as they are noticed.
    updates: watch::Receiver<ConfigTls>,
}

/// The TLS configuration both API servers should run with, or `None` when the
/// service was not asked to serve HTTPS.
///
/// The certificate and key are read and parsed here rather than left to
/// dropshot, so that anything wrong with them is a startup failure naming the
/// path at fault instead of turning up later as a handshake failure. Starting
/// the watch here too means the load that runs on every renewal is this one.
pub(crate) fn config(
    args: &ServerArgs,
    log: &Logger,
) -> Result<Option<Tls>, TlsError> {
    if !args.tls {
        return Ok(None);
    }
    if !args.cert_file.exists() {
        return Err(TlsError::NoCertFile(args.cert_file.clone()));
    }
    if !args.key_file.exists() {
        return Err(TlsError::NoKeyFile(args.key_file.clone()));
    }

    let initial = load(&args.cert_file, &args.key_file)?;

    let (tx, updates) = watch::channel(initial.clone());
    tokio::spawn(watch_for_renewals(
        args.cert_file.clone(),
        args.key_file.clone(),
        tx,
        log.new(slog::o!("component" => "tls")),
    ));

    Ok(Some(Tls { initial, updates }))
}

/// Read the certificate and key, and build what a server can be handed.
///
/// [`ConfigTls::Dynamic`] rather than [`ConfigTls::AsFile`] on purpose. Given
/// paths, a server reads and parses them itself, and on the refresh path
/// dropshot does that behind an `unwrap`. Given an already-built configuration
/// there is nothing left for it to fail at.
///
/// The parsing is done here rather than by handing dropshot the bytes for the
/// same reason: its conversion returns an error for a file it cannot read, but
/// *panics* on one it can read and cannot use — an empty certificate, or a key
/// belonging to a different certificate. Both are ordinary things to see for a
/// moment during a renewal. A panic in the task that watches for renewals does
/// not stop the service; it stops the watching, silently and for good, which
/// is worse.
///
/// The result is otherwise exactly what dropshot builds, ALPN included, so a
/// renewed certificate changes nothing about how connections are negotiated.
fn load(
    cert_file: &Utf8Path,
    key_file: &Utf8Path,
) -> Result<ConfigTls, TlsError> {
    let cert_pem = std::fs::read(cert_file)
        .map_err(|e| TlsError::Read(cert_file.to_owned(), e))?;
    let key_pem = std::fs::read(key_file)
        .map_err(|e| TlsError::Read(key_file.to_owned(), e))?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Parse(cert_file.to_owned(), e))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates(cert_file.to_owned()));
    }

    // Any of the three encodings a private key comes in, where dropshot takes
    // only PKCS#8. Certbot writes PKCS#8, so this is about not failing
    // mysteriously on a key that came from somewhere else.
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| TlsError::Parse(key_file.to_owned(), e))?
        .ok_or_else(|| TlsError::NoPrivateKey(key_file.to_owned()))?;

    // Checks that the key belongs to the certificate, which is what catches a
    // renewal read halfway through.
    let mut raw = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|source| TlsError::Unusable {
            cert_file: cert_file.to_owned(),
            key_file: key_file.to_owned(),
            source,
        })?;
    raw.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(ConfigTls::Dynamic(raw))
}

/// Notice when certbot has replaced the certificate, and pass it on.
///
/// Polled rather than watched for filesystem events because of the shape of
/// the thing being watched. Certbot keeps every certificate it has issued in
/// `archive/` and points a symlink in `live/` at the current one, so the
/// configured path is a symlink that is never itself modified — it is
/// replaced, and the writes land in a directory nobody configured. A `stat` a
/// minute is immune to all of that, and a renewal noticed a minute late is a
/// renewal noticed on time.
async fn watch_for_renewals(
    cert_file: Utf8PathBuf,
    key_file: Utf8PathBuf,
    tx: watch::Sender<ConfigTls>,
    log: Logger,
) {
    let mut current = stamp(&cert_file, &key_file);

    loop {
        tokio::time::sleep(POLL).await;

        let latest = stamp(&cert_file, &key_file);
        if latest == current {
            continue;
        }

        match load(&cert_file, &key_file) {
            Ok(config) => {
                info!(log, "certificate replaced";
                    "cert_file" => %cert_file,
                    "key_file" => %key_file,
                );
                current = latest;
                // Fails only once every server has gone, by which point the
                // service is on its way down and has no use for this.
                let _ = tx.send(config);
            }
            Err(e) => {
                // `current` is deliberately left behind, so the next pass sees
                // the files as still changed and tries again. The likeliest
                // cause is catching a renewal mid-flight, with the certificate
                // replaced and the key not yet — which resolves itself within
                // the minute.
                warn!(
                    log,
                    "cannot serve the replaced certificate, keeping the \
                     current one";
                    InlineErrorChain::new(&e),
                );
            }
        }
    }
}

/// What is compared to decide the files have been replaced.
///
/// Size and modification time of each, followed through the symlinks, which is
/// all that is needed: a renewal writes a different certificate at a later
/// time. Hashing the contents would be more precise about a case that does not
/// arise, and this is only ever the decision to look closer.
///
/// A file that cannot be stat'd reads as `None` rather than an error. A
/// certificate that has briefly gone missing is not a reason to bring the
/// service down, and it compares unequal when it comes back.
fn stamp(
    cert_file: &Utf8Path,
    key_file: &Utf8Path,
) -> [Option<(u64, SystemTime)>; 2] {
    [cert_file, key_file].map(|path| {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    })
}

/// The configuration a server should start with.
pub(crate) fn initial(tls: Option<&Tls>) -> Option<ConfigTls> {
    tls.map(|tls| tls.initial.clone())
}

/// Hand `server` every certificate that replaces the one it started with.
///
/// Returns immediately; the following is a task that lives as long as the
/// service does. Does nothing when there is no TLS, so that a caller does not
/// have to ask twice whether there is any.
pub(crate) fn follow_renewals<C: ServerContext>(
    server: Arc<HttpServer<C>>,
    tls: Option<Tls>,
    log: Logger,
) {
    let Some(Tls { mut updates, .. }) = tls else {
        return;
    };

    tokio::spawn(async move {
        // Ends when the watch is dropped, which is when the service is done.
        while updates.changed().await.is_ok() {
            let config = updates.borrow_and_update().clone();
            match server.refresh_tls(&config).await {
                // Connections already up are untouched and keep the old
                // certificate for as long as they live; everything from here
                // on gets the new one.
                Ok(()) => info!(log, "now serving the replaced certificate"),
                // Only reachable on a server built without TLS, and nothing
                // subscribes without one.
                Err(e) => {
                    error!(log, "cannot install the replaced certificate";
                        "error" => e,
                    )
                }
            }
        }
    });
}

/// The URL scheme the servers answer on, for log messages.
pub(crate) fn scheme(args: &ServerArgs) -> &'static str {
    if args.tls {
        "https"
    } else {
        "http"
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Every way a certificate can be unusable, written where `load` will find
    /// it.
    fn write(contents: &[u8]) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        let path = root.join("pem");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn an_unusable_certificate_is_an_error_and_not_a_panic() {
        // This is load-bearing rather than tidy. `load` runs on a timer in a
        // task nobody is watching, and a panic there does not stop the
        // service — it stops renewals, silently, until somebody restarts. Each
        // of these is a state a certificate directory really passes through
        // while certbot is writing to it.
        for (what, contents) in [
            ("an empty file", &b""[..]),
            ("a file that is not PEM at all", b"not a certificate"),
            (
                "a PEM header and nothing else",
                b"-----BEGIN CERTIFICATE-----",
            ),
            (
                "a truncated certificate",
                b"-----BEGIN CERTIFICATE-----\nMIIB\n",
            ),
        ] {
            let (_dir, path) = write(contents);

            let loaded = load(&path, &path);

            assert!(loaded.is_err(), "{what} should be refused, not accepted");
        }
    }

    #[test]
    fn a_certificate_that_is_not_there_is_an_error() {
        // The window while certbot has unlinked one symlink and not yet
        // written the next.
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let missing = root.join("nothing.pem");

        assert!(load(&missing, &missing).is_err());
    }

    #[test]
    fn nothing_new_leaves_the_stamp_alone() {
        // The whole poll rests on this: an untouched file must compare equal,
        // or every pass would reload a certificate that had not changed.
        let (_dir, path) = write(b"whatever");

        assert_eq!(stamp(&path, &path), stamp(&path, &path));
    }

    #[test]
    fn a_missing_file_stamps_rather_than_fails() {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let missing = root.join("nothing.pem");

        assert_eq!(stamp(&missing, &missing), [None, None]);
    }
}
