use super::*;

/// The whole handoff in one test, because every step shares the one process-wide slot and
/// parallel tests would race on it.
#[test]
fn a_check_result_crosses_the_post_once_and_a_forged_repeat_finds_nothing() {
    let found = update::LatestRelease {
        tag: "9.9.9".into(),
        published_unix: Some(1_700_000_000),
        security: true,
    };
    assert_eq!(post_code(update::UpdateCheck::Available(found.clone())), 1);
    assert!(matches!(status_for_code(1), Status::Available(r) if r == found));
    // A second "available" (forged by another process, or a repeat) takes an empty slot.
    assert!(matches!(
        status_for_code(1),
        Status::Available(r) if r == update::LatestRelease::unknown()
    ));

    assert_eq!(post_code(update::UpdateCheck::UpToDate), 0);
    assert!(matches!(status_for_code(0), Status::UpToDate));
    assert_eq!(post_code(update::UpdateCheck::Failed), 2);
    assert!(matches!(status_for_code(2), Status::Failed));
    // Any code the worker never posts reads as up to date, not as an offer.
    assert!(matches!(status_for_code(7), Status::UpToDate));
}
