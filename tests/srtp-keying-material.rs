use std::time::Duration;

use str0m::crypto::dtls::SrtpProfile;
use str0m::media::{MediaKind, Mid};
use str0m::rtp::{RtpHeader, RtpWrite, Ssrc};
use str0m::{Rtc, RtcError};

mod common;
use common::{connect_l_r, init_crypto_default, init_log, progress};

/// The keying material accessor: `None` before DTLS completes, the same
/// exporter output on both sides afterwards with opposite roles, and
/// retained for the life of the session (a later call still answers).
#[test]
pub fn srtp_keying_material_is_exported_after_dtls() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let mut fresh = Rtc::new(std::time::Instant::now());
    assert!(fresh.direct_api().srtp_keying_material().is_none());

    let (mut l, mut r) = connect_l_r();

    let km_l = l
        .direct_api()
        .srtp_keying_material()
        .expect("material after DTLS")
        .clone();
    let km_r = r
        .direct_api()
        .srtp_keying_material()
        .expect("material after DTLS")
        .clone();
    assert_eq!(
        km_l.material, km_r.material,
        "one exporter output, both sides"
    );
    assert_eq!(km_l.profile, km_r.profile);
    assert!(km_l.active, "L started DTLS active");
    assert!(!km_r.active, "R started DTLS passive");
    let (key_len, salt_len) = match km_l.profile {
        SrtpProfile::AES128_CM_SHA1_80 => (16, 14),
        SrtpProfile::AEAD_AES_128_GCM => (16, 12),
        SrtpProfile::AEAD_AES_256_GCM => (32, 12),
        _ => panic!("unexpected profile"),
    };
    assert_eq!(km_l.material.len(), 2 * (key_len + salt_len));

    // Sequence-number getters follow the streams.
    let mid: Mid = "aud".into();
    let ssrc: Ssrc = 42.into();
    l.direct_api().declare_media(mid, MediaKind::Audio);
    r.direct_api().declare_media(mid, MediaKind::Audio);
    l.direct_api().declare_stream_tx(ssrc, None, mid, None);
    r.direct_api().expect_stream_rx(ssrc, None, mid, None);
    assert!(
        l.direct_api()
            .stream_tx(&ssrc)
            .unwrap()
            .last_sent_seq_no()
            .is_none()
    );
    assert!(
        r.direct_api()
            .stream_rx(&ssrc)
            .unwrap()
            .max_seq_no()
            .is_none()
    );

    let pt = l.params_opus().pt();
    for i in 0..5u16 {
        let wallclock = l.start + l.duration();
        let mut api = l.direct_api();
        let tx = api.stream_tx(&ssrc).unwrap();
        let header = RtpHeader {
            payload_type: pt,
            sequence_number: 65533u16.wrapping_add(i),
            timestamp: i as u32 * 960,
            ssrc,
            ..Default::default()
        };
        let seq_no = (65533u64 + i as u64).into();
        tx.write_rtp(RtpWrite::new(
            pt,
            seq_no,
            header.timestamp,
            wallclock,
            vec![0xd5; 40],
        ));
        drop(api);
        for _ in 0..4 {
            progress(&mut l, &mut r)?;
        }
        let _ = header;
    }
    l.set_forced_time_advance(Duration::from_millis(20));
    for _ in 0..20 {
        progress(&mut l, &mut r)?;
    }
    let sent = l
        .direct_api()
        .stream_tx(&ssrc)
        .unwrap()
        .last_sent_seq_no()
        .expect("sent");
    assert_eq!(*sent, 65537, "the last of five from 65533, extended");
    assert_eq!(sent.roc(), 1, "one wrap");
    let got = r
        .direct_api()
        .stream_rx(&ssrc)
        .unwrap()
        .max_seq_no()
        .expect("received");
    assert_eq!(*got, 65537);
    assert_eq!(got.roc(), 1);

    // Still there long after the handshake.
    assert!(l.direct_api().srtp_keying_material().is_some());
    Ok(())
}
