//! Offline reference test (runs by default, no network): pypush's reference
//! identity + Apple's public validation cert (`cert-1.0.plist`, placed by
//! tools/bootstrap.sh) through `nac_init` must give a 338-byte request with no
//! hook anomalies.
//!
//! Skips (passes with a `SKIP:` line) when the Apple binary or the cert file
//! is absent, so CI without the non-redistributable binary stays green.

mod common;

use open_absinthe::nac::{self, ValidationCtx};

#[test]
fn reference_request_is_338_bytes() {
    let _ = env_logger::builder().is_test(true).try_init();

    let bin = nac::binary_path();
    if !bin.exists() {
        println!("SKIP: {} not present (run tools/bootstrap.sh)", bin.display());
        return;
    }
    let cert = match common::local_cert() {
        Ok(c) => c,
        Err(e) => {
            println!("SKIP: cert unavailable ({e}; run tools/bootstrap.sh)");
            return;
        }
    };
    let hw = common::reference_hw();

    // Twice: each context is independent, and the request size is stable even
    // though its bytes are randomised (arc4random) per run.
    let mut requests = Vec::new();
    for run in 1..=2 {
        let mut request = Vec::new();
        let ctx = ValidationCtx::new(&cert, &mut request, &hw).expect("nac_init failed");
        println!("run {run}: request {} bytes, hook anomalies {}", request.len(), ctx.hook_anomalies());
        assert_eq!(request.len(), 338, "nac_init request should be 338 bytes for the reference identity");
        assert_eq!(ctx.hook_anomalies(), 0, "a hook logged an error/warning on a healthy run");
        requests.push(request);
    }
    assert_ne!(requests[0], requests[1], "requests should differ (arc4random)");
}
