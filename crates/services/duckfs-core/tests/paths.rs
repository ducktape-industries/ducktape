//! task 4: pure path canonicalization + write authority. no harness — this
//! test imports only `duckfs_core::paths` so it runs under `duckfs-core`
//! too (the paths module is part of the always-compiled pure core).

use duckfs_core::paths::{canonical, check_authority};

#[test]
fn canonical_and_authority_table() {
    // ---- canonical: happy path splits into segments ----
    assert_eq!(canonical("/shared/a.txt").unwrap(), vec!["shared", "a.txt"]);
    // a bare root canonicalizes to no segments (read queries like Ls use it)
    assert_eq!(canonical("/").unwrap(), Vec::<String>::new());

    // ---- canonical: must be absolute ----
    assert!(canonical("shared/x").unwrap_err().contains("absolute"));

    // ---- canonical: empty / dot / dotdot segments ----
    assert!(canonical("/shared//x").unwrap_err().contains("segment"));
    assert!(canonical("/shared/./x").unwrap_err().contains("segment"));
    assert!(canonical("/shared/../x").unwrap_err().contains("segment"));

    // ---- canonical: NFC both ways ----
    // NFD "é" (e + combining acute) is rejected; NFC "é" (U+00E9) is accepted
    // and preserved verbatim (canonicalization rejects, it never rewrites).
    assert!(
        canonical("/shared/cafe\u{0301}")
            .unwrap_err()
            .contains("NFC")
    );
    assert_eq!(
        canonical("/shared/caf\u{00e9}").unwrap(),
        vec!["shared", "caf\u{00e9}"]
    );

    // ---- canonical: NUL guard (a segment can never carry '/' after split) ----
    assert!(canonical("/shared/a\0b").is_err());

    // ---- canonical: per-segment name byte cap ----
    // ASCII: 256 bytes > 255 rejects; 255 bytes is the inclusive limit.
    assert!(
        canonical(&format!("/shared/{}", "x".repeat(256)))
            .unwrap_err()
            .contains("name")
    );
    assert!(canonical(&format!("/shared/{}", "x".repeat(255))).is_ok());
    // the cap is on BYTES, not chars: 128 * "é" = 128 chars but 256 UTF-8 bytes.
    assert!(
        canonical(&format!("/shared/{}", "\u{00e9}".repeat(128)))
            .unwrap_err()
            .contains("name")
    );

    // ---- canonical: total path byte cap ----
    // 20 * 250-byte names ≈ 5k bytes > 4096, but depth 20 and each name < 255,
    // so only the total-length cap can fire here.
    let long = format!("/{}", vec!["y".repeat(250); 20].join("/"));
    assert!(canonical(&long).is_err());

    // ---- canonical: depth cap (129 segments > 128) ----
    let deep = format!("/shared{}", "/d".repeat(128));
    assert!(canonical(&deep).unwrap_err().contains("depth"));

    // ---- authority ----
    let seg = |p: &str| canonical(p).unwrap();
    // home: the owner writes under their own root (any depth ≥ 1 below it).
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/home/ext:aa/x")
        )
        .is_ok()
    );
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/home/ext:aa/deep/nested/f")
        )
        .is_ok()
    );
    // home: a different actor writes under it too — the label is attribution.
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xbb],
                account: None
            },
            &seg("/home/ext:aa/x")
        )
        .is_ok()
    );
    // home: the home root itself (and bare /home) is never a writable file.
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/home/ext:aa")
        )
        .unwrap_err()
        .contains("home root")
    );
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/home")
        )
        .unwrap_err()
        .contains("home root")
    );
    // shared: any actor (here a plain module id) may write under it (≥ 2 segs).
    assert!(
        check_authority(
            &duckfs_core::Authority::Module("chat".into()),
            &seg("/shared/x")
        )
        .is_ok()
    );
    // shared: the /shared root itself is not a writable target.
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/shared")
        )
        .is_err()
    );
    // outside /home and /shared: rejected for a normal actor.
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/etc/passwd")
        )
        .unwrap_err()
        .contains("outside")
    );
    // the root path is outside for a normal actor (only system may touch it).
    assert!(
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None
            },
            &seg("/")
        )
        .unwrap_err()
        .contains("outside")
    );
    // system bypasses authority entirely (but the path is still canonical).
    assert!(check_authority(&duckfs_core::Authority::System, &seg("/etc/passwd")).is_ok());
    assert!(check_authority(&duckfs_core::Authority::System, &seg("/")).is_ok());
}

/// hygiene: every rejection out of the pure path layer reads as one plain
/// sentence with no `<module>: ` prefix — these strings surface verbatim
/// through commit rejections, and the renderer prints the class token last,
/// in brackets, so a prefix inside the sentence only repeats it.
#[test]
fn path_errors_carry_no_module_prefix() {
    let seg = |p: &str| canonical(p).unwrap();
    for err in [
        canonical("shared/x").unwrap_err(),
        canonical("/shared//x").unwrap_err(),
        canonical("/shared/cafe\u{0301}").unwrap_err(),
        canonical("/shared/a\0b").unwrap_err(),
        canonical(&format!("/shared/{}", "x".repeat(256))).unwrap_err(),
        canonical(&format!("/shared{}", "/d".repeat(128))).unwrap_err(),
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            &seg("/home"),
        )
        .unwrap_err(),
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            &seg("/shared"),
        )
        .unwrap_err(),
        check_authority(
            &duckfs_core::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            &seg("/etc/passwd"),
        )
        .unwrap_err(),
    ] {
        assert!(
            err.starts_with(|c: char| c.is_ascii_lowercase()),
            "sentence must open on a lowercase word: {err}"
        );
        assert!(
            !err.split(' ').next().unwrap_or_default().ends_with(':'),
            "module-prefixed path error: {err}"
        );
    }
}
