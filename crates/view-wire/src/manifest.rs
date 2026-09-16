//! Static metadata for an Ice view. Parsing never instantiates or runs a guest.

pub const MANIFEST_SECTION: &str = "ice.manifest";

/// A finite positive logical size, bounded like wire geometry. Private bits
/// keep equality and hashing exact without admitting NaN or signed zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PreferredSize([u32; 2]);

impl PreferredSize {
    pub fn new(width: f32, height: f32) -> Option<Self> {
        [width, height]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0 && *value <= 8192.0)
            .then_some(Self([width.to_bits(), height.to_bits()]))
    }

    pub fn dimensions(self) -> [f32; 2] {
        [f32::from_bits(self.0[0]), f32::from_bits(self.0[1])]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub wire_epoch: u32,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub preferred_size: Option<PreferredSize>,
}

/// A valid manifest requests a payload protocol this host does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolMismatch {
    pub guest: u32,
    pub host: u32,
}

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wire epoch guest {}, host {}", self.guest, self.host)
    }
}

impl std::error::Error for ProtocolMismatch {}

/// What a manifest may say about itself. The catalog is read before anything
/// is installed, and the store shapes every field of every entry on every
/// relayout — outside the sandbox, with no fuel and no memory limit — so a
/// module whose manifest is a megabyte of capability names is left out of the
/// catalog rather than laid out.
const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 256;
const MAX_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_BYTES: usize = 32;

/// Extracts exactly one current manifest from a component, including nested modules.
/// Returns `None` for missing, duplicate, malformed, or out-of-bounds metadata.
/// This parses the binary structure; it does not validate the component ABI or execute it.
#[cfg(feature = "manifest")]
pub fn read_manifest(bytes: &[u8]) -> Option<Manifest> {
    read_manifest_with(bytes, Manifest::parse)
}

#[cfg(feature = "manifest")]
pub(crate) fn read_manifest_with(
    bytes: &[u8],
    parse: impl Fn(&str) -> Option<Manifest>,
) -> Option<Manifest> {
    // The parser walks into the core modules a component nests, which is
    // where the app's own sections are.
    let mut payloads = wasmparser::Parser::new(0).parse_all(bytes);
    // A bare core module — an app built but not yet componentized — is not
    // something the host can instantiate, so it is not in the catalog.
    let Some(Ok(wasmparser::Payload::Version {
        encoding: wasmparser::Encoding::Component,
        ..
    })) = payloads.next()
    else {
        return None;
    };
    let mut manifest = None;
    for payload in payloads {
        if let wasmparser::Payload::CustomSection(section) = payload.ok()?
            && section.name() == MANIFEST_SECTION
        {
            if manifest.is_some() {
                return None;
            }
            manifest = Some(parse(std::str::from_utf8(section.data()).ok()?)?);
        }
    }
    manifest
}

impl Manifest {
    /// Parses the strict six-line `ice.manifest.v2` text and its bounds.
    pub fn parse(text: &str) -> Option<Self> {
        Self::parse_with_header(text, "ice.manifest.v2")
    }

    pub(crate) fn parse_with_header(text: &str, header: &str) -> Option<Self> {
        if text.len() > 1024 || text.chars().any(|c| c.is_control() && c != '\n') {
            return None;
        }
        let mut lines = text.split('\n');
        if lines.next()? != header {
            return None;
        }
        let name = lines.next()?.to_owned();
        let description = lines.next()?.to_owned();
        let caps = lines.next()?;
        let capabilities = if caps.is_empty() {
            Vec::new()
        } else {
            let caps = caps.strip_suffix(',')?;
            if caps.split(',').any(str::is_empty) {
                return None;
            }
            caps.split(',').map(str::to_owned).collect()
        };
        let preferred_size = match lines.next()? {
            "none" => None,
            value => {
                let (width, height) = value.split_once(',')?;
                Some(PreferredSize::new(
                    width.parse().ok()?,
                    height.parse().ok()?,
                )?)
            }
        };
        let epoch = lines.next()?;
        let wire_epoch = epoch.parse::<u32>().ok()?;
        if wire_epoch == 0 || wire_epoch.to_string() != epoch {
            return None;
        }
        if lines.next().is_some() {
            return None;
        }
        let manifest = Self {
            wire_epoch,
            name,
            description,
            capabilities,
            preferred_size,
        };
        manifest.within_bounds().then_some(manifest)
    }

    /// Reject a different payload protocol before executing or restoring a guest.
    pub fn check_wire_protocol(&self) -> Result<(), ProtocolMismatch> {
        if self.wire_epoch == crate::WIRE_EPOCH {
            Ok(())
        } else {
            Err(ProtocolMismatch {
                guest: self.wire_epoch,
                host: crate::WIRE_EPOCH,
            })
        }
    }

    fn within_bounds(&self) -> bool {
        !self.name.is_empty()
            && self.name.len() <= MAX_NAME_BYTES
            && self.description.len() <= MAX_DESCRIPTION_BYTES
            && self.capabilities.len() <= MAX_CAPABILITIES
            && self
                .capabilities
                .iter()
                .all(|capability| capability.len() <= MAX_CAPABILITY_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_requires_an_explicit_canonical_wire_epoch() {
        let current = "ice.manifest.v2\nSized\nDescription\nclock,\nnone\n1";
        assert!(
            Manifest::parse(current).is_some(),
            "current epoch manifest rejected"
        );
        assert!(Manifest::parse("ice.manifest.v1\nSized\nDescription\nclock,\nnone").is_none());
        for epoch in ["", "0", "01", "+1", "-1", " 1", "1 ", "4294967296"] {
            assert!(
                Manifest::parse(&format!("ice.manifest.v2\nSized\n\n\nnone\n{epoch}")).is_none(),
                "accepted {epoch:?}"
            );
        }
        assert!(
            Manifest::parse("ice.manifest.v2\nSized\n\n\nnone\n2").is_some(),
            "unsupported is distinct from malformed"
        );
    }

    #[test]
    fn wire_protocol_mismatch_reports_both_epochs() {
        let current = Manifest::parse(&format!(
            "ice.manifest.v2\nApp\n\n\nnone\n{}",
            crate::WIRE_EPOCH
        ))
        .unwrap();
        assert_eq!(current.check_wire_protocol(), Ok(()));
        let mut other = current;
        other.wire_epoch = crate::WIRE_EPOCH + 1;
        let mismatch = other.check_wire_protocol().unwrap_err();
        assert_eq!(
            mismatch,
            ProtocolMismatch {
                guest: crate::WIRE_EPOCH + 1,
                host: crate::WIRE_EPOCH
            }
        );
        assert_eq!(
            mismatch.to_string(),
            format!(
                "wire epoch guest {}, host {}",
                crate::WIRE_EPOCH + 1,
                crate::WIRE_EPOCH
            )
        );
    }

    #[cfg(feature = "manifest")]
    #[test]
    fn extraction_rejects_duplicate_and_truncated_sections() {
        let mut bytes = b"\0asm\x0d\0\x01\0".to_vec();
        let text = b"ice.manifest.v2\nSized\n\n\n640.5,480.25\n1";
        let mut section = vec![
            0,
            (1 + MANIFEST_SECTION.len() + text.len()) as u8,
            MANIFEST_SECTION.len() as u8,
        ];
        section.extend_from_slice(MANIFEST_SECTION.as_bytes());
        section.extend_from_slice(text);
        bytes.extend_from_slice(&section);
        assert_eq!(
            read_manifest(&bytes)
                .unwrap()
                .preferred_size
                .unwrap()
                .dimensions(),
            [640.5, 480.25]
        );
        let mut duplicate = bytes.clone();
        duplicate.extend_from_slice(&section);
        assert!(
            read_manifest(&duplicate).is_none(),
            "duplicate manifest accepted"
        );
        bytes.extend_from_slice(&[0, 127]);
        assert!(
            read_manifest(&bytes).is_none(),
            "truncated section accepted"
        );
        assert!(
            read_manifest(b"\0asm\x01\0\0\0").is_none(),
            "core module accepted"
        );
    }

    // Claim: untrusted module metadata has one strict current format and a
    // finite positive bounded preferred size. Dropping those guards is Red.
    #[test]
    fn manifest_format_and_preferred_size_are_strict() {
        let good = "ice.manifest.v2\nSized\nDescription\nclock,storage,\n640.5,480.25\n1";
        let parsed = Manifest::parse(good).unwrap();
        assert_eq!(parsed.preferred_size.unwrap().dimensions(), [640.5, 480.25]);
        assert_eq!(parsed.capabilities, ["clock", "storage"]);
        assert!(
            Manifest::parse("ice.manifest.v2\nDefault\n\n\nnone\n1")
                .unwrap()
                .preferred_size
                .is_none()
        );
        for invalid in [
            "Sized\nDescription\nclock,", // no legacy format
            "ice.manifest.v2\nSized\nDescription\n\nnone",
            "ice.manifest.v2\nSized\nDescription\n\nnone\nextra\n1",
            "ice.manifest.v2\nSized\nDescription\nclock\nnone\n1",
            "ice.manifest.v2\nSized\nDescription\nclock,,\nnone\n1",
        ] {
            assert!(
                Manifest::parse(invalid).is_none(),
                "accepted malformed manifest: {invalid}"
            );
        }
        for invalid in [
            "NaN,500",
            "inf,500",
            "-inf,500",
            "0,500",
            "-0,500",
            "-1,500",
            "8192.01,500",
            "1e40,500",
            "1e-50,500",
            "500,0",
            "1,2,3",
        ] {
            assert!(
                Manifest::parse(&format!(
                    "ice.manifest.v2\nSized\nDescription\n\n{invalid}\n1"
                ))
                .is_none(),
                "accepted {invalid}"
            );
        }
        assert!(PreferredSize::new(8192.0, f32::MIN_POSITIVE).is_some());
        assert!(PreferredSize::new(f32::from_bits(1), 1.0).is_some());
        use std::hash::{Hash, Hasher};
        let value = PreferredSize::new(640.5, 480.25).unwrap();
        let mut hashes = [
            std::collections::hash_map::DefaultHasher::new(),
            std::collections::hash_map::DefaultHasher::new(),
        ];
        value.hash(&mut hashes[0]);
        parsed.preferred_size.unwrap().hash(&mut hashes[1]);
        assert_eq!(hashes[0].finish(), hashes[1].finish());
    }
}
