use std::time::Duration;

use netem::{NetemConfig, Probability, RandomLoss};
use str0m::{Event, IceConnectionState, RtcError};

mod common;
use common::{TestRtc, connect_l_r, init_crypto_default, init_log, progress};

fn ice_states(t: &TestRtc, since: usize) -> Vec<IceConnectionState> {
    t.events[since..]
        .iter()
        .filter_map(|(_, e)| match e {
            Event::IceConnectionStateChange(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn run_for(l: &mut TestRtc, r: &mut TestRtc, secs: u64) -> Result<(), RtcError> {
    let until = l.duration() + Duration::from_secs(secs);
    while l.duration() < until {
        progress(l, r)?;
    }
    Ok(())
}

/// A connected pair whose path dies: both agents report `Disconnected`
/// once their checks go unanswered and drop every pair. The path comes
/// back — and nothing happens, because neither side has a pair left to
/// check on. `DirectApi::recreate_candidate_pairs` on one side resumes
/// the checks from the candidates both still hold; the other side
/// answers, forms its pair from the request, and both are `Connected`
/// again on the same credentials — no ICE restart, DTLS untouched.
#[test]
pub fn recreate_candidate_pairs_reconnects_after_an_outage() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let (mut l, mut r) = connect_l_r();
    assert!(l.is_connected() && r.is_connected());

    // The outage: every datagram in both directions is lost.
    let dead = || NetemConfig::new().loss(RandomLoss::new(Probability::new(1.0)));
    l.set_netem(dead());
    r.set_netem(dead());
    let (l_from, r_from) = (l.events.len(), r.events.len());
    run_for(&mut l, &mut r, 40)?;
    assert!(
        ice_states(&l, l_from).contains(&IceConnectionState::Disconnected),
        "L gave up during the outage: {:?}",
        ice_states(&l, l_from)
    );
    assert!(
        ice_states(&r, r_from).contains(&IceConnectionState::Disconnected),
        "R gave up during the outage: {:?}",
        ice_states(&r, r_from)
    );

    // The path returns. Nobody checks any more: no pair survived.
    l.set_netem(NetemConfig::new());
    r.set_netem(NetemConfig::new());
    let (l_from, r_from) = (l.events.len(), r.events.len());
    run_for(&mut l, &mut r, 10)?;
    assert!(
        ice_states(&l, l_from).is_empty() && ice_states(&r, r_from).is_empty(),
        "nothing reconnects by itself: L {:?} R {:?}",
        ice_states(&l, l_from),
        ice_states(&r, r_from)
    );
    assert!(!l.is_connected(), "L stays disconnected");

    // One side probes again.
    l.rtc.direct_api().recreate_candidate_pairs();
    let (l_from, r_from) = (l.events.len(), r.events.len());
    run_for(&mut l, &mut r, 10)?;
    assert!(
        ice_states(&l, l_from).iter().any(|s| s.is_connected()),
        "L reconnected: {:?}",
        ice_states(&l, l_from)
    );
    assert!(
        ice_states(&r, r_from).iter().any(|s| s.is_connected()),
        "R reconnected: {:?}",
        ice_states(&r, r_from)
    );
    assert!(l.is_connected() && r.is_connected());

    Ok(())
}
