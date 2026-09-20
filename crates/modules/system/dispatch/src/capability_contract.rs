use std::collections::BTreeMap;

pub const MAX_TAG_LEN: usize = 64;
pub const MAX_RESOURCE_DIMS: usize = 16;

pub fn validate_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("capability tag must be non-empty".into());
    }
    if tag.len() > MAX_TAG_LEN {
        return Err(format!(
            "capability tag exceeds {MAX_TAG_LEN} bytes: {} bytes",
            tag.len()
        ));
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(format!(
            "capability tag has invalid characters (want [a-z0-9._-]): {tag:?}"
        ));
    }
    Ok(())
}

pub fn validate_resources(resources: &BTreeMap<String, u64>) -> Result<(), String> {
    if resources.len() > MAX_RESOURCE_DIMS {
        return Err(format!(
            "too many resource dimensions: {} exceeds the {MAX_RESOURCE_DIMS} cap",
            resources.len()
        ));
    }
    for (key, value) in resources {
        validate_tag(key).map_err(|e| format!("resource dimension {key:?}: {e}"))?;
        if *value == 0 {
            return Err(format!(
                "resource dimension {key:?} is zero (omit it instead)"
            ));
        }
    }
    Ok(())
}
