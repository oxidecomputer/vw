//! Telling each instance where the artifacts it builds should go.
//!
//! Only this service can do it. The instance that holds the object store mints
//! the key but cannot know which address its neighbours reach it on; the
//! instances that fill the store cannot know which of their neighbours holds
//! it. This service sees both, and its whole job here is to introduce them.
//!
//! Done in two places, for two different reasons. On startup, so an
//! environment created while its instances were still coming up — or one whose
//! store has been rebuilt since — is put right without anyone having to do
//! anything. And on each first sync, so an environment created after startup
//! does not have to wait for the next one.
//!
//! A bucket per workspace, so the startup pass has to know which workspaces an
//! environment has. It asks the instance rather than looking it up: a workspace
//! is on an instance because somebody synchronized one there, and this service
//! keeps no record that could be right about it.

use slog::{error, info, Logger};
use vw_api_types_versions::latest::TargetKind;

use crate::{db, relay, ServerArgs};

/// The key and bucket for one of an environment's artifact stores.
///
/// Used both to configure an instance and to read the store back out on a
/// developer's behalf, which is the same question asked by two callers.
pub(crate) async fn store_for(
    user: &str,
    name: &str,
    workspace: &str,
    kind: TargetKind,
    args: &ServerArgs,
) -> Result<
    vw_api_types_versions::latest::S3Credentials,
    crate::artifacts::ArtifactError,
> {
    let artifact = relay::Agent::resolve_artifact(user, name, workspace, args)
        .map_err(|_| crate::artifacts::ArtifactError::NoStore)?;
    let address = artifact_address(user, name, args)
        .ok_or(crate::artifacts::ArtifactError::NoStore)?;

    artifact
        .object_store(address, kind)
        .await
        .map_err(|_| crate::artifacts::ArtifactError::NoStore)
}

/// The kinds of instance that build something worth keeping.
///
/// Helios is wired up before it has anything to put anywhere. A bucket costs
/// nothing standing empty, and asking for it on the first sync means the day
/// the driver build starts producing something there is nowhere for it to be
/// missing — which the store used to guarantee by making every bucket at boot,
/// and cannot now that they follow the workspaces.
const KINDS: [TargetKind; 2] = [TargetKind::Vivado, TargetKind::Helios];

/// Make sure one instance knows where its artifacts go.
///
/// Cheap when there is nothing to do: the instance is asked what it currently
/// believes, and only told again if that differs from the truth. Which means a
/// store rebuilt with a new key is noticed and corrected, rather than left
/// pointing somewhere that no longer accepts it.
pub(crate) async fn ensure(
    user: &str,
    name: &str,
    workspace: &str,
    kind: TargetKind,
    args: &ServerArgs,
    instance: &relay::Agent,
    log: &Logger,
) {
    let artifact =
        match relay::Agent::resolve_artifact(user, name, workspace, args) {
            Ok(artifact) => artifact,
            Err(e) => {
                info!(log, "no artifact instance to store artifacts on yet";
                    "environment" => name,
                    "detail" => %e,
                );
                return;
            }
        };

    let Some(address) = artifact_address(user, name, args) else {
        return;
    };

    let wanted = match artifact.object_store(address, kind).await {
        Ok(wanted) => wanted,
        Err(e) => {
            info!(log, "cannot read the environment's object store yet";
                "environment" => name,
                "detail" => %e,
            );
            return;
        }
    };

    // Already right, so say nothing and do nothing. This is the ordinary case
    // on every restart after the first.
    if let Ok(current) = instance.artifact_target().await {
        if current.endpoint == wanted.endpoint
            && current.bucket == wanted.bucket
            && current.access_key_id == wanted.access_key_id
        {
            return;
        }
    }

    if let Err(e) = instance.set_artifact_target(&wanted).await {
        error!(log, "cannot tell an instance where artifacts go";
            "environment" => name,
            "kind" => %kind,
            slog_error_chain::InlineErrorChain::new(&e),
        );
        return;
    }

    info!(log, "artifacts wired up";
        "environment" => name,
        "kind" => %kind,
        "bucket" => &wanted.bucket,
    );
}

/// Put every environment right, once.
///
/// Run at startup. An environment whose instances were still coming up when it
/// was created has nowhere to put artifacts until someone tells it, and until
/// now the only thing that ever told it was a source synchronization — so an
/// environment nobody had synced since the service last started would build
/// images that went nowhere.
///
/// Failures are per environment and only logged. One environment whose
/// instances are down should not stop the rest being configured, and the sync
/// path will catch it later regardless.
pub(crate) async fn ensure_all(args: &ServerArgs, log: &Logger) {
    let environments = match db::list_all_environments() {
        Ok(environments) => environments,
        Err(e) => {
            error!(log, "cannot list environments to configure artifacts for";
                slog_error_chain::InlineErrorChain::new(&e),
            );
            return;
        }
    };

    if environments.is_empty() {
        return;
    }

    info!(log, "checking that every environment can store artifacts";
        "environments" => environments.len(),
    );

    for entry in environments {
        let (user, name) = (&entry.user, &entry.environment.name);

        // Which workspaces an environment has is not something this service
        // records, so the vivado instance is asked. Not measured: this wants
        // names, and measuring means walking every build tree on the machine.
        let workspaces = match workspaces_on(user, name, args, log).await {
            Some(workspaces) => workspaces,
            None => continue,
        };

        if workspaces.is_empty() {
            continue;
        }

        for workspace in &workspaces {
            for kind in KINDS {
                let instance = match relay::Agent::resolve(
                    user, name, workspace, kind, args,
                ) {
                    Ok(instance) => instance,
                    Err(e) => {
                        // Ordinary while an environment is still being built.
                        info!(log, "instance not ready to be configured";
                            "environment" => name,
                            "kind" => %kind,
                            "detail" => %e,
                        );
                        continue;
                    }
                };

                ensure(user, name, workspace, kind, args, &instance, log).await;
            }
        }
    }
}

/// The workspaces an environment is holding, as its vivado instance sees them.
///
/// `None` when the question could not be put — an instance still coming up, an
/// agent that did not answer. That is not a failure worth reporting at
/// startup: the environment gets wired on its next sync regardless, which is
/// the path that catches everything this one is only trying to get ahead of.
async fn workspaces_on(
    user: &str,
    name: &str,
    args: &ServerArgs,
    log: &Logger,
) -> Option<Vec<String>> {
    let instance =
        relay::Agent::resolve_instance(user, name, TargetKind::Vivado, args)
            .inspect_err(|e| {
                info!(log, "instance not ready to be asked what it holds";
                    "environment" => name,
                    "detail" => %e,
                );
            })
            .ok()?;

    let held = instance
        .workspaces(false)
        .await
        .inspect_err(|e| {
            info!(log, "cannot ask an instance what workspaces it holds";
                "environment" => name,
                "detail" => %e,
            );
        })
        .ok()?;

    Some(held.into_iter().map(|workspace| workspace.name).collect())
}

/// The artifact instance's address on the rack's network.
pub(crate) fn artifact_address(
    user: &str,
    name: &str,
    args: &ServerArgs,
) -> Option<std::net::IpAddr> {
    // A development override is an address the other instances can reach too,
    // since with no rack behind this everything is on one machine.
    if let Some(address) = args.artifact_agent.as_deref() {
        return address.split(':').next().and_then(|host| host.parse().ok());
    }

    db::get_environment_status(
        vw_api_types_versions::latest::UserEnvironmentPathParam {
            user: user.to_owned(),
            name: name.to_owned(),
        },
    )
    .ok()
    .and_then(|environment| environment.artifact_instance)
    .and_then(|instance| instance.internal_ip)
}
