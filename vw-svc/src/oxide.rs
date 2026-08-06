//! this module contains functionality for interacting with an Oxide Cloud
//! Computer.

use std::sync::OnceLock;

use futures::StreamExt;
use oxide::{
    types, ClientCurrentUserExt, ClientDisksExt, ClientImagesExt,
    ClientInstancesExt, ClientSystemStatusExt, Error,
};
use slog::{info, warn, Logger};
use vw_api_types_versions::latest::{
    EnvironmentImages, ImageRef, KeptImage, OxideInstance, RecycledImage,
};

use crate::reconciler::{InstanceKind, InstanceMap, UserInstance};

pub(crate) type OxideError = Error<types::Error>;

/// How many items to ask for per page when listing.
///
/// The Oxide API paginates and the client's `stream()` follows the page tokens
/// for us. Asking for everything in a single request instead — `limit` of
/// `u32::MAX` — leaves the control plane trying to satisfy a four-billion-item
/// query, which is a good way to have the request die at the transport layer
/// with nothing but "error sending request" to show for it.
const PAGE_SIZE: u32 = 100;

/// How long to give a single Oxide API call, in seconds.
///
/// The client's own default is 15 seconds, which creating an instance
/// comfortably exceeds — the control plane is still laying an image down on a
/// fresh disk. The request goes through regardless; it is only the client that
/// gives up, so a short timeout does not prevent the work, it just means the
/// reconciler never learns it succeeded and reports a failure for something
/// that worked.
const REQUEST_TIMEOUT_SECS: u64 = 300;

/// How long to give the initial connection, in seconds.
///
/// Set separately because it otherwise inherits [`REQUEST_TIMEOUT_SECS`], and
/// a rack that is simply not there should be reported in seconds rather than
/// minutes.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Boot disk of every instance the reconciler creates.
///
/// One size for every kind, because it is the images rather than the work that
/// set the floor: the vivado image alone is 512 GiB and an instance's disk
/// cannot be smaller than the image laid down on it. Cpu and memory do vary —
/// see [`InstanceKind::shape`].
const BOOT_DISK_GIB: u64 = 600;

/// Instances are named `vwsvc-{user}-{env}-{kind}`, and this prefix is the
/// only thing that marks an Oxide instance as ours.
///
/// Anything without it is somebody else's and is never touched, which is what
/// keeps a reconciler pass from deleting unrelated instances in the project.
pub(crate) const INSTANCE_PREFIX: &str = "vwsvc";

/// How to reach the Oxide API, recorded at startup.
///
/// Deliberately just the credentials rather than a live client — see
/// [`session`].
struct OxideConfig {
    endpoint: String,
    token: String,
    project: String,
}

/// `None` until [`init`] runs, and permanently `None` when the service was
/// started without Oxide credentials.
static OXIDE: OnceLock<Option<OxideConfig>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub(crate) enum InitError {
    #[error("the oxide configuration has already been initialized")]
    AlreadyInitialized,
}

/// Error conditions for opening a session against the Oxide API.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("this service has no oxide backend configured")]
    NotConfigured,
    #[error("building the oxide client failed")]
    Client(#[source] oxide::OxideAuthError),
}

/// Error conditions for choosing an image to boot an instance from.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageError {
    #[error("no image named '{0}' is visible to this service")]
    NoSuchImage(String),
    #[error(
        "no image matching '{0}*' is visible to this service; \
         name one explicitly to use a different image"
    )]
    NoMatchingImage(String),
    // No `{0}`: log sites report this through `InlineErrorChain`, which
    // appends the source itself. See `PassError`.
    //
    // Boxed because the oxide client's error is large enough that carrying it
    // inline makes every `Result` in this module the size of a failure that
    // almost never happens. `RelayError` boxes its own for the same reason.
    #[error("listing images failed")]
    List(#[source] Box<OxideError>),
}

impl From<OxideError> for ImageError {
    fn from(value: OxideError) -> Self {
        ImageError::List(Box::new(value))
    }
}

/// Establish the connection to the Oxide API.
///
/// `endpoint` and `token` are both `None` when the service is being run
/// without an Oxide backend, in which case every call in this module reports
/// that it is not configured and the reconciler does not run.
pub(crate) fn init(
    endpoint: Option<&str>,
    token: Option<&str>,
    project: &str,
) -> Result<(), InitError> {
    let config = match (endpoint, token) {
        (Some(endpoint), Some(token)) => Some(OxideConfig {
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
            project: project.to_owned(),
        }),
        _ => None,
    };

    OXIDE.set(config).map_err(|_| InitError::AlreadyInitialized)
}

/// Open a session for one unit of work — a reconciler pass, or one API
/// request.
///
/// Built fresh each time rather than kept alive for the life of the process.
/// The reconciler polls on an interval, so a long-lived client's connections
/// would spend nearly all their time idle and be dropped by the far end before
/// the next use. Picking one of those back up surfaces as:
///
/// ```text
/// client error (SendRequest): connection error: peer closed connection
/// without sending TLS close_notify
/// ```
///
/// because rustls treats a close without `close_notify` as an error rather
/// than a clean EOF (curl and OpenSSL do not). At this call rate there is
/// nothing to gain from holding a connection open between passes, and a client
/// that outlives its work is just somewhere for that class of bug to live.
pub(crate) fn session() -> Result<Session, SessionError> {
    let config = config().ok_or(SessionError::NotConfigured)?;

    let client = oxide::Client::new_authenticated_config(
        &oxide::ClientConfig::default()
            .with_host_and_token(&config.endpoint, &config.token)
            .with_timeout(REQUEST_TIMEOUT_SECS)
            .with_connect_timeout(CONNECT_TIMEOUT_SECS),
    )
    .map_err(SessionError::Client)?;

    Ok(Session {
        client,
        project: config.project.clone(),
    })
}

/// A connection to the Oxide API, scoped to the project we manage.
pub(crate) struct Session {
    client: oxide::Client,
    project: String,
}

/// Whether this service has an Oxide backend to reconcile against.
pub(crate) fn is_configured() -> bool {
    config().is_some()
}

fn config() -> Option<&'static OxideConfig> {
    OXIDE
        .get()
        .expect("the oxide client has not been initialized")
        .as_ref()
}

impl Session {
    /// Confirm the Oxide API is reachable and the configured credentials work.
    pub(crate) async fn ping(&self, log: &Logger) -> Result<(), OxideError> {
        self.client.ping().send().await?;
        info!(log, "oxide api reachable"; "project" => &self.project);
        Ok(())
    }

    /// Every instance in the project that this service manages.
    pub(crate) async fn get_instances(
        &self,
    ) -> Result<InstanceMap, OxideError> {
        let instances = self
            .client
            .instance_list()
            .project(self.project.as_str())
            .limit(4096)
            .send()
            .await?
            .items
            .clone();

        let managed = ours(instances.iter().map(|instance| {
            (instance.name.to_string(), instance.id, instance.run_state)
        }));

        // The external address is what somebody needs in order to ssh in, and
        // it is only available per instance rather than from the list.
        let mut map = InstanceMap::new();
        for mut instance in managed {
            let name = instance.oxide_instance_name();
            if instance.oxide_instance.is_some() {
                let external = self.external_ip(&name).await?;
                let internal = self.internal_ip(&name).await?;
                if let Some(oxide) = instance.oxide_instance.as_mut() {
                    oxide.external_ip = external;
                    oxide.internal_ip = internal;
                }
            }
            map.insert_overwrite(instance);
        }

        Ok(map)
    }

    /// The instance's address on the rack's own network.
    ///
    /// The primary interface's, since that is the one every instance has. A
    /// dual-stack interface reports its v4 address: it is what the agent binds
    /// and what a URL can name without bracketing.
    async fn internal_ip(
        &self,
        name: &str,
    ) -> Result<Option<std::net::IpAddr>, OxideError> {
        let interfaces = self
            .client
            .instance_network_interface_list()
            .instance(name)
            .project(self.project.as_str())
            .limit(PAGE_SIZE)
            .send()
            .await?
            .items
            .clone();

        let primary = interfaces
            .iter()
            .find(|interface| interface.primary)
            .or_else(|| interfaces.first());

        Ok(primary.map(|interface| match &interface.ip_stack {
            types::PrivateIpStack::V4(v4) => std::net::IpAddr::V4(v4.ip),
            types::PrivateIpStack::V6(v6) => std::net::IpAddr::V6(v6.ip),
            types::PrivateIpStack::DualStack { v4, .. } => {
                std::net::IpAddr::V4(v4.ip)
            }
        }))
    }

    /// The address an instance can be reached on from outside the rack.
    ///
    /// SNAT addresses are skipped: they carry outbound traffic only, so one
    /// would look like a way in without being one.
    async fn external_ip(
        &self,
        name: &str,
    ) -> Result<Option<std::net::IpAddr>, OxideError> {
        let addresses = self
            .client
            .instance_external_ip_list()
            .instance(name)
            .project(self.project.as_str())
            .send()
            .await?
            .items
            .clone();

        Ok(addresses.iter().find_map(|address| match address {
            types::ExternalIp::Ephemeral { ip, .. }
            | types::ExternalIp::Floating { ip, .. } => Some(*ip),
            types::ExternalIp::Snat { .. } => None,
        }))
    }

    /// Register every environment key the instances about to be created will
    /// reference.
    ///
    /// Once per pass and one at a time, rather than from inside each create.
    /// An environment's three instances share a single key, and creating them
    /// concurrently had all three finding it absent and all three trying to
    /// register it — the one that got there first won, and the other two
    /// failed on a name that now existed, taking their instance creates down
    /// with them.
    pub(crate) async fn ensure_ssh_keys(
        &self,
        instances: &InstanceMap,
        log: &Logger,
    ) -> Result<(), OxideError> {
        // Collapsed by key name, so an environment is considered once however
        // many of its instances are being created.
        let mut wanted = std::collections::BTreeMap::new();
        for instance in instances.iter() {
            let Some(public_key) = instance.public_key.as_deref() else {
                warn!(log, "environment has no ssh key recorded";
                    "instance" => instance.oxide_instance_name(),
                );
                continue;
            };
            wanted.entry(instance.ssh_key_name()).or_insert((
                instance.user.clone(),
                instance.environment.clone(),
                public_key.to_owned(),
            ));
        }
        if wanted.is_empty() {
            return Ok(());
        }

        let existing = self.ssh_key_names().await?;

        for (name, (user, environment, public_key)) in wanted {
            if existing.contains(&name) {
                continue;
            }

            info!(log, "registering environment ssh key"; "key" => &name);
            let created = self
                .client
                .current_user_ssh_key_create()
                .body(types::SshKeyCreate {
                    name: name.parse().map_err(bad_name)?,
                    description: format!("vw environment {user}/{environment}"),
                    public_key,
                })
                .send()
                .await;

            if let Err(e) = created {
                // Another writer got there between the list and the create.
                // The key being present is the outcome we wanted, so this is
                // not a failure.
                if already_exists(&e) {
                    info!(log, "environment ssh key was already registered";
                        "key" => &name,
                    );
                    continue;
                }
                return Err(e);
            }
        }

        Ok(())
    }

    /// The names of every ssh key on the silo user this service acts as.
    async fn ssh_key_names(
        &self,
    ) -> Result<std::collections::BTreeSet<String>, OxideError> {
        let mut names = std::collections::BTreeSet::new();
        let mut keys = self
            .client
            .current_user_ssh_key_list()
            .limit(PAGE_SIZE)
            .stream();
        while let Some(key) = keys.next().await {
            names.insert(key?.name.to_string());
        }
        Ok(names)
    }

    /// Delete silo ssh keys belonging to environments that are gone.
    ///
    /// Same reasoning as the disks: nothing else cleans these up, and a key
    /// that opens instances which no longer exist is just clutter in the
    /// silo's key list.
    pub(crate) async fn reap_ssh_keys(
        &self,
        target: &InstanceMap,
        log: &Logger,
    ) -> Result<(), OxideError> {
        // Every key an environment in the db still wants.
        let wanted: std::collections::BTreeSet<String> =
            target.iter().map(|i| i.ssh_key_name()).collect();

        let mut names = Vec::new();
        let mut keys = self
            .client
            .current_user_ssh_key_list()
            .limit(PAGE_SIZE)
            .stream();
        while let Some(key) = keys.next().await {
            let name = key?.name.to_string();
            // Ours by the same prefix rule as everything else: this key list
            // belongs to a silo user that may well have keys of their own.
            if name.starts_with(&format!("{INSTANCE_PREFIX}-"))
                && !wanted.contains(&name)
            {
                names.push(name);
            }
        }

        for name in names {
            info!(log, "deleting orphaned ssh key"; "key" => &name);
            let deleted = self
                .client
                .current_user_ssh_key_delete()
                .ssh_key(name.as_str())
                .send()
                .await;

            if let Err(e) = deleted {
                if is_inconclusive(&e) {
                    warn!(log, "ssh key delete did not report back";
                        "key" => &name,
                        slog_error_chain::InlineErrorChain::new(&e),
                    );
                    continue;
                }
                return Err(e);
            }
        }

        Ok(())
    }

    /// Delete boot disks belonging to instances nothing wants any more.
    ///
    /// Deleting an instance leaves its boot disk behind, detached, so without
    /// this every environment that goes away costs the rack a disk the size of
    /// [`BOOT_DISK_GIB`] forever.
    ///
    /// Disks carry the same name as the instance they were built for, so the
    /// same rule decides ownership: a disk whose name does not parse as one of
    /// ours is somebody else's and is left alone.
    pub(crate) async fn reap_disks(
        &self,
        target: &InstanceMap,
        log: &Logger,
    ) -> Result<(), OxideError> {
        let mut disks = self
            .client
            .disk_list()
            .project(self.project.as_str())
            .limit(PAGE_SIZE)
            .stream();

        while let Some(disk) = disks.next().await {
            let disk = disk?;
            let name = disk.name.to_string();

            if !reapable(&name, &disk.state, target) {
                continue;
            }

            info!(log, "deleting orphaned boot disk"; "disk" => &name);
            let deleted = self
                .client
                .disk_delete()
                .disk(name.as_str())
                .project(self.project.as_str())
                .send()
                .await;

            if let Err(e) = deleted {
                // As with instances, a delete whose connection died may have
                // gone through. One disk is also no reason to stop reaping the
                // rest, so report and carry on either way.
                if is_inconclusive(&e) {
                    warn!(log, "boot disk delete did not report back";
                        "disk" => &name,
                        slog_error_chain::InlineErrorChain::new(&e),
                    );
                    continue;
                }
                return Err(e);
            }
        }

        Ok(())
    }

    /// Resolve the images an environment's instances should boot from.
    ///
    /// A name given explicitly must exist. A kind left unset resolves to the
    /// newest image whose name starts with that kind's prefix.
    pub(crate) async fn resolve_images(
        &self,
        vivado: Option<&str>,
        helios: Option<&str>,
        artifact: Option<&str>,
    ) -> Result<EnvironmentImages, ImageError> {
        let images = self.image_facts().await?;

        Ok(EnvironmentImages {
            vivado: choose_image(&images, InstanceKind::Vivado, vivado)?,
            helios: choose_image(&images, InstanceKind::Helios, helios)?,
            artifact: choose_image(&images, InstanceKind::Artifact, artifact)?,
        })
    }

    /// Every visible image, reduced to what this service acts on.
    async fn image_facts(&self) -> Result<Vec<ImageFacts>, ImageError> {
        Ok(self.visible_images().await?.iter().map(facts).collect())
    }

    /// Delete the service's images that nothing is using and nothing would
    /// use, and report both halves of what that came to.
    ///
    /// The whole plan is worked out before the first deletion, so what gets
    /// reported as kept is decided against the same picture of the rack as
    /// what gets deleted — rather than "newest" moving as the list is walked.
    /// See [`plan_recycle`] for what spares an image.
    ///
    /// A `dry_run` pass does everything except the deleting, and says so in
    /// the report.
    ///
    /// One image failing to delete does not stop the pass: the images are
    /// independent of each other, and stopping would leave the caller with a
    /// report claiming deletions that came after the failure. A failed
    /// deletion is logged and the image is reported as neither deleted nor
    /// kept, which is the truth — it is still there, and not because anything
    /// spared it.
    pub(crate) async fn recycle_images(
        &self,
        in_use: &std::collections::BTreeMap<uuid::Uuid, Vec<String>>,
        dry_run: bool,
        log: &Logger,
    ) -> Result<RecyclePlan, ImageError> {
        let images = self.image_facts().await?;
        let mut plan = plan_recycle(&images, in_use);

        if dry_run {
            return Ok(plan);
        }

        let mut deleted = Vec::with_capacity(plan.delete.len());
        for image in plan.delete {
            // By name rather than id: the delete endpoint takes either, and a
            // name is what the log and the report are in terms of.
            let result = self
                .client
                .image_delete()
                .image(image.name.as_str())
                .project(self.project.as_str())
                .send()
                .await;
            match result {
                Ok(_) => {
                    info!(log, "recycled an image";
                        "image" => &image.name,
                        "id" => %image.id,
                    );
                    deleted.push(image);
                }
                Err(e) => warn!(log, "cannot delete an image";
                    "image" => &image.name,
                    "id" => %image.id,
                    slog_error_chain::InlineErrorChain::new(&e),
                ),
            }
        }
        plan.delete = deleted;

        Ok(plan)
    }

    /// Every image the service can see, from both the project and the silo.
    ///
    /// Project images shadow silo images of the same name, matching how the
    /// control plane resolves them.
    async fn visible_images(&self) -> Result<Vec<types::Image>, ImageError> {
        let mut images = Vec::new();

        let mut silo = self.client.image_list().limit(PAGE_SIZE).stream();
        while let Some(image) = silo.next().await {
            images.push(image?);
        }

        let mut project = self
            .client
            .image_list()
            .project(self.project.as_str())
            .limit(PAGE_SIZE)
            .stream();
        while let Some(image) = project.next().await {
            images.push(image?);
        }

        Ok(images)
    }
    pub(crate) async fn create_instance(
        &self,
        instance: &UserInstance,
        log: &Logger,
    ) -> Result<(), OxideError> {
        let name = instance.oxide_instance_name();

        let Some(image) = instance.image.as_ref() else {
            warn!(log, "cannot create instance without an image";
                "instance" => &name,
            );
            return Ok(());
        };

        // The boot disk is created along with the instance and is what the image
        // is laid down on.
        let boot_disk = types::InstanceDiskAttachment::Create {
            description: format!("vw {} boot disk", instance.kind),
            disk_backend: types::DiskBackend::Distributed(
                types::DiskSource::Image {
                    image_id: image.id,
                    read_only: false,
                },
            ),
            name: name.parse().map_err(bad_name)?,
            size: types::ByteCount(BOOT_DISK_GIB * 1024 * 1024 * 1024),
        };

        let shape = instance.kind.shape();

        // Registered once per pass by `ensure_ssh_keys`, before any of this
        // environment's instances are created.
        let key_name = instance
            .public_key
            .as_ref()
            .map(|_| instance.ssh_key_name());

        let body = types::InstanceCreate {
            name: name.parse().map_err(bad_name)?,
            description: format!(
                "vw {} instance for {}/{}",
                instance.kind, instance.user, instance.environment
            ),
            hostname: instance.hostname().parse().map_err(bad_name)?,
            ncpus: types::InstanceCpuCount(shape.vcpus),
            memory: types::ByteCount(shape.memory_gib * 1024 * 1024 * 1024),
            boot_disk: Some(boot_disk),
            // Instances are reachable from outside the rack so that the vw client
            // and its source-sync daemon can talk to them directly.
            external_ips: vec![types::ExternalIpCreate::Ephemeral {
                pool_selector: types::PoolSelector::Auto { ip_version: None },
            }],
            start: true,
            // Attached by name. Without this the instance comes up with no
            // way in.
            ssh_public_keys: Some(
                key_name
                    .iter()
                    .map(|name| {
                        types::NameOrId::Name(
                            name.parse().expect("a name we built ourselves"),
                        )
                    })
                    .collect(),
            ),
            ..default_instance_create()
        };

        info!(log, "creating instance";
            "instance" => &name,
            "image" => &image.name,
            "ssh_key" => key_name.as_deref().unwrap_or("none"),
            "vcpus" => shape.vcpus,
            "memory_gib" => shape.memory_gib,
        );
        self.client
            .instance_create()
            .project(self.project.as_str())
            .body(body)
            .send()
            .await?;

        Ok(())
    }

    pub(crate) async fn delete_instance(
        &self,
        instance: &UserInstance,
        log: &Logger,
    ) -> Result<(), OxideError> {
        let name = instance.oxide_instance_name();

        // An instance has to be stopped before the control plane will delete it.
        // Stopping one that is already stopping or stopped is harmless, so the
        // only states worth acting on are the ones that are still running.
        if instance.is_running_or_starting() {
            info!(log, "stopping instance before delete"; "instance" => &name);
            self.client
                .instance_stop()
                .instance(name.as_str())
                .project(self.project.as_str())
                .send()
                .await?;
            // The stop is not instantaneous. Leave the delete for a later pass
            // rather than blocking this one waiting for the state to settle.
            return Ok(());
        }
        if !instance.is_stopped() {
            // Still on its way down, or in a state the control plane will not let
            // us delete from. Try again next pass.
            return Ok(());
        }

        info!(log, "deleting instance"; "instance" => &name);
        self.client
            .instance_delete()
            .instance(name.as_str())
            .project(self.project.as_str())
            .send()
            .await?;

        Ok(())
    }

    pub(crate) async fn ensure_instance_running(
        &self,
        instance: &UserInstance,
        log: &Logger,
    ) -> Result<(), OxideError> {
        match run_action(instance) {
            RunAction::Create => self.create_instance(instance, log).await,
            RunAction::Wait => Ok(()),
            RunAction::Start => {
                let name = instance.oxide_instance_name();
                info!(log, "starting instance"; "instance" => &name);
                self.client
                    .instance_start()
                    .instance(name.as_str())
                    .project(self.project.as_str())
                    .send()
                    .await?;
                Ok(())
            }
        }
    }
}

/// Narrow a project's instances down to the ones this service manages.
///
/// NOTE the Oxide Cloud Computer does not have tags, so we need to encode
/// vw instance semantics in names. The format is
///
///   vwsvc-{user name}-{env name}-{instance kind}
///
/// where instance kind is currently one of vivado, helios or artifact.
///
/// This is the only thing standing between a reconciler pass and somebody
/// else's instances: the project holds more than ours, and an instance that
/// does not parse back out to the shape above is dropped here rather than
/// carried forward as an instance with no target — which is what a pass
/// deletes. Nothing downstream re-checks, so the filter has to be right here.
fn ours(
    instances: impl IntoIterator<Item = (String, uuid::Uuid, types::InstanceState)>,
) -> InstanceMap {
    let mut map = InstanceMap::new();
    for (name, id, state) in instances {
        let Some(mut instance) = parse_instance_name(&name) else {
            continue;
        };
        instance.oxide_instance = Some(OxideInstance {
            id: Some(id),
            state,
            // Filled in per instance by the caller; the list does not carry
            // addresses.
            external_ip: None,
            internal_ip: None,
        });
        // Two Oxide instances cannot share a name, so this cannot collide.
        map.insert_overwrite(instance);
    }
    map
}

/// What a pass should do with an instance it wants running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunAction {
    /// Not on the rack. Make it.
    Create,
    /// Up, on its way up, or on its way down. Leave it for a later pass.
    Wait,
    /// Settled and stopped. Start it.
    Start,
}

/// Decide what to do with an instance, separately from doing it.
///
/// Split out because the first branch is easy to get subtly wrong and
/// impossible to notice: `oxide_instance` holds both what the rack reported
/// and the marker written when a create is asked for, and reading the marker
/// as an existing instance makes every pass skip the create, report success,
/// and build nothing.
pub(crate) fn run_action(instance: &UserInstance) -> RunAction {
    // Nothing on the rack yet, so there is nothing to start.
    if !instance.exists_on_rack() {
        return RunAction::Create;
    }
    // Starting an instance that is mid-transition is at best a no-op and at
    // worst an error.
    if instance.is_running_or_starting() {
        return RunAction::Wait;
    }
    // Anything still shutting down is picked up once it has settled; only a
    // fully stopped instance can be started.
    if !instance.is_stopped() {
        return RunAction::Wait;
    }
    RunAction::Start
}

/// Whether the control plane refused a create because the thing is already
/// there.
///
/// Two writers racing on the same name is not a failure when the name existing
/// is exactly the outcome wanted. Matched on the control plane's own error
/// code rather than the status, which is a plain `400 Bad Request` and would
/// otherwise swallow genuinely malformed requests:
///
/// ```text
/// status: 400 Bad Request; value: Error { error_code: Some("ObjectAlreadyExists"),
/// message: "already exists: ssh-key \"vwsvc-rcgoodfellow-darmok\"", .. }
/// ```
fn already_exists(error: &OxideError) -> bool {
    match error {
        Error::ErrorResponse(response) => {
            response.error_code.as_deref() == Some("ObjectAlreadyExists")
        }
        _ => false,
    }
}

/// Whether an error leaves the outcome of a request genuinely unknown.
///
/// A request whose connection died on the way to or from the rack may well
/// have been carried out anyway — the control plane takes longer to delete an
/// instance than something in the path is willing to hold a connection open
/// for, so the work happens and the answer never arrives. Treating that as a
/// failure is wrong twice over: it reports a problem where there is none, and
/// it abandons the rest of the pass over something the next pass will see the
/// truth about.
///
/// A response that did arrive is a different matter. If the control plane said
/// no, it meant it.
///
/// The match is written out rather than using a wildcard so that a new variant
/// in the client has to be classified rather than silently assumed benign.
pub(crate) fn is_inconclusive(error: &OxideError) -> bool {
    match error {
        // Nothing came back, so the request's fate is unknown.
        Error::CommunicationError(_)
        | Error::ResponseBodyError(_)
        | Error::InvalidUpgrade(_) => true,

        // The rack answered. Whatever it said, it is the truth.
        Error::ErrorResponse(_)
        | Error::UnexpectedResponse(_)
        | Error::InvalidResponsePayload(_, _)
        // Ours to fix, and retrying will not help.
        | Error::InvalidRequest(_)
        | Error::Custom(_) => false,
    }
}

/// Whether a disk should be deleted on this pass.
///
/// Three things all have to hold, and the first is the one that matters: the
/// project holds disks belonging to other people's instances, and deleting one
/// of those destroys work this service never created.
fn reapable(
    name: &str,
    state: &types::DiskState,
    target: &InstanceMap,
) -> bool {
    // Ours, by the same naming rule the instances use.
    parse_instance_name(name).is_some()
        // Not wanted. This also covers the window during creation, when the
        // disk exists before the instance it belongs to does.
        && target.get(name).is_none()
        // Deletable. Anything still attached belongs to an instance on its way
        // out, and a later pass gets it once the control plane has finished.
        && matches!(state, types::DiskState::Detached)
}

/// Recover the environment an Oxide instance belongs to from its name.
///
/// Returns `None` for anything that is not one of ours.
///
/// Parsed from the right, because the username is the one field that may
/// contain a `-`: Github hands out names like `foo-bar` and we do not get to
/// choose them. The kind and the environment name are both guaranteed
/// hyphen-free — the kind by being one of a fixed set, the environment name by
/// [`crate::reconciler::validate_environment_name`] — so whatever sits between
/// the prefix and those two is the user, hyphens and all.
pub(crate) fn parse_instance_name(name: &str) -> Option<UserInstance> {
    let rest = name.strip_prefix(INSTANCE_PREFIX)?.strip_prefix('-')?;

    let (rest, kind) = rest.rsplit_once('-')?;
    let kind = kind.parse().ok()?;
    let (user, environment) = rest.rsplit_once('-')?;

    if user.is_empty() || environment.is_empty() {
        return None;
    }

    Some(UserInstance {
        user: user.to_owned(),
        environment: environment.to_owned(),
        kind,
        // The image an existing instance was built from, and the key it was
        // given, are recorded in the db rather than recoverable from the
        // instance itself.
        image: None,
        public_key: None,
        oxide_instance: None,
    })
}

/// Pick the image for `kind`, either the one named or the newest match.
fn choose_image(
    images: &[ImageFacts],
    kind: InstanceKind,
    requested: Option<&str>,
) -> Result<ImageRef, ImageError> {
    if let Some(name) = requested {
        return images
            .iter()
            .find(|image| image.name == name)
            .map(ImageFacts::as_ref)
            .ok_or_else(|| ImageError::NoSuchImage(name.to_owned()));
    }

    // Newest by creation time rather than by the date in the name: the
    // timestamp is authoritative and always present, whereas the name suffix
    // is a convention.
    images
        .iter()
        .filter(|image| image.name.starts_with(kind.image_prefix()))
        .max_by_key(|image| image.created)
        .map(ImageFacts::as_ref)
        .ok_or_else(|| {
            ImageError::NoMatchingImage(kind.image_prefix().to_owned())
        })
}

/// The facts about an image that this service acts on.
///
/// A narrow view of [`types::Image`], and the only description of an image
/// that resolution and recycling both work from — so what "the newest image
/// of a kind" means cannot come apart between the code that boots one and the
/// code that deletes one. Cheap to build in a test, which is the other half
/// of why it exists.
#[derive(Debug, Clone)]
pub(crate) struct ImageFacts {
    pub(crate) id: uuid::Uuid,
    pub(crate) name: String,
    /// Whether the image lives in this service's project rather than being
    /// published to the whole silo.
    pub(crate) in_project: bool,
    pub(crate) created: chrono::DateTime<chrono::Utc>,
}

impl ImageFacts {
    fn as_ref(&self) -> ImageRef {
        ImageRef {
            id: self.id,
            name: self.name.clone(),
        }
    }
}

/// What a recycle pass should do, worked out before anything is deleted.
#[derive(Debug, Default)]
pub(crate) struct RecyclePlan {
    pub(crate) delete: Vec<RecycledImage>,
    pub(crate) keep: Vec<KeptImage>,
}

/// Decide which images a recycle pass may delete.
///
/// Deleting an image cannot be undone and the images take the better part of
/// an hour to rebuild, so this is written as four reasons to spare one rather
/// than one reason to remove it. An image survives if any of them holds:
///
/// - it is not one of ours — its name does not match an [`InstanceKind`]
///   prefix — so the rack's base images are never this service's to reclaim;
/// - it is not in this service's project: a silo-scoped image is shared with
///   everyone, and who else boots it is not a question answerable from here;
/// - it is the newest of its kind, decided by asking [`choose_image`] rather
///   than by a second implementation of "newest" that could drift from it, so
///   what an environment created a moment from now would boot is exactly what
///   cannot be taken away;
/// - an environment names it, however old it is and however many newer
///   images of its kind exist.
///
/// `in_use` maps an image id to the environments booting it, as `user/name`.
pub(crate) fn plan_recycle(
    images: &[ImageFacts],
    in_use: &std::collections::BTreeMap<uuid::Uuid, Vec<String>>,
) -> RecyclePlan {
    // Asked of the function that answers it when an environment is created,
    // over the same images, so the two cannot disagree about which image is
    // the newest of a kind. A kind with no images has nothing to protect —
    // and nothing to delete either.
    let latest: std::collections::BTreeSet<uuid::Uuid> = InstanceKind::ALL
        .iter()
        .filter_map(|kind| choose_image(images, *kind, None).ok())
        .map(|image| image.id)
        .collect();

    let mut plan = RecyclePlan::default();
    for image in images {
        // Not named like one of ours, or not ours to reach: skipped rather
        // than reported, since a rack's image list is mostly this and burying
        // the few images the pass is about in it helps nobody.
        if !is_ours(&image.name) || !image.in_project {
            continue;
        }

        let is_latest = latest.contains(&image.id);
        let used_by = in_use.get(&image.id).cloned().unwrap_or_default();
        if is_latest || !used_by.is_empty() {
            plan.keep.push(KeptImage {
                id: image.id,
                name: image.name.clone(),
                latest: is_latest,
                used_by,
            });
        } else {
            plan.delete.push(RecycledImage {
                id: image.id,
                name: image.name.clone(),
            });
        }
    }
    plan
}

fn facts(image: &types::Image) -> ImageFacts {
    ImageFacts {
        id: image.id,
        name: image.name.to_string(),
        in_project: image.project_id.is_some(),
        created: image.time_created,
    }
}

/// Whether an image is one this service publishes and boots.
fn is_ours(name: &str) -> bool {
    InstanceKind::ALL
        .iter()
        .any(|kind| name.starts_with(kind.image_prefix()))
}

/// The fields of an `InstanceCreate` the reconciler does not set.
fn default_instance_create() -> types::InstanceCreate {
    types::InstanceCreate {
        anti_affinity_groups: Vec::new(),
        auto_restart_policy: None,
        boot_disk: None,
        cpu_platform: None,
        description: String::new(),
        disks: Vec::new(),
        enable_jumbo_frames: false,
        external_ips: Vec::new(),
        hostname: "placeholder".parse().expect("valid hostname"),
        memory: types::ByteCount(0),
        multicast_groups: Vec::new(),
        name: "placeholder".parse().expect("valid instance name"),
        ncpus: types::InstanceCpuCount(0),
        network_interfaces:
            types::InstanceNetworkInterfaceAttachment::DefaultIpv4,
        ssh_public_keys: None,
        start: true,
        user_data: String::new(),
    }
}

/// An instance name the control plane will not accept.
///
/// User and environment names are validated on the way in, so reaching this
/// means the naming scheme itself is wrong rather than the caller's input.
fn bad_name(e: impl std::fmt::Display) -> OxideError {
    OxideError::InvalidRequest(format!("invalid instance name: {e}"))
}

#[cfg(test)]
mod test {
    use super::*;
    use uuid::Uuid;

    fn listed(name: &str) -> (String, Uuid, types::InstanceState) {
        (
            name.to_owned(),
            Uuid::new_v4(),
            types::InstanceState::Running,
        )
    }

    #[test]
    fn only_instances_we_named_are_managed() {
        // A realistic project: ours mixed in with everything else that
        // happens to live there. Only ours may come out the other side,
        // because whatever does is a candidate for deletion.
        let map = ours([
            listed("vwsvc-ferris-alpha-vivado"),
            listed("vwsvc-ferris-alpha-helios"),
            listed("vwsvc-foo-bar-beta-artifact"),
            // Not ours, and deleting any of these would be somebody's bad day.
            listed("build-runner-3"),
            listed("gimlet-dev"),
            listed("vwsvc"),
            listed("vwsvc-ferris"),
            listed("vwsvc-ferris-alpha"),
            listed("vwsvc-ferris-alpha-mystery"),
            listed("notvwsvc-ferris-alpha-vivado"),
            listed("vwsvcferris-alpha-vivado"),
        ]);

        let mut names: Vec<String> =
            map.iter().map(|i| i.oxide_instance_name()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "vwsvc-ferris-alpha-helios",
                "vwsvc-ferris-alpha-vivado",
                "vwsvc-foo-bar-beta-artifact",
            ]
        );
    }

    #[test]
    fn a_managed_instance_keeps_its_identity_and_state() {
        let id = Uuid::new_v4();
        let map = ours([(
            String::from("vwsvc-foo-bar-alpha-vivado"),
            id,
            types::InstanceState::Stopped,
        )]);

        let instance = map.iter().next().expect("one instance");
        assert_eq!(instance.user, "foo-bar");
        assert_eq!(instance.environment, "alpha");
        assert_eq!(instance.kind, InstanceKind::Vivado);

        // The name has to reconstruct exactly, because that is what a delete
        // is aimed at. A lossy round trip would target the wrong instance.
        assert_eq!(
            instance.oxide_instance_name(),
            "vwsvc-foo-bar-alpha-vivado"
        );

        let oxide = instance.oxide_instance.as_ref().expect("carries state");
        assert_eq!(oxide.id, Some(id));
        assert_eq!(oxide.state, types::InstanceState::Stopped);
    }

    /// A target map holding exactly the named instances.
    fn wanted(names: &[&str]) -> InstanceMap {
        let mut map = InstanceMap::new();
        for name in names {
            map.insert_overwrite(
                parse_instance_name(name).expect("a well formed name"),
            );
        }
        map
    }

    #[test]
    fn only_our_disks_are_ever_reaped() {
        // Names taken from a real project: ours alongside boot disks that
        // belong to instances this service knows nothing about. Reaping one of
        // those would destroy somebody else's machine.
        let target = wanted(&[]);
        for theirs in [
            "katie-test-redhawk-dev-20260724035113-48889b",
            "rhbs-noble-cloud-a6640b",
            "vhdl-sim-jammy-server-6c074e",
        ] {
            assert!(
                !reapable(theirs, &types::DiskState::Detached, &target),
                "'{theirs}' is not ours and must be left alone",
            );
        }

        for ours in [
            "vwsvc-rcgoodfellow-darmok-vivado",
            "vwsvc-rcgoodfellow-darmok-helios",
            "vwsvc-rcgoodfellow-darmok-artifact",
        ] {
            assert!(
                reapable(ours, &types::DiskState::Detached, &target),
                "'{ours}' is ours, unwanted and detached, so it should go",
            );
        }
    }

    #[test]
    fn a_disk_its_environment_still_wants_is_kept() {
        let target = wanted(&["vwsvc-rcgoodfellow-darmok-vivado"]);

        assert!(!reapable(
            "vwsvc-rcgoodfellow-darmok-vivado",
            &types::DiskState::Detached,
            &target,
        ));
        // A sibling whose environment was deleted is still fair game.
        assert!(reapable(
            "vwsvc-rcgoodfellow-darmok-helios",
            &types::DiskState::Detached,
            &target,
        ));
    }

    #[test]
    fn a_disk_still_in_use_is_left_for_a_later_pass() {
        let target = wanted(&[]);
        let name = "vwsvc-rcgoodfellow-darmok-vivado";
        let instance = Uuid::new_v4();

        // Only a detached disk can be deleted; the rest are mid-transition.
        for busy in [
            types::DiskState::Attached(instance),
            types::DiskState::Attaching(instance),
            types::DiskState::Detaching(instance),
            types::DiskState::Creating,
            types::DiskState::Finalizing,
        ] {
            assert!(
                !reapable(name, &busy, &target),
                "a disk in {busy:?} cannot be deleted yet",
            );
        }
    }

    #[test]
    fn a_request_that_never_answered_is_not_a_failure() {
        // The rack takes longer to delete an instance than something in the
        // path will hold a connection open for, so the work happens and the
        // answer never arrives. Calling that a failure reports a problem that
        // is not there and abandons the rest of the pass.
        //
        // `reqwest::Error` has no public constructor, so the transport
        // variants cannot be built here; `is_inconclusive` matches every
        // variant explicitly instead, which makes the compiler insist that any
        // new one gets classified. What is testable is the other side of the
        // rule: an answer that did arrive is always conclusive.
        assert!(!is_inconclusive(&OxideError::InvalidRequest(
            "malformed".into()
        )));
        assert!(!is_inconclusive(&OxideError::Custom("nope".into())));
    }

    #[test]
    fn a_name_that_is_already_taken_is_not_a_failure() {
        // An environment's three instances share one ssh key, and two writers
        // racing to register it is not a problem when all that was wanted is
        // for it to exist. Matched on the control plane's error code rather
        // than the status, which is a plain 400 that a malformed request also
        // carries — as the other two cases here check.
        assert!(!already_exists(&OxideError::InvalidRequest("bad".into())));
        assert!(!already_exists(&OxideError::Custom("nope".into())));
    }

    fn recorded(state: Option<types::InstanceState>) -> UserInstance {
        let mut instance = parse_instance_name("vwsvc-ferris-alpha-vivado")
            .expect("a well formed name");
        instance.oxide_instance = state.map(|state| OxideInstance {
            id: Some(Uuid::new_v4()),
            state,
            external_ip: None,
            internal_ip: None,
        });
        instance
    }

    #[test]
    fn an_instance_the_rack_has_not_got_is_created() {
        // Nothing recorded at all.
        assert_eq!(run_action(&recorded(None)), RunAction::Create);

        // And the case that actually bit: this service's own marker for a
        // create it asked for. It carries `Creating`, so anything treating it
        // as a live instance decides it is already on its way up and never
        // builds it — every pass, without an error to show for it.
        let mut pending = recorded(None);
        pending.oxide_instance = Some(OxideInstance {
            id: None,
            state: types::InstanceState::Creating,
            external_ip: None,
            internal_ip: None,
        });
        assert_eq!(run_action(&pending), RunAction::Create);
    }

    #[test]
    fn an_instance_in_motion_is_left_alone() {
        for state in [
            types::InstanceState::Creating,
            types::InstanceState::Starting,
            types::InstanceState::Running,
            types::InstanceState::Rebooting,
            types::InstanceState::Migrating,
            types::InstanceState::Repairing,
            types::InstanceState::Stopping,
        ] {
            assert_eq!(
                run_action(&recorded(Some(state))),
                RunAction::Wait,
                "{state} needs no action",
            );
        }
    }

    #[test]
    fn a_stopped_instance_is_started() {
        assert_eq!(
            run_action(&recorded(Some(types::InstanceState::Stopped))),
            RunAction::Start,
        );
    }

    #[test]
    fn an_instance_names_itself_after_its_kind_and_environment() {
        // The Oxide instance name has to be unique project-wide and so drags
        // the owner and a prefix along; the shell prompt does not need any of
        // it.
        for (kind, expected) in [
            (InstanceKind::Vivado, "vivado-darmok"),
            (InstanceKind::Helios, "helios-darmok"),
            (InstanceKind::Artifact, "artifact-darmok"),
        ] {
            let mut instance =
                parse_instance_name("vwsvc-rcgoodfellow-darmok-vivado")
                    .expect("a well formed name");
            instance.kind = kind;

            assert_eq!(instance.hostname(), expected);
            // No dot: cloud-init would read one as an FQDN separator and keep
            // only the first label, dropping the environment from the name the
            // box calls itself.
            assert!(
                !expected.contains('.'),
                "{expected} would be truncated to its first label",
            );
            // And the control plane has to accept it, which is the part a
            // typo would only reveal at create time.
            assert!(
                expected.parse::<types::Hostname>().is_ok(),
                "{expected} is not a hostname the control plane will take",
            );
        }
    }

    #[test]
    fn a_hyphenated_owner_does_not_reach_the_hostname() {
        // Github names may carry hyphens, which are fine in a hostname label
        // but would put the owner into a name nobody inside the environment
        // needs to read.
        let instance = parse_instance_name("vwsvc-foo-bar-darmok-vivado")
            .expect("a well formed name");
        assert_eq!(instance.user, "foo-bar");
        assert_eq!(instance.hostname(), "vivado-darmok");
    }
}

#[cfg(test)]
mod recycle_test {
    use super::*;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    /// An image in this service's project, `minutes` after the epoch.
    fn ours(name: &str, minutes: i64) -> ImageFacts {
        ImageFacts {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            in_project: true,
            created: chrono::DateTime::from_timestamp(minutes * 60, 0)
                .expect("a representable time"),
        }
    }

    /// The same, published to the whole silo rather than to our project.
    fn silo(name: &str, minutes: i64) -> ImageFacts {
        ImageFacts {
            in_project: false,
            ..ours(name, minutes)
        }
    }

    fn names(images: &[RecycledImage]) -> Vec<&str> {
        images.iter().map(|i| i.name.as_str()).collect()
    }

    fn kept_names(images: &[KeptImage]) -> Vec<&str> {
        images.iter().map(|i| i.name.as_str()).collect()
    }

    /// A rack's worth of images: three generations of each of our kinds,
    /// alongside the base images they were built from.
    fn a_rack() -> Vec<ImageFacts> {
        vec![
            ours("vw-vivado-20260101000000", 1),
            ours("vw-vivado-20260601000000", 2),
            ours("vw-vivado-20260806000000", 3),
            ours("vw-helios-20260101000000", 1),
            ours("vw-helios-20260806000000", 3),
            ours("vw-artifact-20260806000000", 3),
            silo("ubuntu-24-04-20260108", 0),
            silo("helios-3-base-20260509", 0),
        ]
    }

    /// The guarantee: whatever else happens, the image an environment created
    /// a moment from now would boot is still there afterwards.
    #[test]
    fn the_newest_of_each_kind_is_never_deleted() {
        let images = a_rack();
        let plan = plan_recycle(&images, &BTreeMap::new());

        for kind in InstanceKind::ALL {
            let newest = choose_image(&images, kind, None)
                .expect("this rack has one of each kind");
            assert!(
                !plan.delete.iter().any(|i| i.id == newest.id),
                "{} would have been deleted",
                newest.name,
            );
            assert!(
                plan.keep.iter().any(|i| i.id == newest.id && i.latest),
                "{} is not reported as the newest",
                newest.name,
            );
        }
    }

    /// The other guarantee, and the one an old environment depends on: age is
    /// no reason to delete an image somebody is still booting.
    #[test]
    fn an_image_an_environment_boots_is_never_deleted() {
        let images = a_rack();
        // The oldest vivado image there is, with two newer ones above it.
        let ancient = images[0].id;
        let in_use = BTreeMap::from([(
            ancient,
            vec!["rcgoodfellow/tenagra".to_owned()],
        )]);

        let plan = plan_recycle(&images, &in_use);

        assert!(!plan.delete.iter().any(|i| i.id == ancient));
        let kept = plan
            .keep
            .iter()
            .find(|i| i.id == ancient)
            .expect("the image in use is kept");
        assert!(!kept.latest, "it is not the newest — being in use is why");
        assert_eq!(kept.used_by, ["rcgoodfellow/tenagra"]);
    }

    /// Nothing outside the service's own naming is a candidate, whether or
    /// not anything is using it. The base images the rack publishes are the
    /// case that matters: deleting one costs an upload nobody planned for.
    #[test]
    fn an_image_that_is_not_ours_is_never_touched() {
        let images = a_rack();
        let plan = plan_recycle(&images, &BTreeMap::new());

        let mentioned: Vec<&str> = names(&plan.delete)
            .into_iter()
            .chain(kept_names(&plan.keep))
            .collect();
        for base in ["ubuntu-24-04-20260108", "helios-3-base-20260509"] {
            assert!(
                !mentioned.contains(&base),
                "{base} should not even be considered",
            );
        }
    }

    /// Named like ours but published to the silo: shared with everyone, and
    /// who else boots it is not a question answerable from here.
    #[test]
    fn a_silo_image_of_ours_is_never_deleted() {
        let images = vec![
            ours("vw-vivado-20260806000000", 3),
            silo("vw-vivado-20260101000000", 1),
        ];
        let plan = plan_recycle(&images, &BTreeMap::new());

        assert!(plan.delete.is_empty(), "{:?}", names(&plan.delete));
    }

    /// What the command is for.
    #[test]
    fn older_unused_images_of_ours_are_deleted() {
        let plan = plan_recycle(&a_rack(), &BTreeMap::new());

        assert_eq!(
            names(&plan.delete),
            [
                "vw-vivado-20260101000000",
                "vw-vivado-20260601000000",
                "vw-helios-20260101000000",
            ],
        );
    }

    /// A kind with a single image has that image as its newest, so a pass
    /// over a service that has only ever built once deletes nothing.
    #[test]
    fn a_kind_with_one_image_keeps_it() {
        let images = vec![ours("vw-artifact-20260806000000", 3)];
        let plan = plan_recycle(&images, &BTreeMap::new());

        assert!(plan.delete.is_empty());
        assert_eq!(kept_names(&plan.keep), ["vw-artifact-20260806000000"]);
    }

    /// Two images built in the same second — which the image names alone
    /// cannot tell apart — must still leave exactly one of them protected,
    /// and it must be the one an environment would boot.
    #[test]
    fn a_tie_on_creation_time_still_protects_what_would_boot() {
        let images = vec![
            ours("vw-helios-20260806220915", 7),
            ours("vw-helios-20260806220915", 7),
        ];
        let would_boot = choose_image(&images, InstanceKind::Helios, None)
            .expect("one of them wins");

        let plan = plan_recycle(&images, &BTreeMap::new());

        assert_eq!(plan.delete.len(), 1, "the other one goes");
        assert!(!plan.delete.iter().any(|i| i.id == would_boot.id));
    }
}
