//! This module implements the vw instance reconciler. The reconciler looks
//! at the environments in the db and ensures and reconciles their target
//! state with what's actually running on an Oxide Cloud Computer.
//!
//! A pass is a diff: the db says which instances should exist, the Oxide API
//! says which do, and the difference becomes a list of things to create,
//! destroy, or start. Nothing here waits for an instance to finish changing
//! state — a pass acts on what it can and leaves the rest for the next one,
//! so a slow boot or a slow shutdown never blocks reconciliation of anything
//! else.

use std::{fmt::Display, str::FromStr, time::Duration};

use crate::{
    db::{self, list_all_environment_instances, ListError},
    oxide as ox,
};
use daft::Diffable;
use futures::{stream::FuturesUnordered, StreamExt};
use iddqd::{id_upcast, IdOrdItem, IdOrdMap};
use slog::{error, info, warn, Logger};
use slog_error_chain::InlineErrorChain;
use tokio::sync::Notify;
use vw_api_types_versions::latest::{
    ImageRef, OxideInstance, UserEnvironmentPathParam,
};

pub(crate) struct InstanceReconciler {
    /// How long to wait between passes when nothing asks for one sooner.
    interval: Duration,
    /// Rung by the API when an environment is created or deleted, so a pass
    /// runs immediately instead of waiting out the interval.
    wake: std::sync::Arc<Notify>,
}

impl InstanceReconciler {
    pub(crate) fn new(
        interval: Duration,
        wake: std::sync::Arc<Notify>,
    ) -> Self {
        Self { interval, wake }
    }

    /// Reconcile forever, one pass at a time.
    ///
    /// A failed pass is logged and retried on the next tick rather than
    /// stopping the loop: the causes are transient often enough (an API
    /// blip, an instance mid-transition) that giving up would be worse than
    /// trying again.
    pub(crate) async fn run(&self, log: Logger) {
        loop {
            self.run_once(&log).await;
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn run_once(&self, log: &Logger) {
        if let Err(e) = self.pass(log).await {
            error!(log, "reconciliation error"; InlineErrorChain::new(&e));
        }
    }

    async fn pass(&self, log: &Logger) -> Result<(), PassError> {
        // One client for this pass, discarded when it ends.
        let session = ox::session()?;

        let target = list_all_environment_instances()?;
        let current = session.get_instances().await?;
        let plan = self.plan(&target, &current, log);

        // Before acting, not after: the writes describe state read at the top
        // of this pass, and `execute` can spend a long time waiting on the
        // control plane. Deferring them would leave the environment looking
        // untouched for the whole of it.
        plan.write_status(log);
        plan.execute(&session, log).await?;

        // Deleting an instance does not take its boot disk with it, so the
        // disks have to be reconciled too or every environment that goes away
        // leaves 600GiB behind. Done against the target rather than against
        // what was just deleted, so disks orphaned by an earlier crash or a
        // half-finished pass get swept up as well.
        session.reap_disks(&target, log).await?;
        session.reap_ssh_keys(&target, log).await?;

        Ok(())
    }

    fn plan(
        &self,
        target: &InstanceMap,
        current: &InstanceMap,
        log: &Logger,
    ) -> PassAction {
        // daft diffs `before.diff(&after)`, so the rack goes on the left and
        // the db on the right. That makes `added` the instances the db wants
        // that the rack does not have, and `removed` the ones on the rack that
        // nothing wants any more.
        let diff = current.diff(target);

        // These come off the db side, so they carry the image to build from.
        let mut to_create = InstanceMap::new();
        for instance in diff.added.iter() {
            to_create.insert_overwrite((*instance).clone());
        }

        // These come off the rack side, so they carry the live state that says
        // whether to stop an instance or delete it.
        let mut to_destroy = InstanceMap::new();
        for instance in diff.removed.iter() {
            to_destroy.insert_overwrite((*instance).clone());
        }

        // Anything in both places should be running, and the db's idea of its
        // state should match the rack's.
        let mut ensure_running = InstanceMap::new();
        let mut status_updates = Vec::new();
        for pair in diff.common.iter() {
            let (current, target) = (pair.before(), pair.after());

            // Take the db's record for the image, and the rack's for the
            // state: acting on what the db last wrote would mean starting
            // instances that are already up.
            let mut instance = (*target).clone();
            instance.oxide_instance = current.oxide_instance.clone();
            ensure_running.insert_overwrite(instance);

            if !same_state(&target.oxide_instance, &current.oxide_instance) {
                status_updates.push(StatusUpdate {
                    user: current.user.clone(),
                    environment: current.environment.clone(),
                    kind: current.kind,
                    oxide_instance: current.oxide_instance.clone(),
                });
            }
        }

        for instance in diff.added.iter() {
            match instance.oxide_instance.as_ref() {
                // The db names an instance id the rack no longer has. Clear
                // it, or the db goes on advertising something that does not
                // resolve.
                Some(recorded) if recorded.id.is_some() => {
                    status_updates.push(StatusUpdate {
                        user: instance.user.clone(),
                        environment: instance.environment.clone(),
                        kind: instance.kind,
                        oxide_instance: None,
                    });
                }
                // A create already asked for and not yet seen to land. Leave
                // the marker be; it is the only sign of life there is until
                // the rack starts reporting the instance.
                Some(_) => {}
                // About to be asked for. Record that now rather than after the
                // request comes back: the control plane takes long enough that
                // an environment would otherwise sit there looking like
                // nothing had happened.
                None => {
                    status_updates.push(StatusUpdate {
                        user: instance.user.clone(),
                        environment: instance.environment.clone(),
                        kind: instance.kind,
                        oxide_instance: Some(OxideInstance {
                            id: None,
                            state: oxide::types::InstanceState::Creating,
                            external_ip: None,
                            internal_ip: None,
                        }),
                    });
                }
            }
        }

        if !to_create.is_empty()
            || !to_destroy.is_empty()
            || !status_updates.is_empty()
        {
            info!(log, "reconciliation plan";
                "create" => to_create.len(),
                "destroy" => to_destroy.len(),
                "ensure_running" => ensure_running.len(),
                "status_updates" => status_updates.len(),
            );
        }

        PassAction {
            to_create,
            to_destroy,
            ensure_running,
            status_updates,
        }
    }
}

/// Reject an environment name that would not survive the instance naming
/// scheme.
///
/// Instance names are `vwsvc-{user}-{env}-{kind}`, and they are taken back
/// apart from the right so that a username may contain `-`. That only works if
/// the environment name does not — otherwise the split lands in the wrong
/// place and one environment can be mistaken for another. The remaining rules
/// are what the control plane accepts for a `Name`.
pub(crate) fn validate_environment_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(String::from("environment name cannot be empty"));
    }
    if name.contains('-') {
        return Err(format!(
            "'{name}' cannot contain '-'; it separates the parts of the \
             underlying instance name"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(format!(
            "'{name}' may only contain lowercase letters and digits"
        ));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(format!("'{name}' must start with a lowercase letter"));
    }
    Ok(())
}

/// Reject a username that cannot be part of an Oxide instance name.
///
/// Unlike an environment name this is not the caller's to choose — it comes
/// from Github — so the rules are as loose as the control plane allows: `-` is
/// fine, since names are parsed from the right. What is left over is a Github
/// name the control plane would refuse, which is worth saying plainly rather
/// than letting the reconciler fail on it later.
pub(crate) fn validate_user_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(String::from("username cannot be empty"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "github username '{name}' contains characters that cannot appear \
             in an instance name"
        ));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(format!(
            "github username '{name}' must start with a letter to be part of \
             an instance name"
        ));
    }
    if name.ends_with('-') {
        return Err(format!("github username '{name}' cannot end with '-'"));
    }
    Ok(())
}

/// Whether the db's record of an instance already matches the rack's.
fn same_state(
    recorded: &Option<OxideInstance>,
    actual: &Option<OxideInstance>,
) -> bool {
    match (recorded, actual) {
        (Some(recorded), Some(actual)) => {
            recorded.id == actual.id
                && recorded.state == actual.state
                // The address turns up some time after the instance does, and
                // it is the part somebody actually needs.
                && recorded.external_ip == actual.external_ip
                && recorded.internal_ip == actual.internal_ip
        }
        (None, None) => true,
        _ => false,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Diffable)]
pub(crate) enum InstanceKind {
    Vivado,
    Helios,
    Artifact,
}

impl InstanceKind {
    /// Every kind, so a whole environment can be enumerated.
    pub(crate) const ALL: [InstanceKind; 3] =
        [Self::Vivado, Self::Helios, Self::Artifact];

    /// Images for this kind are named with this prefix followed by a date, and
    /// an environment that does not name an image explicitly gets the newest
    /// one that matches.
    ///
    /// These are vw's own images, built by `redhawk-dev-image`, each of which
    /// has an agent already installed and enabled. A stock OS image will boot
    /// and answer ssh, but nothing will ever reach it through this service.
    pub(crate) fn image_prefix(&self) -> &'static str {
        match self {
            Self::Vivado => "vw-vivado-",
            Self::Helios => "vw-helios-",
            Self::Artifact => "vw-artifact-",
        }
    }

    /// How much machine this kind of instance gets.
    ///
    /// Synthesis and a kernel build both take whatever they are given, so the
    /// two build instances get the largest shape an environment is worth. The
    /// artifact instance compiles nothing: it runs an object store, and the
    /// work it does is moving bytes between a socket and a disk. Sizing it
    /// like a build machine only takes cores away from environments that
    /// would use them.
    pub(crate) fn shape(&self) -> Shape {
        match self {
            Self::Vivado | Self::Helios => Shape {
                vcpus: 16,
                memory_gib: 32,
            },
            Self::Artifact => Shape {
                vcpus: 4,
                memory_gib: 16,
            },
        }
    }
}

/// The cpu and memory an instance is created with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) vcpus: u16,
    pub(crate) memory_gib: u64,
}

impl Display for InstanceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vivado => write!(f, "vivado"),
            Self::Helios => write!(f, "helios"),
            Self::Artifact => write!(f, "artifact"),
        }
    }
}

impl FromStr for InstanceKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vivado" => Ok(Self::Vivado),
            "helios" => Ok(Self::Helios),
            "artifact" => Ok(Self::Artifact),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Diffable)]
pub(crate) struct UserInstance {
    pub(crate) user: String,
    pub(crate) environment: String,
    pub(crate) kind: InstanceKind,
    /// The image this instance should boot from.
    ///
    /// `None` for instances discovered on the rack, whose image was chosen
    /// when their environment was created, and for environments recorded
    /// without an Oxide backend to resolve images against.
    pub(crate) image: Option<ImageRef>,
    /// The public half of the environment's ssh key, attached to the instance
    /// when it is created so it comes up reachable.
    ///
    /// `None` for instances discovered on the rack, where the key is recorded
    /// in the db rather than on the instance.
    pub(crate) public_key: Option<String>,
    /// What the Oxide API says about this instance, or what the db last
    /// recorded, depending on which side of the diff it came from.
    pub(crate) oxide_instance: Option<OxideInstance>,
}

impl UserInstance {
    /// The name of the Oxide silo ssh key shared by an environment's
    /// instances.
    ///
    /// One key per environment rather than per instance, so a user has a
    /// single key to fetch and use against all three.
    pub(crate) fn ssh_key_name(&self) -> String {
        format!("{}-{}-{}", ox::INSTANCE_PREFIX, self.user, self.environment)
    }

    /// The hostname the instance sees itself as.
    ///
    /// Separate from the Oxide instance name, which has to be unique across
    /// the whole project and so carries the owner and a prefix. Inside the
    /// environment none of that is news, and a shell prompt reading
    /// `ubuntu@vwsvc-rcgoodfellow-darmok-vivado` is a lot of it.
    ///
    /// Joined with `-` rather than `.` on purpose. A dotted name is a valid
    /// hostname, but cloud-init reads the dot as an FQDN separator and sets
    /// the static hostname to the first label alone — asking for
    /// `vivado.darmok` gets a box that calls itself `vivado`, and which
    /// environment it belongs to disappears from the prompt. A hyphen carries
    /// no such meaning, so the whole name survives.
    pub(crate) fn hostname(&self) -> String {
        format!("{}-{}", self.kind, self.environment)
    }

    pub(crate) fn oxide_instance_name(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            ox::INSTANCE_PREFIX,
            self.user,
            self.environment,
            self.kind
        )
    }

    /// Whether the rack has actually told us about this instance.
    ///
    /// A record carrying no id is this service's own marker for a create it
    /// asked for and has not yet seen land — it says what was wanted, not what
    /// exists. Anything deciding whether to create must ask this rather than
    /// whether `oxide_instance` is set.
    pub(crate) fn exists_on_rack(&self) -> bool {
        self.oxide_instance
            .as_ref()
            .is_some_and(|instance| instance.id.is_some())
    }

    /// Whether this instance is up or on its way up, in which case starting it
    /// again is not something to do.
    pub(crate) fn is_running_or_starting(&self) -> bool {
        use oxide::types::InstanceState;
        matches!(
            self.oxide_instance.as_ref().map(|i| &i.state),
            Some(
                InstanceState::Running
                    | InstanceState::Starting
                    | InstanceState::Creating
                    | InstanceState::Rebooting
                    | InstanceState::Migrating
                    | InstanceState::Repairing
            )
        )
    }

    /// Whether this instance has settled in a state it can be started or
    /// deleted from.
    pub(crate) fn is_stopped(&self) -> bool {
        use oxide::types::InstanceState;
        matches!(
            self.oxide_instance.as_ref().map(|i| &i.state),
            Some(InstanceState::Stopped | InstanceState::Failed)
        )
    }
}

impl IdOrdItem for UserInstance {
    type Key<'a> = String;

    fn key(&self) -> Self::Key<'_> {
        // The Oxide instance name is unique across the rack by construction,
        // which makes it the natural identity for both sides of the diff.
        self.oxide_instance_name()
    }

    id_upcast!();
}

pub(crate) type InstanceMap = IdOrdMap<UserInstance>;

/// A db record that no longer matches the rack.
struct StatusUpdate {
    user: String,
    environment: String,
    kind: InstanceKind,
    oxide_instance: Option<OxideInstance>,
}

pub struct PassAction {
    to_create: InstanceMap,
    to_destroy: InstanceMap,
    ensure_running: InstanceMap,
    status_updates: Vec<StatusUpdate>,
}

impl PassAction {
    async fn execute(
        &self,
        session: &ox::Session,
        log: &Logger,
    ) -> Result<(), PassError> {
        // Each `async fn` has its own anonymous type, so futures from
        // different functions cannot share a `Vec` without being boxed.
        // `FuturesUnordered` of boxed futures lets them all run together and
        // report back as they finish.
        // Before anything concurrent: an environment's instances share one
        // ssh key, and racing to register it is how two of the three end up
        // failing.
        session.ensure_ssh_keys(&self.to_create, log).await?;

        let mut tasks = FuturesUnordered::new();
        for inst in self.to_create.iter() {
            tasks.push(boxed(labeled(
                "create",
                inst,
                session.create_instance(inst, log),
            )));
        }
        for inst in self.ensure_running.iter() {
            tasks.push(boxed(labeled(
                "ensure_running",
                inst,
                session.ensure_instance_running(inst, log),
            )));
        }
        for inst in self.to_destroy.iter() {
            tasks.push(boxed(labeled(
                "destroy",
                inst,
                session.delete_instance(inst, log),
            )));
        }

        // One instance failing says nothing about the others, so let every
        // task finish and report the first failure only once they have. The
        // next pass retries whatever did not take.
        let mut first_error = None;
        while let Some((operation, instance, result)) = tasks.next().await {
            let Err(e) = result else { continue };

            if ox::is_inconclusive(&e) {
                // The rack may well have done what was asked and simply never
                // said so. The next pass reads the actual state, so let it
                // settle there rather than calling this a failure and
                // abandoning the rest of the pass.
                warn!(log, "instance operation did not report back";
                    "operation" => operation,
                    "instance" => &instance,
                    InlineErrorChain::new(&e),
                );
                continue;
            }

            error!(log, "instance operation failed";
                "operation" => operation,
                "instance" => &instance,
                InlineErrorChain::new(&e),
            );
            first_error.get_or_insert(e);
        }

        match first_error {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// Bring the db's record of instance state back in line with the rack.
    ///
    /// Failures here are logged rather than propagated: the rack is the source
    /// of truth and the next pass will try again, so a write that does not
    /// land is not a reason to abandon the rest of the update.
    fn write_status(&self, log: &Logger) {
        for update in &self.status_updates {
            let key = UserEnvironmentPathParam {
                user: update.user.clone(),
                name: update.environment.clone(),
            };
            let mut environment = match db::get_environment_status(key.clone())
            {
                Ok(environment) => environment,
                Err(e) => {
                    warn!(log, "cannot read environment to update status";
                        "user" => &update.user,
                        "environment" => &update.environment,
                        InlineErrorChain::new(&e),
                    );
                    continue;
                }
            };

            let slot = match update.kind {
                InstanceKind::Vivado => &mut environment.vivado_instance,
                InstanceKind::Helios => &mut environment.helios_instance,
                InstanceKind::Artifact => &mut environment.artifact_instance,
            };
            *slot = update.oxide_instance.clone();

            if let Err(e) = db::update_environment_status(key, environment) {
                warn!(log, "cannot record instance state";
                    "user" => &update.user,
                    "environment" => &update.environment,
                    "kind" => %update.kind,
                    InlineErrorChain::new(&e),
                );
            }
        }
    }
}

/// What a task was doing and to which instance, carried alongside its result.
///
/// Without this a failed pass says only that *an* operation failed, which is
/// no help when several instances are in flight at once.
type TaskResult = (&'static str, String, Result<(), ox::OxideError>);

/// Tag a task with the operation and instance it belongs to.
async fn labeled<'a>(
    operation: &'static str,
    instance: &UserInstance,
    future: impl std::future::Future<Output = Result<(), ox::OxideError>>
        + Send
        + 'a,
) -> TaskResult {
    let name = instance.oxide_instance_name();
    (operation, name, future.await)
}

/// Box a future so differently-typed futures can share one collection.
fn boxed<'a>(
    future: impl std::future::Future<Output = TaskResult> + Send + 'a,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TaskResult> + Send + 'a>>
{
    Box::pin(future)
}

#[derive(Debug, thiserror::Error)]
enum PassError {
    // These deliberately do not interpolate their source: every log site
    // reports them through `InlineErrorChain`, which appends the whole chain
    // of causes itself. A wrapper that also embedded its source would print
    // the cause twice.
    #[error("listing environments from the db failed")]
    DbList(#[from] ListError),

    #[error("talking to the oxide api failed")]
    Oxide(#[from] ox::OxideError),

    #[error("connecting to the oxide api failed")]
    Session(#[from] ox::SessionError),
}

#[cfg(test)]
mod test {
    use super::*;
    use oxide::types::InstanceState;
    use uuid::Uuid;

    fn instance(
        user: &str,
        environment: &str,
        kind: InstanceKind,
        state: Option<InstanceState>,
    ) -> UserInstance {
        UserInstance {
            user: user.to_owned(),
            environment: environment.to_owned(),
            kind,
            image: None,
            public_key: None,
            oxide_instance: state.map(|state| OxideInstance {
                id: Some(Uuid::nil()),
                state,
                external_ip: None,
                internal_ip: None,
            }),
        }
    }

    fn map(instances: impl IntoIterator<Item = UserInstance>) -> InstanceMap {
        let mut map = InstanceMap::new();
        for instance in instances {
            map.insert_overwrite(instance);
        }
        map
    }

    fn reconciler() -> InstanceReconciler {
        InstanceReconciler::new(
            Duration::from_secs(1),
            std::sync::Arc::new(Notify::new()),
        )
    }

    fn log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn wanted_but_absent_instances_are_created() {
        let target =
            map([instance("ferris", "alpha", InstanceKind::Vivado, None)]);
        let plan = reconciler().plan(&target, &InstanceMap::new(), &log());

        assert_eq!(plan.to_create.len(), 1);
        assert!(plan.to_destroy.is_empty());
        assert!(plan.ensure_running.is_empty());
    }

    #[test]
    fn instances_nothing_wants_are_destroyed() {
        let current = map([instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Running),
        )]);
        let plan = reconciler().plan(&InstanceMap::new(), &current, &log());

        assert!(plan.to_create.is_empty());
        assert_eq!(plan.to_destroy.len(), 1);
    }

    #[test]
    fn instances_on_both_sides_are_kept_running() {
        let target =
            map([instance("ferris", "alpha", InstanceKind::Vivado, None)]);
        let current = map([instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Stopped),
        )]);
        let plan = reconciler().plan(&target, &current, &log());

        assert!(plan.to_create.is_empty());
        assert!(plan.to_destroy.is_empty());
        assert_eq!(plan.ensure_running.len(), 1);

        // The plan carries the live state, not the db's stale idea of it, so
        // the executor can tell a stopped instance from a running one.
        let instance = plan.ensure_running.iter().next().unwrap();
        assert!(instance.is_stopped());
    }

    #[test]
    fn a_state_the_db_has_not_caught_up_with_is_recorded() {
        let target = map([instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Starting),
        )]);
        let current = map([instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Running),
        )]);
        let plan = reconciler().plan(&target, &current, &log());

        assert_eq!(plan.status_updates.len(), 1);
        assert_eq!(
            plan.status_updates[0]
                .oxide_instance
                .as_ref()
                .map(|i| &i.state),
            Some(&InstanceState::Running),
        );
    }

    #[test]
    fn a_state_already_matching_the_rack_is_left_alone() {
        let both = || {
            map([instance(
                "ferris",
                "alpha",
                InstanceKind::Vivado,
                Some(InstanceState::Running),
            )])
        };
        let plan = reconciler().plan(&both(), &both(), &log());

        assert!(plan.status_updates.is_empty());
    }

    #[test]
    fn an_instance_the_rack_has_lost_has_its_record_cleared() {
        // The db still names an instance id, but nothing on the rack answers
        // to it any more.
        let target = map([instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Running),
        )]);
        let plan = reconciler().plan(&target, &InstanceMap::new(), &log());

        assert_eq!(plan.to_create.len(), 1);
        assert_eq!(plan.status_updates.len(), 1);
        assert!(plan.status_updates[0].oxide_instance.is_none());
    }

    #[test]
    fn every_instance_of_an_environment_gets_its_own_entry() {
        // The whole point of keying on the instance name: three instances of
        // one environment coexist, where keying on user or environment alone
        // would collapse them into one.
        let map = map(InstanceKind::ALL
            .map(|kind| instance("ferris", "alpha", kind, None)));

        assert_eq!(map.len(), 3);
    }

    #[test]
    fn the_artifact_instance_is_not_sized_like_a_build_machine() {
        // It runs an object store and nothing else. Giving it a builder's
        // shape costs every environment cores and memory that only the two
        // instances doing the compiling can use.
        let artifact = InstanceKind::Artifact.shape();

        for building in [InstanceKind::Vivado, InstanceKind::Helios] {
            let shape = building.shape();
            assert!(
                artifact.vcpus < shape.vcpus,
                "artifact has as many cpus as {building}",
            );
            assert!(
                artifact.memory_gib < shape.memory_gib,
                "artifact has as much memory as {building}",
            );
        }
    }

    #[test]
    fn instance_names_round_trip() {
        for kind in InstanceKind::ALL {
            let original = instance("ferris", "alpha", kind, None);
            let name = original.oxide_instance_name();
            assert_eq!(name, format!("vwsvc-ferris-alpha-{kind}"));

            let parsed =
                crate::oxide::parse_instance_name(&name).expect("parses back");
            assert_eq!(parsed.user, original.user);
            assert_eq!(parsed.environment, original.environment);
            assert_eq!(parsed.kind, original.kind);
        }
    }

    #[test]
    fn instances_that_are_not_ours_are_ignored() {
        // Nothing here may be mistaken for a vw instance, or a reconciler
        // pass would delete somebody else's work.
        for name in [
            "some-other-instance",
            "vwsvc-ferris-alpha",
            "vwsvc-ferris-alpha-vivado-extra",
            "vwsvc-ferris-alpha-mystery",
            "notvwsvc-ferris-alpha-vivado",
        ] {
            assert!(
                crate::oxide::parse_instance_name(name).is_none(),
                "'{name}' should not be treated as a vw instance",
            );
        }
    }

    #[test]
    fn environment_names_that_would_not_survive_the_scheme_are_rejected() {
        for good in ["alpha", "env2", "a", "x9y9"] {
            assert!(
                validate_environment_name(good).is_ok(),
                "'{good}' should be valid",
            );
        }
        for bad in ["", "my-env", "My", "my_env", "9lives", "a.b"] {
            assert!(
                validate_environment_name(bad).is_err(),
                "'{bad}' should be invalid",
            );
        }
    }

    #[test]
    fn hyphenated_github_usernames_are_allowed() {
        // Github hands these out and we do not get to choose them, so they
        // must work rather than lock somebody out.
        for good in ["foo-bar", "rcgoodfellow", "a-b-c", "x9"] {
            assert!(
                validate_user_name(good).is_ok(),
                "'{good}' should be valid",
            );
        }
        for bad in ["", "Foo", "foo_bar", "9lives", "foo-"] {
            assert!(
                validate_user_name(bad).is_err(),
                "'{bad}' should be invalid",
            );
        }
    }

    #[test]
    fn a_hyphenated_username_still_round_trips() {
        // Parsed from the right, so the hyphens land in the user and nowhere
        // else. Getting this wrong makes the instance unrecognizable, and the
        // reconciler would then recreate it on every pass.
        let original = instance("foo-bar", "alpha", InstanceKind::Vivado, None);
        let name = original.oxide_instance_name();
        assert_eq!(name, "vwsvc-foo-bar-alpha-vivado");

        let parsed =
            crate::oxide::parse_instance_name(&name).expect("parses back");
        assert_eq!(parsed.user, "foo-bar");
        assert_eq!(parsed.environment, "alpha");
        assert_eq!(parsed.kind, InstanceKind::Vivado);
    }

    #[test]
    fn a_create_is_recorded_before_it_is_requested() {
        // The control plane takes long enough over a create that an
        // environment would otherwise sit at "none" for the whole of it,
        // looking like nothing had happened.
        let target =
            map([instance("ferris", "alpha", InstanceKind::Vivado, None)]);
        let plan = reconciler().plan(&target, &InstanceMap::new(), &log());

        assert_eq!(plan.to_create.len(), 1);
        assert_eq!(plan.status_updates.len(), 1);

        let recorded = plan.status_updates[0]
            .oxide_instance
            .as_ref()
            .expect("a marker to show for it");
        assert_eq!(recorded.state, InstanceState::Creating);
        // No id yet: the rack has not answered, and inventing one would name
        // an instance that does not exist.
        assert!(recorded.id.is_none());
    }

    #[test]
    fn a_pending_create_is_not_mistaken_for_a_lost_instance() {
        // Second pass on an instance the rack has not started reporting yet.
        // Clearing the marker here would drop the user back to "none" and undo
        // the whole point of writing it.
        let mut pending =
            instance("ferris", "alpha", InstanceKind::Vivado, None);
        pending.oxide_instance = Some(OxideInstance {
            id: None,
            state: InstanceState::Creating,
            external_ip: None,
            internal_ip: None,
        });
        let target = map([pending]);

        let plan = reconciler().plan(&target, &InstanceMap::new(), &log());

        assert_eq!(plan.to_create.len(), 1, "still worth asking again");
        assert!(
            plan.status_updates.is_empty(),
            "the marker should be left alone",
        );
    }

    #[test]
    fn a_pending_marker_is_not_an_instance_on_the_rack() {
        // This is the distinction that decides whether a create happens at
        // all. Reading the marker as an existing instance made every pass skip
        // the create and report success, so the instance was never built and
        // nothing ever complained.
        let mut pending =
            instance("ferris", "alpha", InstanceKind::Vivado, None);
        pending.oxide_instance = Some(OxideInstance {
            id: None,
            state: InstanceState::Creating,
            external_ip: None,
            internal_ip: None,
        });
        assert!(!pending.exists_on_rack());
        // ... even though it does look like it is on its way up.
        assert!(pending.is_running_or_starting());

        // An id only ever comes from the rack, so one means it is really
        // there.
        let real = instance(
            "ferris",
            "alpha",
            InstanceKind::Vivado,
            Some(InstanceState::Creating),
        );
        assert!(real.exists_on_rack());

        // And nothing recorded at all is plainly absent.
        let absent = instance("ferris", "alpha", InstanceKind::Vivado, None);
        assert!(!absent.exists_on_rack());
    }
}
