use sdk::StateRoot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotionBoundarySource {
    Latest,
}

impl PromotionBoundarySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PromotionBoundarySource::Latest => "latest",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotionBoundary<'a> {
    Promote {
        boundary: &'a statesync::Manifest,
        source: PromotionBoundarySource,
    },
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestFetchRetry {
    pub(crate) log_line: String,
    pub(crate) announce: bool,
}

/// the line for a parked node's `failures`-th failed boundary fetch, or `None`
/// when this one stays silent: a sync that never starts must say why at
/// `warn`, but the loop retries forever — so the first failure speaks, then
/// every [`BOUNDARY_FETCH_WARN_EVERY`]th, and the count rides along as
/// `attempts`.
///
/// [`BOUNDARY_FETCH_WARN_EVERY`]: crate::constants::BOUNDARY_FETCH_WARN_EVERY
pub(crate) fn joiner_manifest_fetch_retry(
    label: &str,
    resident_standing: bool,
    failures: u64,
    error: impl std::fmt::Display,
) -> Option<ManifestFetchRetry> {
    let speaks =
        failures == 1 || failures.is_multiple_of(crate::constants::BOUNDARY_FETCH_WARN_EVERY);
    if !speaks {
        return None;
    }
    if resident_standing {
        return Some(ManifestFetchRetry {
            log_line: format!("[node {label}] resident: boundary fetch retrying ({error})"),
            announce: false,
        });
    }
    Some(ManifestFetchRetry {
        log_line: format!(
            "[node {label}] joining: redemption not landed yet (or the mesh is unreachable) — \
             the announce keeps retrying and a member node redeems it automatically. retrying \
             ({error})"
        ),
        announce: true,
    })
}

/// a boundary is promotable with its finalization floor on the wire, or bare
/// at (below) its own epoch base — the post-cutover window whose epoch has
/// finalized nothing yet, which is exactly the shape a serving node is
/// allowed to answer with (`validator::run::sync`) and the one this node may
/// itself be needed to finalize out of. Same rule as
/// [`crate::sync::serve::verify_manifest_floor`], which verifies the cert
/// when there is one.
pub(crate) fn latest_boundary_has_floor(latest: &statesync::Manifest) -> bool {
    latest.height <= latest.view_base || latest.floor_cert.is_some()
}

pub(crate) fn choose_promotion_boundary<'a>(
    synced_host_hash: StateRoot,
    latest: &'a statesync::Manifest,
    self_public_key: &[u8],
) -> PromotionBoundary<'a> {
    if !latest.participants.iter().any(|key| key == self_public_key) {
        return PromotionBoundary::Retry;
    }
    if latest.root_hash == synced_host_hash {
        return if latest_boundary_has_floor(latest) {
            PromotionBoundary::Promote {
                boundary: latest,
                source: PromotionBoundarySource::Latest,
            }
        } else {
            PromotionBoundary::Retry
        };
    }
    PromotionBoundary::Retry
}
