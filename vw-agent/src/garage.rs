// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Bringing up the object store an environment's artifacts land in.
//!
//! Run on the artifact instance. Garage is configured, started and bootstrapped
//! here rather than provisioned from outside, which means no credential is ever
//! shared in advance: the admin token is generated on this machine, written to
//! a file only this machine reads, and never leaves it. What does leave is one
//! S3 access key, handed to `vw-svc` when it asks, which passes it to the
//! instance that has artifacts to upload.
//!
//! Everything here is written to survive a restart. The instance can reboot,
//! the agent can be upgraded, and the key stays the same — it is recorded on
//! disk beside garage's own state, so nothing downstream has to be told about
//! it again.

use camino::{Utf8Path, Utf8PathBuf};
use rand::Rng;
use serde::{Deserialize, Serialize};
use slog::{info, Logger};
use vw_api_types_versions::latest::S3Credentials;

/// Where garage listens, and where its state lives.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// Directory for garage's config, metadata and data.
    pub(crate) dir: Utf8PathBuf,
    /// The S3 API port. 3900 is garage's own default and what everything
    /// downstream expects.
    pub(crate) s3_port: u16,
    /// The admin API port, reachable only from this machine.
    pub(crate) admin_port: u16,
    /// The internal RPC port garage uses to talk to itself.
    pub(crate) rpc_port: u16,
    /// How much of this instance's disk garage may use.
    pub(crate) capacity: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GarageError {
    #[error("creating {0}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("garage is not installed on this instance")]
    NotInstalled(#[source] std::io::Error),
    #[error("garage did not become ready within {0:?}")]
    NeverReady(std::time::Duration),
    #[error("`garage {0}` failed: {1}")]
    Command(String, String),
    #[error("talking to garage's admin api")]
    Admin(#[source] reqwest::Error),
    #[error("garage's admin api answered {status}: {body}")]
    AdminRefused { status: u16, body: String },
    #[error("garage's answer to {0} was not what was expected")]
    Unexpected(String),
}

/// How long to wait for a fresh garage to start answering.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A running garage, and the key that opens it.
pub(crate) struct Store {
    /// The key, and the bucket each kind of instance writes to.
    pub(crate) credentials: S3Credentials,
    /// The bucket for each kind, by the name the caller asks for.
    pub(crate) buckets: std::collections::BTreeMap<String, String>,
    /// Kept so garage is torn down with the agent rather than outliving it.
    _child: tokio::process::Child,
}

/// Configure, start and bootstrap garage, returning the key for this
/// environment's bucket.
///
/// Safe to run again on a machine that has already done it: the config, the
/// cluster layout and the key are each created only if absent, so a reboot
/// comes back to the same store with the same credentials.
pub(crate) async fn start(
    environment: &str,
    settings: &Settings,
    log: &Logger,
) -> Result<Store, GarageError> {
    for directory in [
        &settings.dir,
        &settings.dir.join("meta"),
        &settings.dir.join("data"),
    ] {
        std::fs::create_dir_all(directory)
            .map_err(|e| GarageError::CreateDir(directory.clone(), e))?;
    }

    let config = settings.dir.join("garage.toml");
    let secrets = ensure_config(&config, settings)?;

    info!(log, "starting garage"; "config" => %config);
    let child = tokio::process::Command::new("garage")
        .args(["-c", config.as_str(), "server"])
        .kill_on_drop(true)
        .spawn()
        .map_err(GarageError::NotInstalled)?;

    let admin = Admin {
        base: format!("http://127.0.0.1:{}", settings.admin_port),
        token: secrets.admin_token.clone(),
        client: reqwest::Client::new(),
    };
    admin.wait_until_ready(log).await?;
    ensure_layout(&config, &admin, settings, log).await?;

    let (credentials, buckets) =
        ensure_credentials(environment, settings, &admin, log).await?;

    Ok(Store {
        credentials,
        buckets,
        _child: child,
    })
}

/// The two secrets garage's own config needs.
struct Secrets {
    admin_token: String,
}

/// Write garage's config if it is not already there, and read back the admin
/// token either way.
///
/// Not overwritten on a restart: the admin token in a config that garage's
/// on-disk state was created under is the one that still opens it, and
/// generating a fresh one every boot would lock us out of our own store.
fn ensure_config(
    path: &Utf8Path,
    settings: &Settings,
) -> Result<Secrets, GarageError> {
    if path.is_file() {
        let existing = std::fs::read_to_string(path)
            .map_err(|e| GarageError::Read(path.to_owned(), e))?;
        let admin_token = existing
            .lines()
            .find_map(|line| line.strip_prefix("admin_token = "))
            .map(|value| value.trim_matches('"').to_owned())
            .ok_or_else(|| {
                GarageError::Unexpected(format!("{path} has no admin_token"))
            })?;
        return Ok(Secrets { admin_token });
    }

    let admin_token = secret();
    let rpc_secret = secret();
    let config = format!(
        "metadata_dir = \"{dir}/meta\"\n\
         data_dir = \"{dir}/data\"\n\
         db_engine = \"lmdb\"\n\
         replication_factor = 1\n\
         \n\
         rpc_bind_addr = \"127.0.0.1:{rpc}\"\n\
         rpc_public_addr = \"127.0.0.1:{rpc}\"\n\
         rpc_secret = \"{rpc_secret}\"\n\
         \n\
         [s3_api]\n\
         s3_region = \"garage\"\n\
         api_bind_addr = \"[::]:{s3}\"\n\
         root_domain = \".s3.garage\"\n\
         \n\
         [admin]\n\
         api_bind_addr = \"127.0.0.1:{admin}\"\n\
         admin_token = \"{admin_token}\"\n",
        dir = settings.dir,
        rpc = settings.rpc_port,
        s3 = settings.s3_port,
        admin = settings.admin_port,
    );

    std::fs::write(path, config)
        .map_err(|e| GarageError::Write(path.to_owned(), e))?;
    // The file holds two secrets and garage itself refuses to read a
    // world-readable one.
    restrict(path)?;

    Ok(Secrets { admin_token })
}

/// Give a single node a share of the cluster, so garage will serve S3.
///
/// A garage with no layout accepts no objects, which is the one step a fresh
/// instance cannot skip. Done through garage's own CLI rather than the admin
/// API: the layout request type is not the layout response type, and the CLI
/// is the interface garage documents for this. Nothing is read back from it —
/// only whether it worked — so there is no output to parse.
async fn ensure_layout(
    config: &Utf8Path,
    admin: &Admin,
    settings: &Settings,
    log: &Logger,
) -> Result<(), GarageError> {
    if admin.layout_version().await? > 0 {
        info!(log, "garage already has a cluster layout");
        return Ok(());
    }

    let node = admin.node_id().await?;
    info!(log, "assigning a cluster layout"; "node" => &node);

    run_garage(
        config,
        &[
            "layout",
            "assign",
            "-z",
            "vw",
            "-c",
            &settings.capacity,
            &node,
        ],
    )
    .await?;
    run_garage(config, &["layout", "apply", "--version", "1"]).await?;

    Ok(())
}

/// The kinds of instance that produce artifacts, and so have a bucket.
///
/// Helios has one before it has anything to put in it. A bucket costs nothing
/// standing empty, and creating it now means the day the driver build starts
/// producing something there is nowhere for it to be missing.
pub(crate) const KINDS: [&str; 2] = ["vivado", "helios"];

/// What was minted, remembered so a reboot comes back to the same store.
#[derive(Serialize, Deserialize)]
struct Minted {
    credentials: S3Credentials,
    buckets: std::collections::BTreeMap<String, String>,
}

/// The key and buckets for this environment, created if this is the first boot.
///
/// One key with access to every bucket rather than one key each: they are all
/// reached from inside one VPC by instances of one environment, so a second
/// key would be ceremony without a boundary behind it.
async fn ensure_credentials(
    environment: &str,
    settings: &Settings,
    admin: &Admin,
    log: &Logger,
) -> Result<
    (S3Credentials, std::collections::BTreeMap<String, String>),
    GarageError,
> {
    let path = settings.dir.join("credentials.json");
    if path.is_file() {
        let stored = std::fs::read_to_string(&path)
            .map_err(|e| GarageError::Read(path.clone(), e))?;
        let minted: Minted = serde_json::from_str(&stored)
            .map_err(|_| GarageError::Unexpected(path.to_string()))?;
        info!(log, "reusing the store credentials from a previous run";
            "buckets" => minted.buckets.len(),
        );
        return Ok((minted.credentials, minted.buckets));
    }

    let key = admin.ensure_key(&format!("vw-{environment}"), log).await?;

    // A bucket per environment per kind, named for both. Even though an
    // artifact instance serves one environment today, a name that says which
    // one keeps the objects legible if a store is ever shared.
    let mut buckets = std::collections::BTreeMap::new();
    for kind in KINDS {
        let bucket = format!("{kind}-{environment}");
        // Adopted if it is already there. That happens when this record was
        // lost but the store's own state was not — a disk restored from a
        // snapshot, a file removed by hand. Failing instead would leave an
        // instance that cannot serve the artifacts sitting right there in a
        // bucket, and creating a second bucket would orphan them. Neither is
        // as good as picking up where we left off.
        let bucket_id = admin.ensure_bucket(&bucket, log).await?;
        admin.allow(&bucket_id, &key.access_key_id).await?;
        buckets.insert(kind.to_owned(), bucket);
    }

    let credentials = S3Credentials {
        // Left for the caller to fill in: this instance cannot see which of
        // its addresses another instance can reach it on. The port it can
        // see, because it chose it.
        endpoint: String::new(),
        port: settings.s3_port,
        region: "garage".to_owned(),
        // Filled in per caller, which is the only thing that differs between
        // one instance's view of this store and another's.
        bucket: String::new(),
        access_key_id: key.access_key_id,
        secret_access_key: key.secret_access_key,
    };

    let minted = Minted {
        credentials: credentials.clone(),
        buckets: buckets.clone(),
    };
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&minted)
            .map_err(|_| GarageError::Unexpected(path.to_string()))?,
    )
    .map_err(|e| GarageError::Write(path.clone(), e))?;
    restrict(&path)?;

    info!(log, "created the store for this environment";
        "buckets" => format!("{:?}", buckets.values().collect::<Vec<_>>()),
    );

    Ok((credentials, buckets))
}

/// Run one garage subcommand against our config.
async fn run_garage(
    config: &Utf8Path,
    args: &[&str],
) -> Result<(), GarageError> {
    let output = tokio::process::Command::new("garage")
        .args(["-c", config.as_str()])
        .args(args)
        .output()
        .await
        .map_err(GarageError::NotInstalled)?;

    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&output.stderr).into_owned();
        detail.push_str(&String::from_utf8_lossy(&output.stdout));
        return Err(GarageError::Command(args.join(" "), detail));
    }

    Ok(())
}

/// Garage's admin API, which only this machine can reach.
struct Admin {
    base: String,
    token: String,
    client: reqwest::Client,
}

/// What creating a key gave us.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedKey {
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedBucket {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Permissions {
    read: bool,
    write: bool,
    owner: bool,
}

impl Admin {
    async fn wait_until_ready(&self, log: &Logger) -> Result<(), GarageError> {
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        loop {
            if self.layout_version().await.is_ok() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(GarageError::NeverReady(READY_TIMEOUT));
            }
            slog::debug!(log, "waiting for garage to come up");
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    async fn get(
        &self,
        endpoint: &str,
    ) -> Result<serde_json::Value, GarageError> {
        let response = self
            .client
            .get(format!("{}/v2/{endpoint}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(GarageError::Admin)?;
        Self::decode(endpoint, response).await
    }

    async fn post<T: Serialize>(
        &self,
        endpoint: &str,
        body: &T,
    ) -> Result<serde_json::Value, GarageError> {
        let response = self
            .client
            .post(format!("{}/v2/{endpoint}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(GarageError::Admin)?;
        Self::decode(endpoint, response).await
    }

    async fn decode(
        endpoint: &str,
        response: reqwest::Response,
    ) -> Result<serde_json::Value, GarageError> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(GarageError::AdminRefused {
                status: status.as_u16(),
                body,
            });
        }
        response
            .json()
            .await
            .map_err(|_| GarageError::Unexpected(endpoint.to_owned()))
    }

    async fn layout_version(&self) -> Result<u64, GarageError> {
        let status = self.get("GetClusterStatus").await?;
        status
            .get("layoutVersion")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| GarageError::Unexpected("GetClusterStatus".into()))
    }

    async fn node_id(&self) -> Result<String, GarageError> {
        let status = self.get("GetClusterStatus").await?;
        status
            .get("nodes")
            .and_then(|nodes| nodes.get(0))
            .and_then(|node| node.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| GarageError::Unexpected("GetClusterStatus".into()))
    }

    /// The key with this name, creating one only if there is not one already.
    ///
    /// Adopted rather than replaced for the same reason a bucket is, plus one
    /// of its own: a new key every time this record was lost would leave the
    /// old ones behind with permission on the bucket, so a store would collect
    /// working credentials nobody remembers issuing. Garage will hand back the
    /// secret of a key it already has, which is what makes reuse possible.
    async fn ensure_key(
        &self,
        name: &str,
        log: &Logger,
    ) -> Result<CreatedKey, GarageError> {
        if let Some(existing) = self.find_key(name).await? {
            info!(log, "reusing the key this store was set up with";
                "key" => &existing.access_key_id,
            );
            return Ok(existing);
        }

        let created = self
            .post("CreateKey", &serde_json::json!({ "name": name }))
            .await?;
        serde_json::from_value(created)
            .map_err(|_| GarageError::Unexpected("CreateKey".into()))
    }

    /// The key named `name`, secret and all, if this store has one.
    async fn find_key(
        &self,
        name: &str,
    ) -> Result<Option<CreatedKey>, GarageError> {
        let keys = self.get("ListKeys").await?;
        let Some(existing) = keys.as_array().and_then(|keys| {
            keys.iter().find(|key| {
                key.get("name").and_then(serde_json::Value::as_str)
                    == Some(name)
            })
        }) else {
            return Ok(None);
        };

        let id = existing
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GarageError::Unexpected("ListKeys".into()))?;

        let full = self
            .get(&format!("GetKeyInfo?id={id}&showSecretKey=true"))
            .await?;
        serde_json::from_value(full)
            .map(Some)
            .map_err(|_| GarageError::Unexpected("GetKeyInfo".into()))
    }

    /// The bucket with this alias, creating it only if it is not there.
    ///
    /// Never destructive: an existing bucket is adopted with everything in it,
    /// because the objects in it are the entire reason any of this exists.
    async fn ensure_bucket(
        &self,
        alias: &str,
        log: &Logger,
    ) -> Result<String, GarageError> {
        match self
            .post("CreateBucket", &serde_json::json!({ "globalAlias": alias }))
            .await
        {
            Ok(created) => {
                let bucket: CreatedBucket = serde_json::from_value(created)
                    .map_err(|_| {
                        GarageError::Unexpected("CreateBucket".into())
                    })?;
                Ok(bucket.id)
            }
            // Already there, from an earlier life of this instance.
            Err(GarageError::AdminRefused { status: 409, .. }) => {
                let existing = self
                    .get(&format!("GetBucketInfo?globalAlias={alias}"))
                    .await?;
                let bucket: CreatedBucket = serde_json::from_value(existing)
                    .map_err(|_| {
                        GarageError::Unexpected("GetBucketInfo".into())
                    })?;
                info!(log, "adopting a bucket that already exists";
                    "bucket" => alias,
                );
                Ok(bucket.id)
            }
            Err(e) => Err(e),
        }
    }

    async fn allow(
        &self,
        bucket_id: &str,
        access_key_id: &str,
    ) -> Result<(), GarageError> {
        self.post(
            "AllowBucketKey",
            &serde_json::json!({
                "bucketId": bucket_id,
                "accessKeyId": access_key_id,
                "permissions": Permissions {
                    read: true,
                    write: true,
                    // Enough to put and get objects. Nothing on this path
                    // needs to reconfigure the bucket it writes to.
                    owner: false,
                },
            }),
        )
        .await?;
        Ok(())
    }
}

/// A secret nobody has to remember, in the shape garage wants.
fn secret() -> String {
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Make a file readable only by its owner.
#[cfg(unix)]
fn restrict(path: &Utf8Path) -> Result<(), GarageError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| GarageError::Write(path.to_owned(), e))
}

#[cfg(not(unix))]
fn restrict(_path: &Utf8Path) -> Result<(), GarageError> {
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn settings(dir: Utf8PathBuf) -> Settings {
        Settings {
            dir,
            s3_port: 3900,
            admin_port: 3903,
            rpc_port: 3901,
            capacity: "1G".to_owned(),
        }
    }

    #[test]
    fn a_restart_keeps_the_config_it_already_had() {
        // The admin token in the config is the one garage's on-disk state was
        // created under. Generating a fresh one on every boot would lock this
        // instance out of its own store, and everything in it.
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        std::fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("garage.toml");
        let settings = settings(root);

        let first = ensure_config(&path, &settings).expect("first boot");
        let written = std::fs::read_to_string(&path).expect("read");

        let second = ensure_config(&path, &settings).expect("second boot");

        assert_eq!(
            first.admin_token, second.admin_token,
            "a restart must not mint a new admin token",
        );
        assert_eq!(
            written,
            std::fs::read_to_string(&path).expect("read"),
            "a restart must not rewrite the config at all",
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_config_holds_secrets_and_is_kept_to_itself() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        std::fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("garage.toml");

        ensure_config(&path, &settings(root)).expect("first boot");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn every_kind_that_builds_gets_a_bucket() {
        // Helios has one before it has anything to put in it, so the day the
        // driver build produces something there is nowhere for it to be
        // missing.
        assert!(KINDS.contains(&"vivado"));
        assert!(KINDS.contains(&"helios"));
    }
}
