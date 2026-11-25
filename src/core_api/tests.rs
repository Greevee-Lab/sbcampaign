// SPDX-License-Identifier: AGPL-3.0-only
//! Unit tests of the boundary types (no database).

use super::{IdentityError, Provider, STEELBOUND_XUID_PREFIX};

#[test]
fn provider_ids_round_trip() {
    for p in Provider::ALL {
        assert_eq!(Provider::from_id(p.id()), Some(p));
    }
    assert_eq!(Provider::from_id("brainbox"), None);
    assert_eq!(Provider::Steelbound.id(), "steelbound");
    assert_eq!(Provider::Insignia.id(), "insignia");
}

#[test]
fn steelbound_external_id_is_a_decimal_account_id() {
    let p = Provider::Steelbound;
    assert_eq!(p.canonical_external_id("1").unwrap(), "1");
    assert_eq!(p.canonical_external_id(" 123456 ").unwrap(), "123456");
    assert_eq!(p.canonical_external_id("4294967295").unwrap(), "4294967295");
    for bad in ["", "0", "007", "-1", "4294967296", "12a", "0x10", "1 2"] {
        assert!(
            matches!(p.canonical_external_id(bad), Err(IdentityError::InvalidExternalId { .. })),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn insignia_external_id_is_a_canonical_xuid() {
    let p = Provider::Insignia;
    assert_eq!(p.canonical_external_id("0x0009FFFF00000001").unwrap(), "0009ffff00000001");
    assert_eq!(p.canonical_external_id("9ffff00000001").unwrap(), "0009ffff00000001");
    assert_eq!(p.canonical_external_id("E000000000000001").unwrap(), "e000000000000001");
    let pseudo = format!("{:x}00000001", STEELBOUND_XUID_PREFIX);
    for bad in ["", "0", "0x", "10000000000000000", "xyz", pseudo.as_str()] {
        assert!(
            matches!(p.canonical_external_id(bad), Err(IdentityError::InvalidExternalId { .. })),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn pseudo_xuid_prefix_is_sbl_version_1() {
    assert_eq!(&STEELBOUND_XUID_PREFIX.to_be_bytes()[..3], b"SBL");
    assert_eq!(STEELBOUND_XUID_PREFIX & 0xFF, 1);
    // Positive as int64, so the database (bigint) sees the same value.
    assert!(((u64::from(STEELBOUND_XUID_PREFIX) << 32) as i64) > 0);
}
