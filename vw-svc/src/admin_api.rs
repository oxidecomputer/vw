//! This module implements the admin api trait `[vw_api::VwAdminApi]`
//!
//! Everything here reaches across users, which is the whole reason it is a
//! separate API on a separate port: the user API can only ever see the caller's
//! own environments, and that property is easier to keep when the endpoints
//! that break it do not sit beside it.

use crate::{auth, db, Context, ServerArgs};
use dropshot::{ApiDescription, BuildError, ConfigDropshot};
use slog::{info, o};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Notify;
use vw_api::VwAdminApi;
use vw_api_types_versions::latest::UserEnvironmentPathParam;

pub struct AdminApi {}
impl VwAdminApi for AdminApi {
    type Context = Arc<Context>;

    async fn get_environments(
        rqctx: dropshot::RequestContext<Self::Context>,
    ) -> Result<
        dropshot::HttpResponseOk<
            dropshot::ResultsPage<
                vw_api_types_versions::latest::UserEnvironment,
            >,
        >,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let caller = auth::authorize_administrator(rqctx).await?;

        let environments = db::list_all_environments().inspect_err(|e| {
            slog::error!(log, "cannot list environments";
                slog_error_chain::InlineErrorChain::new(e),
            );
        })?;

        info!(log, "listed every environment";
            "administrator" => &caller.name,
            "environments" => environments.len(),
        );

        // One complete page: this endpoint takes no pagination parameters, and
        // the number of environments on a rack is bounded by the number of
        // developers using it. If that ever stops being true this becomes a
        // `ResultsPage::new` with a selector keyed on user and name.
        Ok(dropshot::HttpResponseOk(dropshot::ResultsPage {
            next_page: None,
            items: environments,
        }))
    }

    async fn delete_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::UserEnvironmentPathParam,
        >,
    ) -> Result<dropshot::HttpResponseDeleted, dropshot::HttpError> {
        let log = rqctx.log.clone();
        let reconcile = rqctx.context().reconcile.clone();
        let caller = auth::authorize_administrator(rqctx).await?;
        let key: UserEnvironmentPathParam = path_params.into_inner();

        // Named in the log before it happens as well as after: this is one
        // person removing another person's work, and the record of who did it
        // should survive whatever the deletion does next.
        info!(log, "deleting an environment on behalf of the service";
            "administrator" => &caller.name,
            "user" => &key.user,
            "environment" => &key.name,
        );

        db::delete_environment(key.clone()).inspect_err(|e| {
            slog::error!(log, "cannot delete an environment";
                "user" => &key.user,
                "environment" => &key.name,
                slog_error_chain::InlineErrorChain::new(e),
            );
        })?;

        // Tear the instances down now rather than on the next tick, so a rack
        // an administrator is reclaiming starts emptying immediately.
        reconcile.notify_one();

        Ok(dropshot::HttpResponseDeleted())
    }

    async fn recycle_images(
        rqctx: dropshot::RequestContext<Self::Context>,
        query: dropshot::Query<
            vw_api_types_versions::latest::ImageRecycleQuery,
        >,
    ) -> Result<
        dropshot::HttpResponseOk<
            vw_api_types_versions::latest::ImageRecycleReport,
        >,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let dry_run = query.into_inner().dry_run;
        let caller = auth::authorize_administrator(rqctx).await?;

        // Read before anything is deleted, and the deciding input: an image
        // any environment names is spared no matter how old it is. Failing to
        // read them has to fail the whole pass rather than proceed with an
        // empty set, which would look exactly like "nothing is in use".
        let environments = db::list_all_environments().inspect_err(|e| {
            slog::error!(log, "cannot list environments to recycle images";
                slog_error_chain::InlineErrorChain::new(e),
            );
        })?;
        let in_use = images_in_use(&environments);

        // Answered before the session is opened so the caller gets a reason
        // rather than the blank 500 dropshot turns an internal error into.
        // Nothing secret about it, and it is the one thing that would explain
        // why a command that reclaims images found none to reclaim.
        if !crate::oxide::is_configured() {
            slog::warn!(log, "cannot recycle images without an oxide backend";
                "administrator" => &caller.name,
            );
            let message = String::from(
                "this service has no oxide backend configured, so it has no \
                 images to recycle",
            );
            return Err(dropshot::HttpError {
                status_code: dropshot::ErrorStatusCode::SERVICE_UNAVAILABLE,
                error_code: None,
                external_message: message.clone(),
                internal_message: message,
                headers: None,
            });
        }
        let session = crate::oxide::session()?;

        info!(log, "recycling images";
            "administrator" => &caller.name,
            "dry_run" => dry_run,
            "images_in_use" => in_use.len(),
        );

        let plan = session
            .recycle_images(&in_use, dry_run, &log)
            .await
            .inspect_err(|e| {
                slog::error!(log, "cannot recycle images";
                    "administrator" => &caller.name,
                    slog_error_chain::InlineErrorChain::new(e),
                );
            })?;

        info!(log, "recycled images";
            "administrator" => &caller.name,
            "dry_run" => dry_run,
            "deleted" => plan.delete.len(),
            "kept" => plan.keep.len(),
        );

        Ok(dropshot::HttpResponseOk(
            vw_api_types_versions::latest::ImageRecycleReport {
                deleted: plan.delete,
                kept: plan.keep,
                dry_run,
            },
        ))
    }
}

/// Which images the service's environments are booting, as image id to the
/// environments naming it.
///
/// An environment pins its images by id when it is created, so this is an
/// exact answer rather than a match on names — an image that was rebuilt
/// under the same name is a different image, and the one actually booted is
/// the one that must survive.
///
/// An environment with no images is one created while the service had no
/// Oxide backend. It is a bare record that never booted anything and so
/// protects nothing.
fn images_in_use(
    environments: &[vw_api_types_versions::latest::UserEnvironment],
) -> std::collections::BTreeMap<uuid::Uuid, Vec<String>> {
    let mut in_use: std::collections::BTreeMap<uuid::Uuid, Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in environments {
        let Some(images) = entry.environment.images.as_ref() else {
            continue;
        };
        let owner = format!("{}/{}", entry.user, entry.environment.name);
        for image in [&images.vivado, &images.helios, &images.artifact] {
            let users = in_use.entry(image.id).or_default();
            // An environment's three instances can share one image, and
            // naming the same environment three times reads as three
            // environments depending on it.
            if !users.contains(&owner) {
                users.push(owner.clone());
            }
        }
    }
    in_use
}

#[derive(Debug, thiserror::Error)]
pub enum StartServerError {
    #[error("Server build error {0}")]
    ServerBuildError(#[from] BuildError),
    #[error("Unexpected server exit {0}")]
    ServerExit(String),
}

pub async fn start_server(
    server_args: ServerArgs,
    log: slog::Logger,
    bind_address: SocketAddr,
    tls: Option<crate::tls::Tls>,
    reconcile: Arc<Notify>,
) -> Result<(), StartServerError> {
    let scheme = crate::tls::scheme(&server_args);
    let context = Arc::new(Context {
        server_args,
        reconcile,
    });
    let cfg = ConfigDropshot {
        bind_address,
        default_request_body_max_bytes: usize::MAX,
        ..Default::default()
    };
    let lg = log.new(o!("component" => "admin_api"));
    let api = api_description();

    // This API has endpoints that exist only from a given version onwards, so
    // each request has to say which version it is written against — dropshot
    // will not start a server that mixes versioned endpoints with no way to
    // pick between them. No default for a missing header: every client is
    // built from this repository and sends one, and answering a request that
    // did not say what it expects is how a client silently gets a version it
    // was not written for.
    let versions = dropshot::VersionPolicy::Dynamic(Box::new(
        dropshot::ClientSpecifiesVersionInHeader::new(
            http::header::HeaderName::from_static(vw_api::API_VERSION_HEADER),
            vw_api::latest_version(),
        ),
    ));

    // Shared rather than owned so that the certificate can be replaced under a
    // server that is already running. Dropping the last handle shuts the
    // server down, so this one outlives the follower task below.
    let server = Arc::new(
        dropshot::ServerBuilder::new(api, context, lg.clone())
            .config(cfg)
            .version_policy(versions)
            .tls(crate::tls::initial(tls.as_ref()))
            .start()?,
    );

    info!(lg, "listening on {scheme}://{}", server.local_addr());

    crate::tls::follow_renewals(server.clone(), tls, lg.clone());

    server
        .wait_for_shutdown()
        .await
        .map_err(StartServerError::ServerExit)
}

pub fn api_description() -> ApiDescription<Arc<Context>> {
    vw_api::vw_admin_api_mod::api_description::<AdminApi>().unwrap()
}

#[cfg(test)]
mod in_use_test {
    use super::*;
    use uuid::Uuid;
    use vw_api_types_versions::latest::{
        Environment, EnvironmentImages, ImageRef, UserEnvironment,
    };

    fn image(id: Uuid) -> ImageRef {
        ImageRef {
            id,
            name: format!("vw-image-{id}"),
        }
    }

    fn environment(
        user: &str,
        name: &str,
        images: Option<[Uuid; 3]>,
    ) -> UserEnvironment {
        UserEnvironment {
            user: user.to_owned(),
            environment: Environment {
                name: name.to_owned(),
                images: images.map(|[vivado, helios, artifact]| {
                    EnvironmentImages {
                        vivado: image(vivado),
                        helios: image(helios),
                        artifact: image(artifact),
                    }
                }),
                vivado_instance: None,
                helios_instance: None,
                artifact_instance: None,
            },
        }
    }

    #[test]
    fn every_image_an_environment_pins_is_in_use() {
        let [a, b, c] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let in_use = images_in_use(&[environment(
            "rcgoodfellow",
            "tenagra",
            Some([a, b, c]),
        )]);

        assert_eq!(in_use.len(), 3);
        for id in [a, b, c] {
            assert_eq!(in_use[&id], ["rcgoodfellow/tenagra"]);
        }
    }

    /// Everyone booting an image is named, because the report says why an
    /// image survived and one environment is not the whole reason.
    #[test]
    fn an_image_two_environments_boot_names_both() {
        let shared = Uuid::new_v4();
        let in_use = images_in_use(&[
            environment(
                "rcgoodfellow",
                "tenagra",
                Some([shared, shared, shared]),
            ),
            environment(
                "hellokayt",
                "katie",
                Some([shared, Uuid::new_v4(), Uuid::new_v4()]),
            ),
        ]);

        assert_eq!(
            in_use[&shared],
            ["rcgoodfellow/tenagra", "hellokayt/katie"]
        );
    }

    /// An environment whose three instances share one image depends on it
    /// once, not three times.
    #[test]
    fn an_environment_using_one_image_throughout_is_named_once() {
        let only = Uuid::new_v4();
        let in_use = images_in_use(&[environment(
            "rcgoodfellow",
            "tenagra",
            Some([only; 3]),
        )]);

        assert_eq!(in_use[&only], ["rcgoodfellow/tenagra"]);
    }

    /// A record created while the service had no Oxide backend never booted
    /// anything, so it protects nothing.
    #[test]
    fn an_environment_without_images_protects_nothing() {
        let in_use =
            images_in_use(&[environment("rcgoodfellow", "bare", None)]);

        assert!(in_use.is_empty());
    }
}
