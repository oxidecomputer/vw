// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Integration point between the vw service API traits and the
//! [Dropshot API manager](https://crates.io/crates/dropshot-api-manager).
//!
//! The manager owns the OpenAPI documents under `openapi/` at the root of the
//! repository: `cargo openapi generate` writes them from the API traits in
//! `vw-api`, and `cargo openapi check` fails if what is on disk no longer
//! matches. That check also runs as a test in this crate, so a stale document
//! turns up in `cargo test` rather than in a client that has quietly drifted
//! from the service.
//!
//! `vw-api-client` generates its progenitor clients from the `-latest.json`
//! symlink the manager maintains for each API.

use anyhow::Context;
use camino::Utf8PathBuf;
use dropshot_api_manager::{Environment, ManagedApiConfig, ManagedApis};
use dropshot_api_manager_types::{ManagedApiMetadata, Versions};

/// How a developer invokes this binary. The manager quotes it back in its own
/// guidance, so it needs to match the alias in `.cargo/config.toml`.
const COMMAND: &str = "cargo openapi";

/// Where the managed documents live, relative to the repository root.
const OPENAPI_DIR: &str = "openapi";

/// The environment the manager runs in.
pub fn environment() -> anyhow::Result<Environment> {
    Environment::new(COMMAND, repo_root()?, OPENAPI_DIR)
}

/// Every OpenAPI document the manager is responsible for.
///
/// Both APIs are versioned rather than lockstep: `vw-api` declares its
/// supported versions with `api_versions!`, and clients out in the world will
/// not be upgraded in lockstep with the service.
pub fn all_apis() -> anyhow::Result<ManagedApis> {
    ManagedApis::new(vec![
        ManagedApiConfig {
            ident: "vw-user-api",
            versions: Versions::new_versioned(vw_api::supported_versions()),
            title: "VW user API",
            metadata: ManagedApiMetadata {
                description: Some(
                    "Manage your own vw build environments. Callers are \
                     identified by a Github access token.",
                ),
                ..Default::default()
            },
            api_description: vw_api::vw_user_api_mod::stub_api_description,
        },
        ManagedApiConfig {
            ident: "vw-admin-api",
            versions: Versions::new_versioned(vw_api::supported_versions()),
            title: "VW admin API",
            metadata: ManagedApiMetadata {
                description: Some(
                    "Manage every user's vw build environments. Restricted to \
                     the operators named in the service's --admin-users \
                     argument.",
                ),
                ..Default::default()
            },
            api_description: vw_api::vw_admin_api_mod::stub_api_description,
        },
        ManagedApiConfig {
            ident: "vw-sync-api",
            versions: Versions::new_versioned(vw_sync_api::supported_versions()),
            title: "VW agent API",
            metadata: ManagedApiMetadata {
                description: Some(
                    "Receive source on a build instance. Reachable only from \
                     vw-svc over the rack's internal network.",
                ),
                ..Default::default()
            },
            api_description: vw_sync_api::vw_sync_api_mod::stub_api_description,
        },
    ])
}

/// The root of the repository, one directory up from this crate.
fn repo_root() -> anyhow::Result<Utf8PathBuf> {
    let manifest_dir = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir
        .parent()
        .with_context(|| {
            format!(
                "{manifest_dir} has no parent directory to use as the \
                     repository root"
            )
        })?
        .to_owned())
}
