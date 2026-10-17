//! Live end-to-end test: Apple cert (via the IDS bag) -> request (338) ->
//! Apple `initializeValidation` -> session-info (698) -> key establishment ->
//! sign (517).
//!
//! Uses JJTech's published reference identity only (as upstream tests do) and
//! registers nothing. Ignored by default; run with:
//!   cargo test -p open-absinthe --release -- --ignored --nocapture

mod common;

use open_absinthe::nac::{self, ValidationCtx};

#[test]
#[ignore = "requires network; hits Apple's initializeValidation endpoint"]
fn reference_validation_data_is_517_bytes() {
    let _ = env_logger::builder().is_test(true).try_init();

    let bin = nac::binary_path();
    if !bin.exists() {
        println!("SKIP: {} not present (run tools/bootstrap.sh)", bin.display());
        return;
    }
    let Some(bag) = common::fetch_bag() else {
        println!("SKIP: network unavailable (could not fetch the IDS bag)");
        return;
    };
    let cert = common::fetch_cert_chain(&bag).expect("id-validation-cert from the bag");
    let init_url = bag
        .get("id-initialize-validation")
        .and_then(|v| v.as_string())
        .expect("id-initialize-validation in the bag")
        .to_owned();
    let hw = common::reference_hw();

    let mut request = Vec::new();
    let mut ctx = ValidationCtx::new(&cert, &mut request, &hw).expect("nac_init failed");
    println!("request: {} bytes", request.len());
    assert_eq!(request.len(), 338, "unexpected request size");

    let mut body = plist::Dictionary::new();
    body.insert("session-info-request".into(), plist::Value::Data(request));
    let mut body_bytes = Vec::new();
    plist::to_writer_xml(&mut body_bytes, &plist::Value::Dictionary(body)).expect("encode plist");
    let resp = common::http_post_plist(&init_url, &body_bytes).expect("initializeValidation POST failed");
    let resp: plist::Dictionary = plist::from_bytes(&resp).expect("parse initializeValidation response");
    let session_info = resp
        .get("session-info")
        .and_then(|v| v.as_data())
        .unwrap_or_else(|| panic!("no session-info in response: {resp:?}"))
        .to_vec();
    println!("session-info: {} bytes", session_info.len());
    assert_eq!(session_info.len(), 698, "unexpected session-info size");

    ctx.key_establishment(&session_info).expect("key_establishment failed");
    let data = ctx.sign().expect("sign failed");
    println!("validation data: {} bytes", data.len());
    assert_eq!(ctx.hook_anomalies(), 0, "a hook logged an error/warning on a healthy run");
    assert_eq!(data.len(), 517, "validation data should be 517 bytes (389 means a ROM/MLB hook missed)");
}

/// Same pipeline with a real identity from `tools/mac-hwinfo.sh`, to check the
/// emulator copes with it (Apple Silicon values, 17-byte `*_enc` blobs). Only
/// runs when `OPEN_ABSINTHE_HWCONFIG` points at a hwconfig.plist. Nothing is
/// registered; the identity is read, used for one validation round, and never
/// printed.
///
/// The validation-data size depends on the identity (the binary sizes its
/// buffers from board_id / product_name / serial lengths): 517 bytes for the
/// Intel reference identity, 389 for a real Apple Silicon Mac, which
/// Apple accepted for registration on 2026-10-16. So this only checks that
/// the round succeeds and the blob is plausibly sized.
#[test]
#[ignore = "requires network and OPEN_ABSINTHE_HWCONFIG=<hwconfig.plist>"]
fn own_identity_validation_data_is_accepted() {
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(path) = std::env::var_os("OPEN_ABSINTHE_HWCONFIG") else {
        println!("SKIP: OPEN_ABSINTHE_HWCONFIG not set");
        return;
    };
    let bin = nac::binary_path();
    if !bin.exists() {
        println!("SKIP: {} not present (run tools/bootstrap.sh)", bin.display());
        return;
    }
    let hw = load_hwconfig(&path);

    let Some(bag) = common::fetch_bag() else {
        println!("SKIP: network unavailable (could not fetch the IDS bag)");
        return;
    };
    let cert = common::fetch_cert_chain(&bag).expect("id-validation-cert from the bag");
    let init_url = bag
        .get("id-initialize-validation")
        .and_then(|v| v.as_string())
        .expect("id-initialize-validation in the bag")
        .to_owned();

    let mut request = Vec::new();
    let mut ctx = ValidationCtx::new(&cert, &mut request, &hw).expect("nac_init failed");
    println!("request: {} bytes", request.len());
    assert_eq!(request.len(), 338, "unexpected request size");

    let mut body = plist::Dictionary::new();
    body.insert("session-info-request".into(), plist::Value::Data(request));
    let mut body_bytes = Vec::new();
    plist::to_writer_xml(&mut body_bytes, &plist::Value::Dictionary(body)).expect("encode plist");
    let resp = common::http_post_plist(&init_url, &body_bytes).expect("initializeValidation POST failed");
    let resp: plist::Dictionary = plist::from_bytes(&resp).expect("parse initializeValidation response");
    let session_info = resp
        .get("session-info")
        .and_then(|v| v.as_data())
        .unwrap_or_else(|| panic!("no session-info in response: {resp:?}"))
        .to_vec();
    println!("session-info: {} bytes", session_info.len());
    assert_eq!(session_info.len(), 698, "unexpected session-info size");

    ctx.key_establishment(&session_info).expect("key_establishment failed");
    let data = ctx.sign().expect("sign failed");
    println!("validation data: {} bytes", data.len());
    assert_eq!(ctx.hook_anomalies(), 0, "a hook logged an error/warning on a healthy run");
    assert!((300..=800).contains(&data.len()), "implausible validation data size {} (expected 300..=800 bytes)", data.len());
}

/// Parse the `inner` dict of a `tools/mac-hwinfo.sh` plist.
fn load_hwconfig(path: &std::ffi::OsStr) -> open_absinthe::nac::HardwareConfig {
    let root: plist::Dictionary = plist::from_file(path).expect("parse hwconfig.plist");
    let inner = root.get("inner").and_then(|v| v.as_dictionary()).expect("`inner` dict");
    let s = |k: &str| inner.get(k).and_then(|v| v.as_string()).unwrap_or_else(|| panic!("inner.{k}")).to_owned();
    let d = |k: &str| inner.get(k).and_then(|v| v.as_data()).unwrap_or_else(|| panic!("inner.{k}")).to_vec();
    let mac: [u8; 6] = d("io_mac_address").try_into().expect("io_mac_address is 6 bytes");
    open_absinthe::nac::HardwareConfig {
        product_name: s("product_name"),
        io_mac_address: mac,
        platform_serial_number: s("platform_serial_number"),
        platform_uuid: s("platform_uuid"),
        root_disk_uuid: s("root_disk_uuid"),
        board_id: s("board_id"),
        os_build_num: s("os_build_num"),
        rom: d("rom"),
        mlb: s("mlb"),
        platform_serial_number_enc: d("platform_serial_number_enc"),
        platform_uuid_enc: d("platform_uuid_enc"),
        root_disk_uuid_enc: d("root_disk_uuid_enc"),
        rom_enc: d("rom_enc"),
        mlb_enc: d("mlb_enc"),
    }
}

/// One full validation round; returns the validation-data length.
fn validation_len(hw: &open_absinthe::nac::HardwareConfig, cert: &[u8], init_url: &str) -> usize {
    let mut request = Vec::new();
    let mut ctx = ValidationCtx::new(cert, &mut request, hw).expect("nac_init failed");
    let mut body = plist::Dictionary::new();
    body.insert("session-info-request".into(), plist::Value::Data(request));
    let mut body_bytes = Vec::new();
    plist::to_writer_xml(&mut body_bytes, &plist::Value::Dictionary(body)).expect("encode plist");
    let resp = common::http_post_plist(init_url, &body_bytes).expect("initializeValidation POST failed");
    let resp: plist::Dictionary = plist::from_bytes(&resp).expect("parse initializeValidation response");
    let session_info = resp.get("session-info").and_then(|v| v.as_data()).expect("session-info").to_vec();
    ctx.key_establishment(&session_info).expect("key_establishment failed");
    ctx.sign().expect("sign failed").len()
}

/// Diagnostic (prints only, asserts nothing): swap each field of the real
/// identity into the reference one, and each reference field into the real
/// one, and print the resulting validation-data length. Shows which fields
/// drive the size difference between identities (see
/// `own_identity_validation_data_is_accepted`).
#[test]
#[ignore = "diagnostic; requires network and OPEN_ABSINTHE_HWCONFIG"]
fn own_identity_bisect() {
    let Some(path) = std::env::var_os("OPEN_ABSINTHE_HWCONFIG") else {
        println!("SKIP: OPEN_ABSINTHE_HWCONFIG not set");
        return;
    };
    if !nac::binary_path().exists() {
        return;
    }
    let own = load_hwconfig(&path);
    let bag = common::fetch_bag().expect("bag");
    let cert = common::fetch_cert_chain(&bag).expect("cert");
    let init_url = bag.get("id-initialize-validation").and_then(|v| v.as_string()).expect("url").to_owned();
    let reference = common::reference_hw();
    println!("reference: {}", validation_len(&reference, &cert, &init_url));
    println!("own:       {}", validation_len(&own, &cert, &init_url));
    type F = fn(&mut open_absinthe::nac::HardwareConfig, &open_absinthe::nac::HardwareConfig);
    let swaps: &[(&str, F)] = &[
        ("product_name", |t, o| t.product_name = o.product_name.clone()),
        ("io_mac_address", |t, o| t.io_mac_address = o.io_mac_address),
        ("platform_serial_number", |t, o| t.platform_serial_number = o.platform_serial_number.clone()),
        ("platform_uuid", |t, o| t.platform_uuid = o.platform_uuid.clone()),
        ("root_disk_uuid", |t, o| t.root_disk_uuid = o.root_disk_uuid.clone()),
        ("board_id", |t, o| t.board_id = o.board_id.clone()),
        ("os_build_num", |t, o| t.os_build_num = o.os_build_num.clone()),
        ("rom", |t, o| t.rom = o.rom.clone()),
        ("mlb", |t, o| t.mlb = o.mlb.clone()),
        ("platform_serial_number_enc", |t, o| t.platform_serial_number_enc = o.platform_serial_number_enc.clone()),
        ("platform_uuid_enc", |t, o| t.platform_uuid_enc = o.platform_uuid_enc.clone()),
        ("root_disk_uuid_enc", |t, o| t.root_disk_uuid_enc = o.root_disk_uuid_enc.clone()),
        ("rom_enc", |t, o| t.rom_enc = o.rom_enc.clone()),
        ("mlb_enc", |t, o| t.mlb_enc = o.mlb_enc.clone()),
    ];
    for (name, swap) in swaps {
        let mut t = common::reference_hw();
        swap(&mut t, &own);
        println!("reference + own {name:<28} -> {}", validation_len(&t, &cert, &init_url));
    }
    for (name, swap) in swaps {
        let mut t = own.clone();
        swap(&mut t, &reference);
        println!("own + reference {name:<28} -> {}", validation_len(&t, &cert, &init_url));
    }
}
