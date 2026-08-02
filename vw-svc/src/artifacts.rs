//! Reading an environment's artifacts back out of its object store.
//!
//! The store lives on the rack's internal network. A developer's machine has
//! no route to it — the artifact instance's external address is often only
//! reachable over a VPN, and needing one to collect the output of a build
//! would defeat the point of building remotely. So this service, which is on
//! both networks, does the reading and passes the bytes through.

use vw_api_types_versions::latest::{Artifact, S3Credentials, TargetKind};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ArtifactError {
    #[error("the environment has no object store yet")]
    NoStore,
    #[error("talking to the object store")]
    Store(#[source] s3::error::S3Error),
    #[error("the object store answered {0}")]
    Refused(u16),
    #[error("no artifact called '{0}'")]
    NoSuchArtifact(String),
}

/// A handle on one of an environment's buckets.
pub(crate) fn bucket(
    credentials: &S3Credentials,
) -> Result<Box<s3::Bucket>, ArtifactError> {
    let region = s3::Region::Custom {
        region: credentials.region.clone(),
        endpoint: credentials.endpoint.clone(),
    };
    let creds = s3::creds::Credentials::new(
        Some(&credentials.access_key_id),
        Some(&credentials.secret_access_key),
        None,
        None,
        None,
    )
    .map_err(|_| ArtifactError::NoStore)?;

    // Path style: the bucket is reached by address, and there is no DNS inside
    // the VPC that would resolve a bucket-as-subdomain name.
    Ok(s3::Bucket::new(&credentials.bucket, region, creds)
        .map_err(ArtifactError::Store)?
        .with_path_style())
}

/// Everything in one bucket.
pub(crate) async fn list(
    credentials: &S3Credentials,
    kind: TargetKind,
) -> Result<Vec<Artifact>, ArtifactError> {
    let bucket = bucket(credentials)?;
    let pages = bucket
        .list(String::new(), None)
        .await
        .map_err(ArtifactError::Store)?;

    Ok(pages
        .into_iter()
        .flat_map(|page| page.contents)
        .map(|object| Artifact {
            kind,
            name: object.key,
            size: object.size,
            modified: Some(object.last_modified),
        })
        .collect())
}

/// One artifact's bytes, as a stream.
///
/// Not read into memory first: an image runs to hundreds of megabytes, and
/// this service should not have to hold one to hand it on.
///
/// The store's stream is pumped into a small bounded channel rather than
/// handed out directly. That is partly necessity — the response body has to be
/// shareable across threads and the store's stream is not — and partly the
/// better behaviour: a bounded channel means a developer on a slow connection
/// slows the read from the store rather than making this service buffer an
/// entire image on their behalf.
pub(crate) async fn fetch(
    credentials: &S3Credentials,
    artifact: &str,
) -> Result<
    impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>>,
    ArtifactError,
> {
    use futures::StreamExt;

    let bucket = bucket(credentials)?;
    let response = bucket
        .get_object_stream(format!("/{artifact}"))
        .await
        .map_err(|e| match e {
            s3::error::S3Error::HttpFailWithBody(404, _) => {
                ArtifactError::NoSuchArtifact(artifact.to_owned())
            }
            other => ArtifactError::Store(other),
        })?;

    if response.status_code >= 300 {
        return Err(if response.status_code == 404 {
            ArtifactError::NoSuchArtifact(artifact.to_owned())
        } else {
            ArtifactError::Refused(response.status_code)
        });
    }

    let (chunks, receive) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut bytes = response.bytes;
        while let Some(chunk) = bytes.next().await {
            // The store's own error type does not leave this module; what the
            // body needs is an io error, and what a developer needs is the
            // text.
            let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()));
            let failed = chunk.is_err();
            if chunks.send(chunk).await.is_err() {
                // The developer stopped reading. So do we — there is no point
                // pulling the rest of an image nobody is collecting.
                break;
            }
            if failed {
                break;
            }
        }
    });

    Ok(tokio_stream::wrappers::ReceiverStream::new(receive))
}

impl From<ArtifactError> for dropshot::HttpError {
    fn from(value: ArtifactError) -> Self {
        let message = value.to_string();
        match value {
            ArtifactError::NoSuchArtifact(_) => {
                dropshot::HttpError::for_not_found(None, message)
            }
            ArtifactError::NoStore => dropshot::HttpError::for_unavail(
                None,
                String::from(
                    "this environment has no object store yet; its artifact \
                     instance may still be coming up",
                ),
            ),
            _ => dropshot::HttpError::for_internal_error(message),
        }
    }
}
