use anyhow::{Context, Result};

use ostree_ext::container as ostree_container;
use ostree_ext::oci_spec;
use ostree_ext::oci_spec::image::ImageConfiguration;
use ostree_ext::ostree;
use ostree_ext::sysroot::SysrootLock;

use super::CachedImageStatus;
use crate::spec::{ImageReference, ImageStatus};

pub(super) struct OstreeContainerStore;

impl super::ContainerImageStoreImpl for OstreeContainerStore {
    fn spec(&self) -> crate::spec::Store {
        crate::spec::Store::OstreeContainer
    }

    fn imagestatus(
        &self,
        sysroot: &SysrootLock,
        deployment: &ostree::Deployment,
        image: ostree_container::OstreeImageReference,
    ) -> Result<CachedImageStatus> {
        let repo = &sysroot.repo();
        let image = ImageReference::from(image);
        let csum = deployment.csum();
        let imgstate = ostree_container::store::query_image_commit(repo, &csum)?;
        let cached = imgstate.cached_update.map(|cached| {
            create_imagestatus(image.clone(), &cached.manifest_digest, &cached.config)
        });
        let imagestatus =
            create_imagestatus(image, &imgstate.manifest_digest, &imgstate.configuration);

        Ok(CachedImageStatus {
            image: Some(imagestatus),
            cached_update: cached,
        })
    }
}

/// Convert between a subset of ostree-ext metadata and the exposed spec API.
fn create_imagestatus(
    image: ImageReference,
    manifest_digest: &str,
    config: &ImageConfiguration,
) -> ImageStatus {
    let labels = labels_of_config(config);
    let timestamp = labels
        .and_then(|l| {
            l.get(oci_spec::image::ANNOTATION_CREATED)
                .map(|s| s.as_str())
        })
        .and_then(try_deserialize_timestamp);

    let version = ostree_container::version_for_config(config).map(ToOwned::to_owned);
    ImageStatus {
        image,
        version,
        timestamp,
        image_digest: manifest_digest.to_owned(),
    }
}

fn labels_of_config(
    config: &oci_spec::image::ImageConfiguration,
) -> Option<&std::collections::HashMap<String, String>> {
    config.config().as_ref().and_then(|c| c.labels().as_ref())
}

fn try_deserialize_timestamp(t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    match chrono::DateTime::parse_from_rfc3339(t).context("Parsing timestamp") {
        Ok(t) => Some(t.into()),
        Err(e) => {
            tracing::warn!("Invalid timestamp in image: {:#}", e);
            None
        }
    }
}
