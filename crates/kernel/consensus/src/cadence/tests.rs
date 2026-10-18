use super::*;

const BLOCK: Duration = Duration::from_millis(250);

fn ms(millis: u64) -> Option<Duration> {
    Some(Duration::from_millis(millis))
}

#[test]
fn the_defaults_are_multiples_of_the_block_time() {
    let cadence = Cadence::new(BLOCK, Timeouts::default()).unwrap();
    assert_eq!(cadence.leader_timeout(), Duration::from_millis(250));
    assert_eq!(
        cadence.certification_timeout(),
        Duration::from_millis(7_500)
    );
    assert_eq!(cadence.timeout_retry(), Duration::from_millis(500));
    assert_eq!(cadence.skip_timeout(), Duration::from_millis(7_750));
    assert_eq!(cadence.fetch_timeout(), BLOCK);
}

#[test]
fn skip_and_fetch_follow_the_configured_timeouts() {
    let timeouts = Timeouts {
        leader: ms(2_000),
        certification: ms(3_000),
        retry: ms(1_000),
    };
    let cadence = Cadence::new(BLOCK, timeouts).unwrap();
    assert_eq!(cadence.leader_timeout(), Duration::from_millis(2_000));
    assert_eq!(
        cadence.certification_timeout(),
        Duration::from_millis(3_000)
    );
    assert_eq!(cadence.timeout_retry(), Duration::from_millis(1_000));
    assert_eq!(cadence.skip_timeout(), Duration::from_millis(3_250));
    assert_eq!(cadence.fetch_timeout(), BLOCK);
}

#[test]
fn an_unset_timeout_keeps_its_default() {
    let timeouts = Timeouts {
        leader: ms(400),
        ..Timeouts::default()
    };
    let cadence = Cadence::new(BLOCK, timeouts).unwrap();
    assert_eq!(cadence.leader_timeout(), Duration::from_millis(400));
    assert_eq!(
        cadence.certification_timeout(),
        Duration::from_millis(7_500)
    );
    assert_eq!(cadence.timeout_retry(), Duration::from_millis(500));
}

#[test]
fn invalid_timeouts_are_refused() {
    let refused = |block: Duration, timeouts: Timeouts| Cadence::new(block, timeouts);
    assert_eq!(
        refused(Duration::ZERO, Timeouts::default()),
        Err(CadenceError::BlockTimeZero)
    );
    assert_eq!(
        refused(
            BLOCK,
            Timeouts {
                leader: ms(0),
                ..Timeouts::default()
            }
        ),
        Err(CadenceError::LeaderZero)
    );
    assert_eq!(
        refused(
            BLOCK,
            Timeouts {
                leader: ms(8_000),
                ..Timeouts::default()
            }
        ),
        Err(CadenceError::CertificationNotAboveLeader {
            leader: 8_000,
            certification: 7_500
        })
    );
    assert_eq!(
        refused(
            BLOCK,
            Timeouts {
                leader: ms(3_000),
                certification: ms(3_000),
                ..Timeouts::default()
            }
        ),
        Err(CadenceError::CertificationNotAboveLeader {
            leader: 3_000,
            certification: 3_000
        })
    );
    assert_eq!(
        refused(
            BLOCK,
            Timeouts {
                retry: ms(0),
                ..Timeouts::default()
            }
        ),
        Err(CadenceError::RetryZero)
    );
    assert_eq!(
        refused(
            BLOCK,
            Timeouts {
                retry: ms(7_750),
                ..Timeouts::default()
            }
        ),
        Err(CadenceError::RetryNotBelowSkip {
            retry: 7_750,
            skip: 7_750
        })
    );
}
