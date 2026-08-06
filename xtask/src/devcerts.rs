// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Generate a self-signed certificate for running `vw-svc --tls` locally.
//!
//! ```text
//! cargo xtask devcerts
//! cargo run -p vw-svc -- serve --tls \
//!     --cert-file target/devcert/cert.pem \
//!     --key-file target/devcert/key.pem
//! vw cloud --url https://localhost:2727 --insecure list
//! ```
//!
//! The certificate is its own issuer and is trusted by nothing, so clients
//! still have to be told to accept it — `--insecure` for `vw cloud`, `-k` for
//! curl. It exists so the TLS path can be exercised, not to provide any real
//! assurance.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

use crate::DevcertArgs;

/// Where the certificate goes when the caller does not say.
///
/// Under `target/` so it is disposable and already ignored by git.
pub const DEFAULT_DIR: &str = "target/devcert";

/// Names every generated certificate is valid for, so that a service reached
/// as `localhost` and one reached as `127.0.0.1` both work out of the box.
const DEFAULT_SUBJECT_ALT_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// What the certificate calls itself, so it is recognizable in a browser or in
/// `openssl x509 -text` output.
const COMMON_NAME: &str = "vw development certificate";

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";

#[derive(Debug, thiserror::Error)]
pub enum DevcertError {
    #[error("{0} already exists; pass --force to replace it")]
    Exists(Utf8PathBuf),
    #[error("creating {0}: {1}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}: {1}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("generating certificate: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("a certificate cannot be valid for {0} days")]
    Validity(u16),
}

pub fn run(args: DevcertArgs) -> Result<(), DevcertError> {
    let cert_path = args.dir.join(CERT_FILE);
    let key_path = args.dir.join(KEY_FILE);

    // Check both before writing either, so a refusal does not leave behind a
    // certificate whose key was never replaced.
    if !args.force {
        for path in [&cert_path, &key_path] {
            if path.exists() {
                return Err(DevcertError::Exists(path.clone()));
            }
        }
    }

    let subject_alt_names: Vec<String> = DEFAULT_SUBJECT_ALT_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .chain(args.subject_alt_names.iter().cloned())
        .collect();

    let key = KeyPair::generate()?;
    let cert = params(&subject_alt_names, args.days)?.self_signed(&key)?;

    fs::create_dir_all(&args.dir)
        .map_err(|e| DevcertError::CreateDir(args.dir.clone(), e))?;
    write(&cert_path, cert.pem().as_bytes(), false)?;
    // The key is only good for a throwaway service, but there is no reason to
    // leave it world readable.
    write(&key_path, key.serialize_pem().as_bytes(), true)?;

    println!("wrote {cert_path}");
    println!("wrote {key_path}");
    println!("valid for {} days, for:", args.days);
    for name in &subject_alt_names {
        println!("  {name}");
    }
    println!();
    println!("run the service with it:");
    println!(
        "  cargo run -p vw-svc -- serve --tls \\\n    \
         --cert-file {cert_path} --key-file {key_path}"
    );
    println!("clients must be told to accept it, e.g. `vw cloud --insecure`");

    Ok(())
}

/// Certificate parameters for a server certificate valid for `days` days.
fn params(
    subject_alt_names: &[String],
    days: u16,
) -> Result<CertificateParams, DevcertError> {
    let mut params = CertificateParams::new(subject_alt_names.to_vec())?;

    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now
        .checked_add(Duration::days(days.into()))
        .ok_or(DevcertError::Validity(days))?;

    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, COMMON_NAME);
    params.distinguished_name = name;

    // Say outright that this is a leaf, not an authority. Certificates that
    // omit this are rejected by rustls as `CaUsedAsEndEntity`, which is a
    // confusing way to learn that a hand-rolled cert was wrong.
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    Ok(params)
}

/// Write `contents` to `path`, restricting it to the current user when
/// `private`.
fn write(
    path: &Utf8Path,
    contents: &[u8],
    private: bool,
) -> Result<(), DevcertError> {
    fs::write(path, contents)
        .map_err(|e| DevcertError::Write(path.to_owned(), e))?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| DevcertError::Write(path.to_owned(), e))?;
    }
    #[cfg(not(unix))]
    let _ = private;

    Ok(())
}
