//! Only explicit test artifacts speak this protocol. Ordinary view/native ABI
//! does not include a test operation or application-state representation.
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    View(crate::native::Request),
    Begin {
        test: u32,
        fingerprint: u64,
        macos: bool,
    },
    ResolveTarget {
        test: u32,
        step: u32,
    },
    Step {
        test: u32,
        step: u32,
    },
}

/// The test world retains the normal view operations for mounted host rendering.
#[macro_export]
macro_rules! with_test_view_wit {
    ($callback:ident) => {
        $callback!(
            r#"package ice:view@0.1.0;
world view {
    import panicked: func(message: string);
    export init: func(macos: bool);
    export tick: func(events: list<u8>) -> list<u8>;
    export snapshot: func() -> result<list<u8>, string>;
    export restore: func(state: list<u8>, macos: bool) -> result<_, string>;
    export authored: func(command: list<u8>) -> result<list<u8>, string>;
}
"#
        );
    };
}

/// Explicit opt-in parser: the production parser rejects this artifact kind.
pub fn parse_manifest(text: &str) -> Option<crate::manifest::Manifest> {
    crate::manifest::Manifest::parse_with_header(text, "ice.test.manifest.v3")
}

#[cfg(feature = "manifest")]
pub fn read_manifest(bytes: &[u8]) -> Option<crate::manifest::Manifest> {
    crate::manifest::read_manifest_with(bytes, parse_manifest)
}

#[cfg(test)]
mod tests {
    #[test]
    fn previous_test_protocol_is_rejected_before_any_command() {
        let old = format!(
            "ice.test.manifest.v2\nCounter\n\n\nnone\n{}",
            crate::WIRE_EPOCH
        );
        assert!(super::parse_manifest(&old).is_none());
        assert!(
            super::parse_manifest(&old.replacen("test.manifest.v2", "test.manifest.v3", 1))
                .is_some()
        );
        assert!(
            crate::manifest::Manifest::parse(&old.replacen(
                "test.manifest.v2",
                "test.manifest.v3",
                1
            ))
            .is_none()
        );
    }
}
