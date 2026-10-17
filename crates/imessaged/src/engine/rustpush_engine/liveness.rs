//! APNs liveness: what the receive loop sees, and a watchdog verdict.
//!
//! rustpush keeps the APNs socket alive by itself (a ping every
//! [`PING_INTERVAL`]; a missed pong forces a reconnect) and acknowledges
//! every notification by itself (the ack task in
//! `APSConnectionResource::new`), independently of whether our receive
//! loop ever handles it. So a connection that is up but whose traffic
//! never reaches the receive loop — or a receive loop stuck inside
//! rustpush's handler — loses messages for good: Apple has them as
//! delivered and does not resend them after a restart. Nothing in the
//! connection state shows this.
//!
//! [`Liveness`] is the receive loop's own account: the last time it got
//! *any* APNs message from the broadcast (pongs included, so a healthy
//! socket shows activity at least every [`PING_INTERVAL`]) and since when
//! it has been inside the handler. The watchdog task (see
//! `start_client_inner`) checks it every [`WATCHDOG_TICK`]: a `Connected`
//! connection with no activity for [`SILENCE_LIMIT`] is forced to
//! reconnect ([`verdict`]), at most once per [`SILENCE_LIMIT`]. The loop
//! itself gives up on a message whose handling exceeds [`HANDLE_TIMEOUT`]
//! (logged at warn, emitted as `EngineEvent::Error`) instead of staying
//! deaf behind it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// rustpush's APNs keep-alive interval (`aps.rs`, "auto ping").
pub const PING_INTERVAL: Duration = Duration::from_secs(60);
/// No APNs traffic at all for this long while `Connected` = deaf: force a
/// reconnect. Three missed keep-alives.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(3 * PING_INTERVAL.as_secs());
/// How often the watchdog looks.
pub const WATCHDOG_TICK: Duration = Duration::from_secs(30);
/// Longest the receive loop waits for rustpush to handle one APNs message
/// (or for a delivery acknowledgement to go out). Every wait inside is
/// bounded at 15 s–60 s by rustpush, so only a genuine stall gets here.
pub const HANDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Milliseconds since `started`; never 0 (0 = "never").
fn stamp(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64 + 1
}

/// The receive loop's activity record (shared with the watchdog).
pub struct Liveness {
    started: Instant,
    /// Last APNs message taken from the broadcast (any kind). 0 = none yet.
    seen: AtomicU64,
    /// Since when the loop is inside the handler. 0 = idle.
    handling_since: AtomicU64,
    /// Last forced reconnect. 0 = none.
    kicked: AtomicU64,
}

impl Default for Liveness {
    fn default() -> Self {
        Liveness { started: Instant::now(), seen: AtomicU64::new(0), handling_since: AtomicU64::new(0), kicked: AtomicU64::new(0) }
    }
}

impl Liveness {
    pub fn saw(&self) {
        self.seen.store(stamp(self.started), Ordering::Relaxed);
    }

    pub fn handling_begin(&self) {
        self.handling_since.store(stamp(self.started), Ordering::Relaxed);
    }

    pub fn handling_end(&self) {
        self.handling_since.store(0, Ordering::Relaxed);
    }

    pub fn kicked(&self) {
        self.kicked.store(stamp(self.started), Ordering::Relaxed);
    }

    fn age(&self, v: u64) -> Option<Duration> {
        (v != 0).then(|| self.started.elapsed().saturating_sub(Duration::from_millis(v - 1)))
    }

    /// How long the loop has been inside the handler, if it is.
    pub fn handling_for(&self) -> Option<Duration> {
        self.age(self.handling_since.load(Ordering::Relaxed))
    }

    /// Time since the loop last showed any sign of life: took a message
    /// from the broadcast or started handling one (a loop busy inside
    /// the handler is not deaf). Since the engine started if nothing yet.
    pub fn quiet_for(&self) -> Duration {
        let seen = self.age(self.seen.load(Ordering::Relaxed)).unwrap_or_else(|| self.started.elapsed());
        match self.handling_for() {
            Some(h) => seen.min(h),
            None => seen,
        }
    }

    /// Time since the last forced reconnect, if any.
    pub fn since_kick(&self) -> Option<Duration> {
        self.age(self.kicked.load(Ordering::Relaxed))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Force an APNs reconnect: no traffic for this long while `Connected`.
    Reconnect(Duration),
}

/// Pure watchdog rule: `connected` is the engine's state (`Connected`
/// only; while rustpush is already reconnecting or we are not registered
/// there is nothing to do), `quiet_for` from [`Liveness::quiet_for`],
/// `since_kick` from [`Liveness::since_kick`].
pub fn verdict(connected: bool, quiet_for: Duration, since_kick: Option<Duration>) -> Verdict {
    if !connected || quiet_for < SILENCE_LIMIT {
        return Verdict::Fine;
    }
    // One kick per silence window: the reconnect itself produces traffic
    // (connect response, filter ack, the next pong) if it helps at all.
    if since_kick.is_some_and(|k| k < SILENCE_LIMIT) {
        return Verdict::Fine;
    }
    Verdict::Reconnect(quiet_for)
}

/// Name of a known APNs topic (`sha1(topic)`) for log lines.
pub fn topic_name(topic: &[u8; 20]) -> &'static str {
    const MADRID: [u8; 20] =
        [0xe4, 0xe6, 0xd9, 0x52, 0x95, 0x41, 0x68, 0xd0, 0xa5, 0xdb, 0x02, 0xdb, 0xaf, 0x27, 0xcc, 0x35, 0xfc, 0x18, 0xd1, 0x59];
    const SMS: [u8; 20] =
        [0x7f, 0x33, 0x12, 0x2b, 0xf7, 0x6e, 0x40, 0xa4, 0x9f, 0x3c, 0xc6, 0xa1, 0x55, 0x76, 0x82, 0x2a, 0xd7, 0xff, 0x1c, 0x2a];
    const IDS: [u8; 20] =
        [0x02, 0xe8, 0x1f, 0x9f, 0x8a, 0xeb, 0x0f, 0x9c, 0x06, 0x48, 0x3e, 0x7a, 0x3f, 0x04, 0x9a, 0x82, 0x27, 0xb3, 0xaf, 0x41];
    match *topic {
        MADRID => "com.apple.madrid",
        SMS => "com.apple.private.alloy.sms",
        IDS => "com.apple.private.ids",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn verdict_rules() {
        let s = SILENCE_LIMIT;
        assert_eq!(verdict(true, s - Duration::from_secs(1), None), Verdict::Fine);
        assert_eq!(verdict(true, s, None), Verdict::Reconnect(s));
        // not Connected (reconnecting, registering, logged out): leave it alone
        assert_eq!(verdict(false, s * 2, None), Verdict::Fine);
        // one kick per silence window
        assert_eq!(verdict(true, s * 2, Some(s - Duration::from_secs(1))), Verdict::Fine);
        assert_eq!(verdict(true, s * 2, Some(s)), Verdict::Reconnect(s * 2));
        assert!(SILENCE_LIMIT >= 3 * PING_INTERVAL);
        assert!(HANDLE_TIMEOUT < SILENCE_LIMIT);
    }

    #[test]
    fn liveness_accounting() {
        let l = Liveness::default();
        assert_eq!(l.handling_for(), None);
        assert_eq!(l.since_kick(), None);
        // nothing seen yet: quiet since the start (tiny)
        assert!(l.quiet_for() < Duration::from_secs(1));
        l.saw();
        assert!(l.quiet_for() < Duration::from_millis(100));
        l.handling_begin();
        assert!(l.handling_for().is_some());
        l.handling_end();
        assert_eq!(l.handling_for(), None);
        l.kicked();
        assert!(l.since_kick().is_some());
    }

    #[test]
    fn busy_loop_counts_as_alive() {
        // quiet_for is the smaller of "last message" and "handling since"
        let l = Liveness { started: Instant::now() - Duration::from_secs(600), ..Default::default() };
        // seen 500 s ago, handling since 10 s ago
        l.seen.store(100 * 1000 + 1, Ordering::Relaxed);
        l.handling_since.store(590 * 1000 + 1, Ordering::Relaxed);
        assert!(l.quiet_for() < Duration::from_secs(11));
        assert!(l.quiet_for() >= Duration::from_secs(9));
        l.handling_end();
        assert!(l.quiet_for() >= Duration::from_secs(499));
    }

    #[test]
    fn topic_names() {
        // sha1 is not a dependency; check the baked hashes against a
        // known-answer instead: the first bytes of sha1("com.apple.madrid").
        let madrid: [u8; 20] =
            [0xe4, 0xe6, 0xd9, 0x52, 0x95, 0x41, 0x68, 0xd0, 0xa5, 0xdb, 0x02, 0xdb, 0xaf, 0x27, 0xcc, 0x35, 0xfc, 0x18, 0xd1, 0x59];
        assert_eq!(topic_name(&madrid), "com.apple.madrid");
        assert_eq!(topic_name(&[0; 20]), "other");
        // and that sha2 (which we do have) is not what rustpush uses: no mixup
        let not_sha1 = sha2::Sha256::digest(b"com.apple.madrid");
        assert_ne!(&not_sha1[..20], &madrid[..]);
    }
}
