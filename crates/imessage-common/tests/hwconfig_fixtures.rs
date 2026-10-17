//! Integration tests against synthetic (fake) hwconfig fixtures.

use imessage_common::hwconfig::{self, Level, MacOSConfig};

const GOOD: &[u8] = include_bytes!("data/hwconfig-fake.plist");
const MISSING: &[u8] = include_bytes!("data/hwconfig-fake-missing-rom-mlb_enc.plist");

#[test]
fn fixture_parses() {
    let c = MacOSConfig::from_slice(GOOD).expect("fixture parses");
    assert_eq!(c.inner.product_name, "Macmini7,1");
    assert_eq!(c.inner.platform_serial_number, "C02FAKE00001");
    assert_eq!(c.inner.io_mac_address, [0x02, 0x00, 0x5e, 0x10, 0x20, 0x30]);
    assert_eq!(c.inner.rom.len(), 6);
    assert_eq!(c.protocol_version, 1640);
    assert_eq!(c.device_id, c.inner.platform_uuid);
    assert!(c.udid.is_none());
}

#[test]
fn fixture_round_trips() {
    let c = MacOSConfig::from_slice(GOOD).unwrap();
    let xml = c.to_xml().unwrap();
    let back = MacOSConfig::from_slice(&xml).unwrap();
    assert_eq!(back, c);
    // Byte fields stay <data>, so the round-tripped file still inspects clean.
    let r = hwconfig::inspect_bytes(&xml).unwrap();
    assert_ne!(r.worst(), Level::Fail);
}

#[test]
fn fixture_inspects_without_fail() {
    let r = hwconfig::inspect_bytes(GOOD).unwrap();
    let fails: Vec<_> = r.checks.iter().filter(|c| c.level == Level::Fail).collect();
    assert!(fails.is_empty(), "unexpected FAIL checks: {fails:?}");
    assert!(r.parse_error.is_none());
    assert!(r.config.is_some());
    assert_ne!(r.worst(), Level::Fail);
}

#[test]
fn report_never_prints_raw_identifiers() {
    let r = hwconfig::inspect_bytes(GOOD).unwrap();
    for c in &r.checks {
        assert!(!c.detail.contains("C02FAKE00001"), "serial leaked: {c:?}");
        assert!(!c.detail.contains("C02FAKEMLB000001"), "MLB leaked: {c:?}");
        assert!(!c.detail.contains("FA4E0000-0000"), "UUID leaked: {c:?}");
    }
}

#[test]
fn missing_rom_and_mlb_enc_fail() {
    let r = hwconfig::inspect_bytes(MISSING).unwrap();
    assert_eq!(r.worst(), Level::Fail);
    for field in ["inner.rom", "inner.mlb_enc"] {
        assert!(
            r.checks.iter().any(|c| c.field == field && c.level == Level::Fail),
            "{field} should be reported as FAIL: {:?}",
            r.checks
        );
    }
    assert!(r.config.is_none());
    assert!(r.parse_error.is_some());
}

/// Drift guard: `inner` must deserialize identically as open-absinthe's
/// `HardwareConfig` (the type rustpush hands to the NAC emulator).
#[test]
fn fixture_inner_matches_open_absinthe() {
    let c = MacOSConfig::from_slice(GOOD).unwrap();
    let root: plist::Value = plist::from_bytes(GOOD).unwrap();
    let inner = root.as_dictionary().unwrap().get("inner").unwrap().clone();
    let mut xml = Vec::new();
    plist::to_writer_xml(&mut xml, &inner).unwrap();
    let oa: open_absinthe::nac::HardwareConfig = plist::from_bytes(&xml).expect("inner parses as open_absinthe HardwareConfig");
    let h = &c.inner;
    assert_eq!(oa.product_name, h.product_name);
    assert_eq!(oa.io_mac_address, h.io_mac_address);
    assert_eq!(oa.platform_serial_number, h.platform_serial_number);
    assert_eq!(oa.platform_uuid, h.platform_uuid);
    assert_eq!(oa.root_disk_uuid, h.root_disk_uuid);
    assert_eq!(oa.board_id, h.board_id);
    assert_eq!(oa.os_build_num, h.os_build_num);
    assert_eq!(oa.platform_serial_number_enc, h.platform_serial_number_enc);
    assert_eq!(oa.platform_uuid_enc, h.platform_uuid_enc);
    assert_eq!(oa.root_disk_uuid_enc, h.root_disk_uuid_enc);
    assert_eq!(oa.rom, h.rom);
    assert_eq!(oa.rom_enc, h.rom_enc);
    assert_eq!(oa.mlb, h.mlb);
    assert_eq!(oa.mlb_enc, h.mlb_enc);
}
