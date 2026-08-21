use std::time::Instant;

use str0m::Rtc;
use str0m::media::{MediaKind, Mid};

mod common;
use common::{init_crypto_default, init_log};

#[test]
pub fn direct_declared_media_is_active() {
    init_log();
    init_crypto_default();

    let mut rtc = Rtc::new(Instant::now());

    let aud: Mid = "aud".into();
    let vid: Mid = "vid".into();

    rtc.direct_api().declare_media(aud, MediaKind::Audio);
    rtc.direct_api().declare_media(vid, MediaKind::Video);

    for (mid, kind) in [(aud, MediaKind::Audio), (vid, MediaKind::Video)] {
        let media = rtc.media(mid).expect("declared media");

        // In direct mode the application's codec config is the negotiation, so the
        // media must not read as disabled ("no codecs matched"). A disabled media
        // makes the session count as having no active outgoing media, which zeroes
        // the pacer's padding rate and starves BWE probing.
        assert!(
            !media.disabled(),
            "direct-API media {} must not be disabled",
            mid
        );

        // The seeded PTs are exactly the session's configured PTs for the kind,
        // in config order.
        let expected: Vec<_> = rtc
            .codec_config()
            .params()
            .iter()
            .filter(|p| p.spec().codec.is_audio() == kind.is_audio())
            .map(|p| p.pt())
            .collect();

        assert!(!expected.is_empty());
        assert_eq!(media.remote_pts(), expected);
    }
}
