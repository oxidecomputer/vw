//! This module implements the vw service database
//!
//! The only state the vw service itself keeps track of is that of instances
//! within an environment.
//!
//! Environments live in a single redb table keyed by `"{user}/{name}"` with
//! the JSON encoding of an [`Environment`] as the value. Keying this way keeps
//! all of a user's environments contiguous in the table so listing them is a
//! prefix scan.

use std::path::Path;
use std::sync::OnceLock;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use vw_api_types_versions::latest::{
    Environment, EnvironmentImages, SshKeyPair, UserEnvironment,
    UserEnvironmentPathParam,
};

use crate::reconciler::{InstanceKind, InstanceMap, UserInstance};

/// Environments keyed by `"{user}/{name}"`, valued by a JSON encoded
/// [`Environment`].
const ENVIRONMENTS: TableDefinition<&str, &str> =
    TableDefinition::new("environments");

/// Ssh keypairs keyed by `"{user}/{name}"`, valued by a JSON encoded
/// [`SshKeyPair`].
///
/// A separate table from the environments so a private key can never be
/// carried out by an endpoint that returns an [`Environment`].
const SSH_KEYS: TableDefinition<&str, &str> = TableDefinition::new("ssh_keys");

/// The process-wide database handle, established by [`init`] before either API
/// server starts.
static DB: OnceLock<Database> = OnceLock::new();

/// Error conditions for opening the database.
#[derive(thiserror::Error, Debug)]
pub(crate) enum InitError {
    #[error("opening database: {0}")]
    Open(#[from] redb::DatabaseError),
    #[error("beginning transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("committing transaction: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("the database has already been initialized")]
    AlreadyInitialized,
}

/// Error conditions for listing user enviornments.
#[derive(thiserror::Error, Debug)]
pub(crate) enum ListError {
    #[error("beginning read transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("reading environments table: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("decoding environment '{key}': {source}")]
    Decode {
        key: String,
        source: serde_json::Error,
    },
}

/// Error conditions for creating an environment db entry.
#[derive(thiserror::Error, Debug)]
pub(crate) enum CreateError {
    #[error("environment already exists")]
    EnvironmentAlreadyExists,
    #[error("encoding environment: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("beginning write transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("writing environments table: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("committing transaction: {0}")]
    Commit(#[from] redb::CommitError),
}

/// Error conditions for deleting an environment db entry.
#[derive(thiserror::Error, Debug)]
pub(crate) enum DeleteError {
    #[error("environment does not exist")]
    NoSuchEnvironment,
    #[error("beginning write transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("writing environments table: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("committing transaction: {0}")]
    Commit(#[from] redb::CommitError),
}

/// Error conditinos for retreiving environment status.
#[derive(thiserror::Error, Debug)]
pub(crate) enum GetError {
    #[error("environment does not exist")]
    NoSuchEnvironment,
    #[error("beginning read transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("reading environments table: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("decoding environment: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Error conditinos for retreiving environment status.
#[derive(thiserror::Error, Debug)]
pub(crate) enum UpdateError {
    #[error("environment does not exist")]
    NoSuchEnvironment,
    #[error("encoding environment: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("beginning write transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("opening environments table: {0}")]
    Table(#[from] redb::TableError),
    #[error("writing environments table: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("committing transaction: {0}")]
    Commit(#[from] redb::CommitError),
}

/// Open the database at `path`, creating it if it does not exist.
///
/// This must be called once, before any of the accessors below are used. The
/// environments table is created here so that readers never have to contend
/// with a missing table on a fresh database.
pub(crate) fn init(path: impl AsRef<Path>) -> Result<(), InitError> {
    let db = Database::create(path)?;
    let tx = db.begin_write()?;
    tx.open_table(ENVIRONMENTS)?;
    tx.open_table(SSH_KEYS)?;
    tx.commit()?;
    DB.set(db).map_err(|_| InitError::AlreadyInitialized)
}

/// The database handle established by [`init`].
///
/// Panics if [`init`] has not run. That is a service startup bug, not
/// something a request can provoke.
fn db() -> &'static Database {
    DB.get().expect("database has not been initialized")
}

/// The database key for an environment named `name` owned by `user`.
fn key(user: &str, name: &str) -> String {
    format!("{user}/{name}")
}

/// Every environment in the db, with the owning user.
pub(crate) fn list_all_environments() -> Result<Vec<UserEnvironment>, ListError>
{
    let tx = db().begin_read()?;
    let table = tx.open_table(ENVIRONMENTS)?;

    let mut environments = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let key = key.value();
        // Keys are "{user}/{name}"; anything else did not come from here.
        let Some((user, _)) = key.split_once('/') else {
            continue;
        };
        environments.push(UserEnvironment {
            user: user.to_owned(),
            environment: serde_json::from_str(value.value()).map_err(
                |source| ListError::Decode {
                    key: key.to_owned(),
                    source,
                },
            )?,
        });
    }

    Ok(environments)
}

/// Every environment decomposed into the individual instances that make it up.
///
/// This is the reconciler's target state: an environment always wants all
/// three of its instances, whether or not any of them exist yet.
pub(crate) fn list_all_environment_instances() -> Result<InstanceMap, ListError>
{
    let mut instances = InstanceMap::new();

    let tx = db().begin_read()?;
    let keys = tx.open_table(SSH_KEYS)?;

    for UserEnvironment { user, environment } in list_all_environments()? {
        // The public half is what gets attached to the instances. A missing
        // entry is not fatal here; the instance simply comes up without a key
        // and the reconciler says so.
        let db_key = key(&user, &environment.name);
        let public_key = match keys.get(db_key.as_str())? {
            Some(value) => serde_json::from_str::<SshKeyPair>(value.value())
                .map(|pair| pair.public_key)
                .map_err(|source| ListError::Decode {
                    key: db_key.clone(),
                    source,
                })
                .map(Some)?,
            None => None,
        };

        for kind in InstanceKind::ALL {
            let (recorded, image) = match kind {
                InstanceKind::Vivado => (
                    &environment.vivado_instance,
                    environment.images.as_ref().map(|i| &i.vivado),
                ),
                InstanceKind::Helios => (
                    &environment.helios_instance,
                    environment.images.as_ref().map(|i| &i.helios),
                ),
                InstanceKind::Artifact => (
                    &environment.artifact_instance,
                    environment.images.as_ref().map(|i| &i.artifact),
                ),
            };

            // Instance names are unique per user/environment/kind, so this
            // cannot collide with anything already inserted.
            instances.insert_overwrite(UserInstance {
                user: user.clone(),
                environment: environment.name.clone(),
                kind,
                image: image.cloned(),
                public_key: public_key.clone(),
                oxide_instance: recorded.clone(),
            });
        }
    }

    Ok(instances)
}

pub(crate) fn list_user_environments(
    user: impl AsRef<str>,
) -> Result<Vec<Environment>, ListError> {
    // Every one of this user's environments is keyed by this prefix, and redb
    // iterates in key order, so the user's entries are the contiguous run
    // starting at the first key greater than or equal to the prefix.
    let prefix = format!("{}/", user.as_ref());

    let tx = db().begin_read()?;
    let table = tx.open_table(ENVIRONMENTS)?;

    let mut environments = Vec::new();
    for entry in table.range(prefix.as_str()..)? {
        let (key, value) = entry?;
        let key = key.value();
        if !key.starts_with(&prefix) {
            break;
        }
        environments.push(serde_json::from_str(value.value()).map_err(
            |source| ListError::Decode {
                key: key.to_owned(),
                source,
            },
        )?);
    }

    Ok(environments)
}

pub(crate) fn create_environment(
    env: UserEnvironmentPathParam,
    images: Option<EnvironmentImages>,
    ssh_key: &SshKeyPair,
) -> Result<(), CreateError> {
    let key = key(&env.user, &env.name);
    let value = serde_json::to_string(&Environment {
        name: env.name,
        images,
        vivado_instance: None,
        helios_instance: None,
        artifact_instance: None,
    })?;
    let ssh_key = serde_json::to_string(ssh_key)?;

    // Both in one transaction: an environment without its keypair would be
    // unreachable, and a keypair without its environment would never be
    // cleaned up.
    let tx = db().begin_write()?;
    {
        let mut environments = tx.open_table(ENVIRONMENTS)?;
        if environments.get(key.as_str())?.is_some() {
            // Dropping the transaction without committing aborts it.
            return Err(CreateError::EnvironmentAlreadyExists);
        }
        environments.insert(key.as_str(), value.as_str())?;

        let mut keys = tx.open_table(SSH_KEYS)?;
        keys.insert(key.as_str(), ssh_key.as_str())?;
    }
    tx.commit()?;

    Ok(())
}

/// The keypair that opens an environment's instances.
pub(crate) fn get_environment_keys(
    env: UserEnvironmentPathParam,
) -> Result<SshKeyPair, GetError> {
    let key = key(&env.user, &env.name);

    let tx = db().begin_read()?;
    let table = tx.open_table(SSH_KEYS)?;
    let value = table
        .get(key.as_str())?
        .ok_or(GetError::NoSuchEnvironment)?;

    Ok(serde_json::from_str(value.value())?)
}

pub(crate) fn delete_environment(
    env: UserEnvironmentPathParam,
) -> Result<(), DeleteError> {
    let key = key(&env.user, &env.name);

    let tx = db().begin_write()?;
    {
        let mut environments = tx.open_table(ENVIRONMENTS)?;
        if environments.remove(key.as_str())?.is_none() {
            return Err(DeleteError::NoSuchEnvironment);
        }
        // The keypair opens nothing once the environment is gone.
        tx.open_table(SSH_KEYS)?.remove(key.as_str())?;
    }
    tx.commit()?;

    Ok(())
}

pub(crate) fn get_environment_status(
    env: UserEnvironmentPathParam,
) -> Result<Environment, GetError> {
    let key = key(&env.user, &env.name);

    let tx = db().begin_read()?;
    let table = tx.open_table(ENVIRONMENTS)?;
    let value = table
        .get(key.as_str())?
        .ok_or(GetError::NoSuchEnvironment)?;

    Ok(serde_json::from_str(value.value())?)
}

pub(crate) fn update_environment_status(
    key: UserEnvironmentPathParam,
    env: Environment,
) -> Result<(), UpdateError> {
    let db_key = self::key(&key.user, &key.name);
    let value = serde_json::to_string(&env)?;

    let tx = db().begin_write()?;
    {
        let mut table = tx.open_table(ENVIRONMENTS)?;
        // Only an update: creating an environment goes through
        // `create_environment` so that its images get resolved.
        if table.get(db_key.as_str())?.is_none() {
            return Err(UpdateError::NoSuchEnvironment);
        }
        table.insert(db_key.as_str(), value.as_str())?;
    }
    tx.commit()?;

    Ok(())
}
