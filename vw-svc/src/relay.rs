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

/// Where an agent lives on the rack's network.
fn agent_url(address: std::net::IpAddr) -> String {
    match address {
        std::net::IpAddr::V4(v4) => format!("http://{v4}:{AGENT_PORT}"),
        std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{AGENT_PORT}"),
    }
}

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

        Agent::at(&agent_url(address), name, kind)
    }

    /// Find the instance that runs this environment's object store.
    ///
    /// Separate from [`Agent::resolve`] because the artifact instance is not
    /// something source is ever synchronized to — it has no `TargetKind` — but
    /// it is the one machine that knows where finished artifacts go.
    pub(crate) fn resolve_artifact(
        user: &str,
        name: &str,
        args: &crate::ServerArgs,
    ) -> Result<Agent, RelayError> {
        if let Some(address) = args.artifact_agent.as_deref() {
            return Agent::at(
                &format!("http://{address}"),
                name,
                TargetKind::Vivado,
            );
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

        let instance =
            environment
                .artifact_instance
                .ok_or(RelayError::NoInstance {
                    kind: TargetKind::Vivado,
                })?;
        let address = instance.internal_ip.ok_or(RelayError::NoAddress {
            kind: TargetKind::Vivado,
        })?;

        Agent::at(
            &agent_url(address),
            name,
            // Only used for error text; there is no artifact target kind and
            // inventing one would put it in the public API for no reason.
            TargetKind::Vivado,
        )
    }

    /// Where this environment's artifacts go, and the key that opens it.
    ///
    /// The endpoint is filled in here rather than by the instance that minted
    /// the key: that instance cannot know which of its addresses another one
    /// can reach it on, and this service has both in the same record.
    pub(crate) async fn object_store(
        &self,
        address: std::net::IpAddr,
        kind: TargetKind,
    ) -> Result<vw_api_types_versions::latest::S3Credentials, RelayError> {
        let mut credentials = self
            .client
            .get_object_store(&self.environment, Some(&kind))
            .await
            .map_err(|e| self.failed(e))?
            .into_inner();

        let port = credentials.port;
        credentials.endpoint = match address {
            std::net::IpAddr::V4(v4) => format!("http://{v4}:{port}"),
            std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        };

        Ok(credentials)
    }

    /// Where this instance currently believes its artifacts go.
    ///
    /// An instance that has never been told answers that it has not, which is
    /// the answer that matters at startup.
    pub(crate) async fn artifact_target(
        &self,
    ) -> Result<vw_api_types_versions::latest::S3Credentials, RelayError> {
        Ok(self
            .client
            .get_artifact_target(&self.environment)
            .await
            .map_err(|e| self.failed(e))?
            .into_inner())
    }

    /// Tell this instance where the artifacts it builds should go.
    pub(crate) async fn set_artifact_target(
        &self,
        credentials: &vw_api_types_versions::latest::S3Credentials,
    ) -> Result<(), RelayError> {
        self.client
            .put_artifact_target(&self.environment, credentials)
            .await
            .map_err(|e| self.failed(e))?;
        Ok(())
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

        join(client, upgraded).await;

        Ok(())
    }

    /// Run this environment's testbenches on the instance, joined to `client`.
    ///
    /// Relayed the same way a vivado session is, and for the same reason: what
    /// crosses is a conversation between the developer's machine and the
    /// instance, and this service's only business with it was deciding whether
    /// to allow it at all.
    pub(crate) async fn join_bench_session(
        &self,
        client: dropshot::WebsocketConnection,
        query: &vw_api_types_versions::latest::BenchQuery,
    ) -> Result<(), RelayError> {
        let upgraded = self
            .client
            .bench_session(
                &self.environment,
                query.concurrency,
                query.filter.as_deref(),
                query.ignore.as_deref(),
                query.standard.as_deref(),
            )
            .await
            .map_err(|e| self.failed(e))?
            .into_inner();

        join(client, upgraded).await;

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

/// Pass frames between a developer and an instance until one of them stops.
///
/// Untouched in both directions. Reading them would buy nothing — the two ends
/// share a protocol this service has no part in — and would add a place for it
/// to be misunderstood.
///
/// Either side ending ends the session. A developer who interrupted a build
/// wants it torn down rather than left running; an instance whose worker has
/// died has nothing more to say.
async fn join(
    client: dropshot::WebsocketConnection,
    instance: reqwest::Upgraded,
) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::WebSocketStream;

    let instance =
        WebSocketStream::from_raw_socket(instance, Role::Client, None).await;
    let developer = WebSocketStream::from_raw_socket(
        client.into_inner(),
        Role::Server,
        None,
    )
    .await;

    let (mut to_instance, mut from_instance) = instance.split();
    let (mut to_developer, mut from_developer) = developer.split();

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
}

impl From<TargetKind> for InstanceKind {
    fn from(value: TargetKind) -> Self {
        match value {
            TargetKind::Vivado => InstanceKind::Vivado,
            TargetKind::Helios => InstanceKind::Helios,
        }
    }
}
