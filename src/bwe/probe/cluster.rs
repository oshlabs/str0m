//! Probe cluster data structures for bandwidth estimation.
//!
//! Probe clusters are short bursts of packets sent at specific bitrates to test
//! network capacity.

use std::cmp;
use std::time::{Duration, Instant};

use crate::rtp_::{Bitrate, DataSize, TwccClusterId};
use crate::util::{already_happened, not_happening};

/// Preferred size of a probe padding burst, toward a typical MTU.
///
/// Padding requests of this size let the padding generator use spurious resends
/// (full-size RTX packets from the cache). Smaller requests fall back to blank
/// padding packets, which the RTP padding format caps at 255 bytes - at low
/// probe bitrates such crumbs cannot satisfy a cluster's minimum packet count
/// within its byte budget.
const PROBE_BURST_TARGET_SIZE: DataSize = DataSize::bytes(1200);

/// The longest a probe cluster may take on the send side before it is
/// abandoned. Matches the receive-side estimator's `MAX_PROBE_INTERVAL`
/// (WebRTC's `kMaxProbeInterval`): a cluster stretched longer than this can
/// no longer produce a valid estimate.
pub(crate) const MAX_PROBE_CLUSTER_DURATION: Duration = Duration::from_secs(1);

/// Configuration for a probe cluster (the plan).
///
/// This represents the immutable blueprint for a bandwidth probe: what bitrate
/// to test, for how long, and with what constraints.
#[derive(Debug, Clone, Copy)]
pub struct ProbeClusterConfig {
    /// Unique identifier for this probe cluster
    cluster: TwccClusterId,

    /// Target bitrate to probe at (e.g., 3 Mbps)
    target_bitrate: Bitrate,

    /// How long to sustain the target bitrate (e.g., 15ms)
    target_duration: Duration,

    /// Minimum number of packets to send (e.g., 5)
    /// This ensures statistical validity even for short bursts.
    min_packet_count: usize,

    /// Delta time between sent bursts of packets during probe.
    ///
    /// Mirrors WebRTC's `ProbeClusterConfig.min_probe_delta`. The pacer/probe sender must not
    /// schedule probe packets more frequently than this.
    min_probe_delta: Duration,

    /// The kind of probe this is.
    kind: ProbeKind,
}

/// Kind of probe
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeKind {
    Initial,
    Exponential,
    IncreaseAlr,
    PeriodicAlr,
    LargeDrop,
    Stagnant,
}

impl ProbeKind {
    pub(crate) fn is_alr(&self) -> bool {
        matches!(self, ProbeKind::PeriodicAlr | ProbeKind::IncreaseAlr)
    }
}

impl ProbeClusterConfig {
    /// Create a new probe cluster configuration with standard defaults:
    /// - 15ms duration (enough to get meaningful feedback without excessive delay)
    /// - 5 minimum packets (statistical significance for BWE analysis)
    pub fn new(cluster: TwccClusterId, target_bitrate: Bitrate, kind: ProbeKind) -> Self {
        Self {
            cluster,
            target_bitrate,
            // WebRTC defaults
            target_duration: Duration::from_millis(15),
            min_packet_count: 5,
            // WebRTC default for general probing (not initial/network-state): 2ms.
            min_probe_delta: Duration::from_millis(2),
            kind,
        }
    }

    /// Set a custom target duration for this probe.
    /// WebRTC uses 100ms for initial probes to allow time for media to start.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.target_duration = duration;
        self
    }

    /// Set a custom minimum packet count for this probe.
    pub fn with_min_packet_count(mut self, min_packet_count: usize) -> Self {
        self.min_packet_count = min_packet_count;
        self
    }

    /// Set a custom minimum probe delta (spacing constraint) for this probe.
    pub fn with_min_probe_delta(mut self, min_probe_delta: Duration) -> Self {
        self.min_probe_delta = min_probe_delta;
        self
    }

    /// Check if this probe was created during ALR.
    pub fn is_alr_probe(&self) -> bool {
        self.kind.is_alr()
    }

    /// Get the probe cluster ID.
    pub fn cluster(&self) -> TwccClusterId {
        self.cluster
    }

    /// Get the target bitrate.
    pub fn target_bitrate(&self) -> Bitrate {
        self.target_bitrate
    }

    /// Get the minimum packet count required for a valid probe.
    pub fn min_packet_count(&self) -> usize {
        self.min_packet_count
    }

    /// Get the minimum probe delta (spacing constraint).
    pub fn min_probe_delta(&self) -> Duration {
        self.min_probe_delta
    }

    /// Calculate the target bytes for this probe.
    /// This is how much data we expect to send at target_bitrate for target_duration.
    pub fn target_bytes(&self) -> DataSize {
        self.target_bitrate * self.target_duration
    }
}

/// Runtime state of an active probe cluster (the execution).
///
/// This tracks what's actually happening as we send probe packets: how much
/// we've sent, how many packets, and when we started.
#[derive(Debug)]
pub struct ProbeClusterState {
    /// The immutable plan
    config: ProbeClusterConfig,

    /// Total bytes sent so far in this probe
    bytes_sent: DataSize,

    /// Total packets sent so far in this probe
    packets_sent: usize,

    /// When the first packet was sent
    /// None = probe hasn't started yet (still queued)
    started_at: Option<Instant>,

    /// When the last packet was sent
    /// Used to calculate actual send duration (first to last packet)
    last_packet_at: Option<Instant>,

    /// When we last created a padding request
    /// Used to prevent creating multiple padding packets for the same instant
    last_padding_at: Instant,
}

impl ProbeClusterState {
    /// Create a new probe cluster state from a config.
    pub fn new(config: ProbeClusterConfig) -> Self {
        Self {
            config,
            bytes_sent: DataSize::ZERO,
            packets_sent: 0,
            started_at: None,
            last_packet_at: None,
            last_padding_at: already_happened(),
        }
    }

    /// Get the probe configuration.
    pub fn config(&self) -> &ProbeClusterConfig {
        &self.config
    }

    /// Calculates the next probe time based on total bytes sent and target bitrate.
    /// This naturally handles variable packet sizes (media packets can be much larger
    /// than padding packets). Next packet can be sent when:
    /// `now >= start_time + (bytes_sent / target_bitrate)`
    pub fn next_probe_time(&self) -> Instant {
        let Some(started_at) = self.started_at else {
            return already_happened();
        };

        if self.config.target_bitrate == Bitrate::ZERO {
            return not_happening();
        }

        // Packet-level pacing: schedule based on bytes sent at the target bitrate.
        // next_time = start_time + (bytes_sent / target_bitrate)
        let send_duration = self.bytes_sent / self.config.target_bitrate;
        let probe_time = started_at + send_duration;

        let min_burst_time = self.last_padding_at + self.config.min_probe_delta();
        probe_time.max(min_burst_time)
    }

    /// Check if the probe cluster is complete.
    ///
    /// A probe is complete when BOTH conditions are met (matching WebRTC):
    /// 1. Sent bytes >= target_bytes (target_bitrate * target_duration)
    /// 2. Sent packets >= min_packet_count
    ///
    /// Note: WebRTC does NOT check duration for completion. Duration is only used
    /// to calculate the target bytes threshold. This ensures probes complete even
    /// when all packets are sent at the same instant (e.g., in tests or when the
    /// pacer generates bursts faster than wall-clock time advances).
    pub fn is_complete(&self, _now: Instant) -> bool {
        if self.started_at.is_none() {
            return false; // Not started yet
        }

        // Match WebRTC's BitrateProber::ProbeSent() logic:
        // if (sent_bytes >= probe_cluster_min_bytes && sent_probes >= probe_cluster_min_probes)
        let bytes_met = self.bytes_sent >= self.config.target_bytes();
        let packets_met = self.packets_sent >= self.config.min_packet_count;

        bytes_met && packets_met
    }

    /// Check if it's time to send the next probe packet.
    ///
    /// Returns `true` if `now >= next_probe_time()`, meaning we should send a packet now.
    pub fn should_send_now(&self, now: Instant) -> bool {
        let next_time = self.next_probe_time();
        let min_burst_time = self.last_padding_at + self.config.min_probe_delta();
        now >= next_time && now >= min_burst_time
    }

    /// Calculate how much padding should be generated for the next probe packet.
    ///
    /// WebRTC probing uses **bursts** of packets, with a minimum delta between bursts
    /// (`min_probe_delta`). In str0m, a `PaddingRequest` is expressed as a number of bytes
    /// to queue (which may result in multiple RTP padding packets), so we request a burst
    /// sized approximately as:
    ///
    /// `burst_bytes = target_bitrate * min_probe_delta`
    ///
    /// This avoids unintentionally capping probe throughput at `MAX_PADDING_PACKET_SIZE /
    /// min_probe_delta` (e.g. 240 bytes / 2ms = 960 kbit/s), which would be far below the
    /// intended probe bitrate.
    ///
    /// Returns `None` if it's not time to send yet, or if we've already created
    /// a padding packet for this instant.
    pub fn next_packet(&mut self, now: Instant) -> Option<DataSize> {
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }

        // Enforce min burst spacing (`min_probe_delta`) between successive padding requests.
        // (The very first burst is not gated because `last_padding_at` starts at already_happened()).
        if now < self.last_padding_at + self.config.min_probe_delta() {
            return None;
        }

        if now < self.next_probe_time() {
            return None;
        }

        // Check if we've already created a padding packet for this exact instant
        // This prevents creating multiple padding packets when handle_timeout() is
        // called multiple times with the same `now` value
        if self.last_padding_at >= now {
            return None;
        }

        // The cluster is only complete when BOTH the byte target and the minimum
        // packet count are met (see `is_complete`). The byte target is a floor
        // derived from target_bitrate * target_duration, not a ceiling: keep
        // generating padding until both criteria can be satisfied. Capping
        // generation at the byte target would wedge low-bitrate clusters
        // forever, since their byte budget fills before the packet count.
        let bytes_remaining = self.config.target_bytes().saturating_sub(self.bytes_sent);
        let packets_remaining = self
            .config
            .min_packet_count()
            .saturating_sub(self.packets_sent);

        if bytes_remaining == DataSize::ZERO && packets_remaining == 0 {
            return None;
        }

        // Mark that we've created padding for this instant
        self.last_padding_at = now;

        // It's time to send a new probe burst.
        // Calculate recommended probe size as target_bitrate * min_probe_delta.
        let min_delta = self.config.min_probe_delta();
        let rate_burst = self.config.target_bitrate * min_delta;

        // Size bursts toward the MTU (see PROBE_BURST_TARGET_SIZE), but never so
        // large that the minimum packet count of bursts, paced at the target
        // bitrate, would stretch the cluster past the estimator's maximum valid
        // probe interval.
        let packets_for_interval = (self.config.min_packet_count() + 1).max(1) as u32;
        let interval_bound =
            self.config.target_bitrate * (MAX_PROBE_CLUSTER_DURATION / packets_for_interval);
        let burst = DataSize::bytes(cmp::max(
            rate_burst.as_bytes_i64(),
            cmp::min(
                PROBE_BURST_TARGET_SIZE.as_bytes_i64(),
                interval_bound.as_bytes_i64(),
            ),
        ));

        // When only bytes are missing, don't overshoot the byte target.
        let request_bytes = if packets_remaining > 0 {
            burst
        } else {
            cmp::min(bytes_remaining, burst)
        };

        if request_bytes == DataSize::ZERO {
            None
        } else {
            Some(request_bytes)
        }
    }

    /// Check if this probe cluster has been going on for too long to ever
    /// produce a valid estimate. Such a cluster must be abandoned: it would
    /// otherwise occupy the pacer's probe queue forever (probes are only
    /// dequeued on completion) and leave the probe controller waiting for an
    /// answer that never comes.
    pub fn is_timed_out(&self, now: Instant) -> bool {
        let Some(started_at) = self.started_at else {
            return false;
        };

        now.saturating_duration_since(started_at) > MAX_PROBE_CLUSTER_DURATION
    }

    /// Record a packet that was sent as part of this probe (media or padding).
    ///
    /// This updates the probe's tracking state so that `next_probe_time()` correctly
    /// calculates when the next packet should be sent based on the target bitrate.
    ///
    /// Should be called by the pacer whenever ANY packet is sent during an active probe,
    /// not just padding packets generated by `next_packet()`.
    pub fn record_packet(&mut self, now: Instant, size: DataSize) {
        // If a probe is active and we observe a media packet before any probe-generated padding,
        // treat that as the start of the probe. This keeps timing semantics consistent and ensures
        // `next_probe_time()` enforces `min_probe_delta` from the first packet onwards.
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }

        self.bytes_sent += size;
        self.packets_sent += 1;
        self.last_packet_at = Some(now);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Test helper to directly set state for testing
    impl ProbeClusterState {
        fn test_set_state(&mut self, bytes_sent: DataSize, packets_sent: usize, now: Instant) {
            self.bytes_sent = bytes_sent;
            self.packets_sent = packets_sent;
            if self.started_at.is_none() {
                self.started_at = Some(now);
            }
            self.last_packet_at = Some(now);
        }
    }

    #[test]
    fn probe_cluster_not_complete_before_start() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let state = ProbeClusterState::new(config);

        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_not_complete_after_packets_but_not_bytes() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 5 packets (meets packet count) but only small packets (doesn't meet bytes)
        // 5 * 100 = 500 bytes, which is < 5,625 bytes
        state.test_set_state(DataSize::bytes(500), 5, now);

        // Not complete: packets met but bytes not met
        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_not_complete_after_bytes_but_not_packets() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 2 large packets (meets bytes) but not enough packets
        // 2 * 3000 = 6000 bytes > 5,625 bytes, but only 2 packets < 5 packets
        state.test_set_state(DataSize::bytes(6000), 2, now);

        // Not complete: bytes met but packets not met
        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_complete_when_both_criteria_met() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 5 packets of 1200 bytes each = 6000 bytes
        // Meets both: 6000 >= 5625 bytes AND 5 >= 5 packets
        state.test_set_state(DataSize::bytes(6000), 5, now);

        // Complete: both bytes and packets met, even at same instant
        assert!(state.is_complete(now));
    }

    #[test]
    fn probe_cluster_complete_even_with_zero_duration() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        // Send all packets at exactly the same instant (duration = 0)
        state.test_set_state(DataSize::bytes(6000), 5, now);

        // Should still complete (this is the bug fix - no duration requirement)
        assert!(state.is_complete(now));
    }

    #[test]
    fn probe_cluster_tracks_bytes_and_packets() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        state.test_set_state(DataSize::bytes(1200), 1, now);
        assert_eq!(state.bytes_sent, DataSize::bytes(1200));
        assert_eq!(state.packets_sent, 1);

        state.test_set_state(DataSize::bytes(2200), 2, now + Duration::from_millis(1));
        assert_eq!(state.bytes_sent, DataSize::bytes(2200));
        assert_eq!(state.packets_sent, 2);
    }

    #[test]
    fn low_bitrate_cluster_completes_via_min_packet_count() {
        let now = Instant::now();
        // 100 kbps * 15ms = 187 bytes target - far less than 5 padding packets.
        // The byte target is a floor, not a ceiling: padding generation must
        // continue until the packet count is also met.
        let config = ProbeClusterConfig::new(1.into(), Bitrate::kbps(100), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        let mut t = now;
        for _ in 0..20 {
            if state.is_complete(t) {
                break;
            }
            if let Some(size) = state.next_packet(t) {
                state.record_packet(t, size);
            }
            let next = state.next_probe_time().max(t + config.min_probe_delta());
            t = next;
        }

        assert!(
            state.is_complete(t),
            "low-bitrate cluster must complete: {} packets, {:?} bytes",
            state.packets_sent,
            state.bytes_sent
        );
        assert!(state.packets_sent >= config.min_packet_count());
    }

    #[test]
    fn probe_padding_bursts_sized_toward_mtu() {
        let now = Instant::now();
        // At 500 kbps, rate * min_probe_delta is only 125 bytes. Padding
        // requests that small are served as blank padding packets (capped at
        // 255 bytes by the RTP padding format) instead of full-size spurious
        // resends, so the burst must be sized toward the MTU.
        let config = ProbeClusterConfig::new(1.into(), Bitrate::kbps(500), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        let burst = state.next_packet(now).expect("a first burst");

        assert!(
            burst >= DataSize::bytes(1200),
            "probe burst should be MTU-sized, got {:?}",
            burst
        );
    }

    #[test]
    fn stalled_cluster_times_out() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        // Not started: no timeout.
        assert!(!state.is_timed_out(now + Duration::from_secs(10)));

        state.record_packet(now, DataSize::bytes(1200));

        assert!(!state.is_timed_out(now + Duration::from_millis(500)));
        assert!(state.is_timed_out(now + Duration::from_millis(1100)));
        assert!(!state.is_complete(now + Duration::from_millis(1100)));
    }

    #[test]
    fn min_probe_delta_is_enforced_in_next_probe_time() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial)
            .with_min_probe_delta(Duration::from_millis(20));
        let mut state = ProbeClusterState::new(config);
        assert_eq!(state.config().min_probe_delta(), Duration::from_millis(20));

        // Start a burst at `now`, then verify that a second burst is blocked until >= now+20ms.
        assert!(state.next_packet(now).is_some());
        assert!(state.next_packet(now + Duration::from_millis(19)).is_none());
        assert!(state.next_packet(now + Duration::from_millis(20)).is_some());
        let next = state.last_padding_at;
        assert!(
            next >= now + Duration::from_millis(20),
            "next_probe_time {next:?} must be >= now + min_probe_delta"
        );
    }
}
