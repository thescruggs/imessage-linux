# patches/

Applied to the git submodules by `tools/bootstrap.sh` (idempotent: a patch is
skipped when `git apply --check --reverse` shows it is already in place). Never
commit changes inside `third_party/`; change these files instead.

Upstream commits the patches target:

| repo | path | commit |
|---|---|---|
| rustpush | `third_party/rustpush` | `f35c4ee062b3c3eae54dc96b89b90ee99f5e1d0c` |
| apple-private-apis | `third_party/rustpush/third_party/apple-private-apis` | `b8598d2c1656ae73fbe0f1be1b18222ea7cadb0a` |

rustpush's own `open-absinthe` submodule (OpenBubbles' closed stub) is never
initialised; `crates/open-absinthe` replaces it.

## rustpush/01-rustpush.patch

Verbatim copy of `01-rustpush.patch` from
[dennisshey/relay](https://github.com/dennisshey/relay) (`app/src/main/rust/patches/`),
generated against `f35c4ee`:

* `src/activation.rs`: `FAIRPLAY_KEYS` uses the committed
  `certs/legacy-fairplay` cert (the `certs/fairplay/4056…` certs it references
  are not in the repo, and albert.apple.com rejects them).
* `src/activation.rs`: the `<Protocol>` regex spans newlines.
* `src/ids/user.rs`: strips `tel:` handles before registering; Mac-class
  validation data only registers email handles and a phone handle fails the
  whole request with status 6001.
* `src/auth.rs`: drops the anisette provider's `X-Mme-Client-Info` and sends
  the OS config's device id, so the headers agree with the presented Mac.
* `Cargo.toml`: adds a `local-anisette` feature forwarding to
  apple-private-apis (needs the patch below).

## rustpush/02-open-absinthe-path.patch

Ours. Points rustpush's `open-absinthe` dependency at
`../../crates/open-absinthe` (still `optional = true, features = ["serde"]`).

## rustpush/03-phone-handle.patch

Ours; applies on top of 01. `src/ids/user.rs`: `register()` keeps the `tel:`
handle Apple offers (the phone number the account's iPhone linked) and
registers it with the emails, as a Mac does; Text Message Forwarding and
iMessages to the number are addressed to it. If Apple refuses the request
with the number (status 6001, the case 01 guards against), it registers the
email handles alone instead of failing. Logs the handles offered.

## apple-private-apis/01-local-anisette.patch

Verbatim copy of relay's `02-apple-private-apis.patch`. Restores the on-device
Apple ADI anisette provider that upstream deleted (`adi_proxy.rs`,
`store_services_core.rs`, `anisette_headers_provider.rs`, from `91e6890~1`)
behind a new `local-anisette` feature, plus an `SSCAnisetteProvider` for the
current `AnisetteProvider` trait. Not enabled yet (Phase 6); applied now because
patch 01 references the feature and cargo rejects unknown feature names.
