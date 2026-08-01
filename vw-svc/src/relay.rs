//! Passing source through to the instance it belongs on.
//!
//! Nothing is kept here. A developer's machine holds the tree, the instance
//! holds a copy, and this service only decides who is allowed to talk to whom
//! and forwards the bytes. That is deliberate: a copy stored here would be a
//! third place for the tree to be subtly wrong, and there is nothing it could
//! recover that the developer's own working tree cannot.
//!
//! The instance is reached on its VPC address rather than its external one.
//! Both would work, but the internal path is the rack's own fabric — orders of
//! magnitude more bandwidth, and it never leaves the building.

use vw_api_types_versions::latest::{TargetKind, UserEnvironmentPathParam};

use crate::{db, reconciler::InstanceKind};

/// The port an agent listens on.
///
/// Fixed rather than discovered: the agents are started by this service's own
/// provisioning, so there is nothing to negotiate.
pub(crate) const AGENT_PORT: u16 = 2729;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelayError {
    #[error("environment does not exist")]
    NoSuchEnvironment,
    #[error("the {kind} instance for this environment does not exist yet")]
    NoInstance { kind: TargetKind },
    #[error(
        "the {kind} instance has no address on the rack's network yet; it is \
         probably still coming up"
    )]
    NoAddress { kind: TargetKind },
    #[error("reading the environment")]
    Db(#[from] db::GetError),
    #[error("building a client for the instance")]
    Client(#[source] vw_api_client::Error),
    #[error("talking to the {kind} instance")]
    Agent {
        kind: TargetKind,
        #[source]
        source: Box<
            vw_api_client::agent::Error<vw_api_client::agent::types::Error>,
        >,
    },
}

/// A connection to the agent serving one half of one environment.
pub(crate) struct Agent {
    pub(crate) client: vw_api_client::agent::Client,
    pub(crate) environment: String,
    pub(crate) kind: TargetKind,
}

impl Agent {
    /// Find the instance serving `kind` for `user`'s environment `name`.
    ///
    /// Everything this needs is already recorded by the reconciler, so no call
    /// to the rack is made to work out where to send things.
    pub(crate) fn resolve(
        user: &str,
        name: &str,
        kind: TargetKind,
        args: &crate::ServerArgs,
    ) -> Result<Agent, RelayError> {
        // A development override stands in for the instance lookup, so the
        // whole path can be exercised on one machine with no rack behind it.
        let override_address = match kind {
            TargetKind::Vivado => args.vivado_agent.as_deref(),
            TargetKind::Helios => args.helios_agent.as_deref(),
        };
        if let Some(address) = override_address {
            return Agent::at(&format!("http://{address}"), name, kind);
        }

        let environment =
            db::get_environment_status(UserEnvironmentPathParam {
                user: user.to_owned(),
                name: name.to_owned(),
            })
            .map_err(|e| match e {
                db::GetError::NoSuchEnvironment => {
                    RelayError::NoSuchEnvironment
                }
                other => RelayError::Db(other),
            })?;

        let instance = match InstanceKind::from(kind) {
            InstanceKind::Vivado => environment.vivado_instance,
            InstanceKind::Helios => environment.helios_instance,
            InstanceKind::Artifact => None,
        }
        .ok_or(RelayError::NoInstance { kind })?;

        // An instance that has been asked for but not yet built has a record
        // with no address on it. That is worth saying plainly — it is the
        // ordinary case in the first minute of an environment's life, not a
        // failure.
        let address =
            instance.internal_ip.ok_or(RelayError::NoAddress { kind })?;

        let base_url = match address {
            std::net::IpAddr::V4(v4) => format!("http://{v4}:{AGENT_PORT}"),
            std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{AGENT_PORT}"),
        };

        Agent::at(&base_url, name, kind)
    }

    fn at(
        base_url: &str,
        environment: &str,
        kind: TargetKind,
    ) -> Result<Agent, RelayError> {
        Ok(Agent {
            client: vw_api_client::agent_client(base_url)
                .map_err(RelayError::Client)?,
            environment: environment.to_owned(),
            kind,
        })
    }

    /// Wrap an error from the agent so it says which instance failed.
    pub(crate) fn failed(
        &self,
        source: vw_api_client::agent::Error<vw_api_client::agent::types::Error>,
    ) -> RelayError {
        RelayError::Agent {
            kind: self.kind,
            source: Box::new(source),
        }
    }
}

impl From<TargetKind> for InstanceKind {
    fn from(value: TargetKind) -> Self {
        match value {
            TargetKind::Vivado => InstanceKind::Vivado,
            TargetKind::Helios => InstanceKind::Helios,
        }
    }
}
