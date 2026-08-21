//! Probing toward an unmet desired bitrate outside ALR.

use std::time::Duration;

use netem::{DataSize, NetemConfig};
use str0m::RtcError;
use str0m::bwe::Bitrate;

use crate::common::{BweTestContext, Step, connect_with_bwe, init_crypto_default, init_log};

/// A constant-rate sender (an SFU forwarding a fixed-rate layer) on a capped
/// link, wanting more than the link provides. The delay estimator ceilings
/// the estimate at ~1.5x the flowing rate, utilization floors just above the
/// ALR threshold, so without unmet-desired probing no probe ever fires: when
/// the cap lifts, the estimate stays wedged at the capped value and the
/// application can never step up to the rate it wants (which costs ~2x, more
/// than the ~1.5x ceiling grants).
#[test]
fn unmet_desired_recovers_after_link_lift() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let plan = vec![
        Step::Conditions {
            description: "Capped link (delay-limited, no forced loss)",
            config: NetemConfig::new()
                .link(Bitrate::kbps(280), DataSize::kbytes(40))
                .seed(42),
        },
        Step::Media {
            description: "Constant-rate forwarder wanting more than the link",
            desired_bitrate: Bitrate::kbps(750),
            media_send_rate: Bitrate::kbps(250),
        },
        Step::Run {
            description: "Converge on the capped link",
            duration: Duration::from_secs(12),
        },
        Step::Check {
            description: "Estimate carries the flowing rate despite the cap",
            at_least: Bitrate::kbps(250),
        },
        Step::Conditions {
            description: "Cap lifts",
            config: NetemConfig::new()
                .link(Bitrate::mbps(10), DataSize::kbytes(200))
                .seed(43),
        },
        Step::Run {
            description: "Unmet-desired probes climb toward the desired rate",
            duration: Duration::from_secs(16),
        },
        Step::CheckPeak {
            description: "Estimate reaches the desired rate without the send rate rising",
            at_least: Bitrate::kbps(750),
        },
    ];

    let (mut l, mut r) = connect_with_bwe(Bitrate::kbps(300), Bitrate::kbps(750));
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.run_plan(&mut l, &mut r, &plan)?;

    Ok(())
}
