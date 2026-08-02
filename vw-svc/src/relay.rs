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

    /// Hand the instance the credentials a build fetches dependencies with.
    ///
    /// The caller's own token, passed straight through from the request that
    /// carried it. This service keeps no copy and has no credentials of its
    /// own to lend, which is the point: an instance can reach exactly what the
    /// developer using it can reach.
    ///
    /// Does nothing when there is no token to pass on, which is the case under
    /// `--no-auth`. A development service has no credentials to relay and
    /// failing every sync over their absence would make that mode useless.
    pub(crate) async fn give_credentials(
        &self,
        caller: &crate::auth::AuthorizedCaller,
        log: &slog::Logger,
    ) -> Result<(), RelayError> {
        let Some(token) = caller.token.as_deref() else {
            slog::debug!(log, "no credentials to relay";
                "environment" => &self.environment,
                "kind" => %self.kind,
            );
            return Ok(());
        };

        self.client
            .put_credentials(
                &self.environment,
                &vw_api_types_versions::latest::Credentials {
                    user: caller.name.clone(),
                    token: token.to_owned(),
                },
            )
            .await
            .map_err(|e| self.failed(e))?;

        Ok(())
    }

    /// Open a vivado session on the instance and join it to `client`.
    ///
    /// Frames are passed through untouched in both directions. This service
    /// has already decided the only thing it is in a position to decide —
    /// whether this caller owns this environment — and the conversation that
    /// follows is between the developer's machine and the worker. Reading it
    /// would buy nothing and add a place for it to be misunderstood.
    pub(crate) async fn join_vivado_session(
        &self,
        client: dropshot::WebsocketConnection,
        query: &vw_api_types_versions::latest::VivadoSessionQuery,
    ) -> Result<(), RelayError> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::protocol::Role;
        use tokio_tungstenite::WebSocketStream;

        let upgraded = self
            .client
            .vivado_session(
                &self.environment,
                Some(query.info_with_stack),
                query.part.as_deref(),
                query.variant.as_deref(),
                Some(query.verbose),
            )
            .await
            .map_err(|e| self.failed(e))?
            .into_inner();

        let instance =
            WebSocketStream::from_raw_socket(upgraded, Role::Client, None)
                .await;
        let developer = WebSocketStream::from_raw_socket(
            client.into_inner(),
            Role::Server,
            None,
        )
        .await;

        let (mut to_instance, mut from_instance) = instance.split();
        let (mut to_developer, mut from_developer) = developer.split();

        // Either direction ending ends the session. A developer who has
        // interrupted a build wants vivado torn down, not left running; an
        // instance whose worker has died has nothing more to say.
        let outbound = async {
            while let Some(Ok(frame)) = from_developer.next().await {
                if to_instance.send(frame).await.is_err() {
                    break;
                }
            }
            let _ = to_instance.close().await;
        };
        let inbound = async {
            while let Some(Ok(frame)) = from_instance.next().await {
                if to_developer.send(frame).await.is_err() {
                    break;
                }
            }
            let _ = to_developer.close().await;
        };

        tokio::pin!(outbound);
        tokio::pin!(inbound);
        futures::future::select(outbound, inbound).await;

        Ok(())
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
