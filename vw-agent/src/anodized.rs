// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Getting anodizer's output to the instance that builds the driver.
//!
//! Anodization is an nvc pass over the design, so it happens on the vivado
//! instance; the driver is built here, on helios. The vivado instance publishes
//! what it generated to the environment's artifact store, and this fetches it
//! before cargo runs — never from the developer's machine, and never by
//! generating it a second time here.
//!
//! What is fetched is checked, not trusted. Both instances hold the same
//! workspace, so this one can work out what the output's fingerprint has to
//! be, and output made from other sources is refused: a driver built against
//! a record layout the hardware does not have would be wrong in ways nothing
//! downstream would catch.

use camino::Utf8Path;
use slog::{info, Logger};
use vw_api_types_versions::latest::S3Credentials;
use vw_lib::Anodized;
use vw_remote::Prepared;

use crate::artifacts;

/// Make anodizer's output available to the driver build, if the driver uses
/// it, and say how cargo finds it.
pub(crate) async fn prepare(
    root: &Utf8Path,
    credentials: Option<S3Credentials>,
    bucket: Option<String>,
    log: &Logger,
) -> Result<Prepared, String> {
    let dir = vw_lib::anodized_dir(root);
    let env = vec![(vw_lib::ANODIZED_ENV.to_owned(), dir.to_string())];

    // Only a driver that includes the output needs it. Asking every driver to
    // wait on a phase it does not use would make a workspace that merely tags
    // a record for its benches unable to build its driver until something
    // anodized it.
    let driver = root.join(vw_remote::driver::DIRECTORY);
    let mut crates = vec![driver.clone()];
    crates.extend(vw_lib::path_dependency_dirs(&driver));
    if !vw_lib::uses_anodized(&crates) {
        return Ok(Prepared {
            env,
            notes: Vec::new(),
        });
    }

    let status = vw_lib::anodized_status(root).map_err(|e| {
        format!("cannot work out what anodizer output the driver needs: {e}")
    })?;
    let path = match status {
        Anodized::Untagged => {
            return Err(format!(
                "the driver includes anodizer's output (it names \
                 `{}`), but nothing in the design is tagged \
                 `serialize_rust`, so there is nothing to include",
                vw_lib::ANODIZED_ENV
            ))
        }
        Anodized::Current { fingerprint, .. } => {
            return Ok(Prepared {
                env,
                notes: vec![format!(
                    "anodizer output: current ({fingerprint:016x})"
                )],
            })
        }
        Anodized::Stale { path, .. } => path,
    };

    let (Some(mut credentials), Some(bucket)) = (credentials, bucket) else {
        return Err(String::from(
            "the driver needs anodizer output, and this instance has not been \
             told where the environment's artifact store is to fetch it from",
        ));
    };
    credentials.bucket = bucket;

    let key = artifacts::key_for(root, &path);
    match artifacts::download(&credentials, &key, &path).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(String::from(
                "the driver needs anodizer output, and none has been \
                 published. `vw anodize` publishes it; `vw driver build` runs \
                 that first on its own, so if this came from there, the \
                 anodize step was skipped or failed.",
            ))
        }
        Err(e) => {
            return Err(format!(
                "cannot fetch anodizer output ({key}) from the \
                 artifact store: {e}"
            ))
        }
    }

    match vw_lib::anodized_status(root).map_err(|e| {
        format!("cannot work out what anodizer output the driver needs: {e}")
    })? {
        Anodized::Current { fingerprint, .. } => {
            info!(log, "fetched anodizer output";
                "key" => &key,
                "fingerprint" => format!("{fingerprint:016x}"),
            );
            Ok(Prepared {
                env,
                notes: vec![format!(
                    "anodizer output: fetched {key} \
                     ({fingerprint:016x})"
                )],
            })
        }
        Anodized::Stale {
            expected, found, ..
        } => Err(format!(
            "the published anodizer output was made from different sources \
             than this instance holds (it is {}, the sources here need \
             {expected:016x}). Sync, then run `vw anodize`.",
            found
                .map(|f| format!("{f:016x}"))
                .unwrap_or_else(|| String::from("unstamped")),
        )),
        Anodized::Untagged => unreachable!("checked above"),
    }
}
