//! Canonical `.duck` account labels and hostname parsing. Consensus values are
//! strict; lookup is DNS-like and case-normalizes before producing them.

use crate::{DUCKDNS_ZONE, DuckDnsName, MAX_LABEL_LEN, RESERVED_ROOT_LABELS};

/// What a handle IS: lowercase ASCII `[a-z0-9-]`, no leading/trailing hyphen,
/// at most 63 bytes. This shape-only validator is exported for other label
/// domains; DuckDNS state itself uses [`validate_handle`].
pub fn validate_handle_shape(handle: &str) -> Result<(), String> {
    if handle.is_empty() {
        return Err("account handle must be non-empty".into());
    }
    if handle.len() > MAX_LABEL_LEN {
        return Err(format!(
            "account handle exceeds {MAX_LABEL_LEN} bytes: {} bytes",
            handle.len()
        ));
    }
    if handle.starts_with('-') || handle.ends_with('-') {
        return Err(format!(
            "account handle must not start or end with a hyphen: {handle:?}"
        ));
    }
    if !handle
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "account handle has invalid characters (want lowercase [a-z0-9-]): {handle:?}"
        ));
    }
    Ok(())
}

/// The DuckDNS handle rule: shape plus the reserved-root-label policy.
pub fn validate_handle(handle: &str) -> Result<(), String> {
    validate_handle_shape(handle)?;
    if RESERVED_ROOT_LABELS.contains(&handle) {
        return Err(format!(
            "account handle {handle:?} is reserved for a root namespace"
        ));
    }
    Ok(())
}

impl DuckDnsName {
    /// Validate a value decoded directly from the wire.
    pub fn validate(&self) -> Result<(), String> {
        validate_handle(&self.handle)
    }

    pub fn hostname(&self) -> String {
        format!("{}.{DUCKDNS_ZONE}", self.handle)
    }
}

/// Parse one account hostname, accepting DNS case-insensitivity and one
/// terminal dot. Multi-label service names are deliberately unsupported.
pub fn parse_hostname(hostname: &str) -> Result<DuckDnsName, String> {
    if hostname.trim() != hostname {
        return Err("hostname must not have surrounding whitespace".into());
    }
    let without_dot = hostname.strip_suffix('.').unwrap_or(hostname);
    if without_dot.is_empty() || without_dot.ends_with('.') {
        return Err("hostname has an empty label".into());
    }
    if !without_dot.is_ascii() {
        return Err("hostname must be ASCII".into());
    }
    let canonical = without_dot.to_ascii_lowercase();
    let suffix = format!(".{DUCKDNS_ZONE}");
    let handle = canonical
        .strip_suffix(&suffix)
        .ok_or_else(|| format!("hostname is outside {DUCKDNS_ZONE}: {hostname:?}"))?;
    if handle.contains('.') {
        return Err(format!(
            "only direct account names are supported: {hostname:?}"
        ));
    }
    let name = DuckDnsName {
        handle: handle.to_owned(),
    };
    name.validate()?;
    Ok(name)
}
