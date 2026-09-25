use super::*;

/// A made-up window handle: the slots only compare it, never use it.
const HWND_A: isize = 0x1234;

/// The whole handoff in one test, because every step shares the one process-wide slot and
/// parallel tests would race on it.
#[test]
fn a_check_result_crosses_the_post_once_and_a_forged_repeat_finds_nothing() {
    let found = update::LatestRelease {
        tag: "9.9.9".into(),
        published_unix: Some(1_700_000_000),
        security: true,
    };
    assert_eq!(
        post_code(HWND_A, update::UpdateCheck::Available(found.clone())),
        1
    );
    assert!(matches!(status_for_code(HWND_A, 1), Status::Available(r) if r == found));
    // A second "available" (forged by another process, or a repeat) takes an empty slot.
    assert!(matches!(
        status_for_code(HWND_A, 1),
        Status::Available(r) if r == update::LatestRelease::unknown()
    ));

    assert_eq!(post_code(HWND_A, update::UpdateCheck::UpToDate), 0);
    assert!(matches!(status_for_code(HWND_A, 0), Status::UpToDate));
    assert_eq!(post_code(HWND_A, update::UpdateCheck::Failed), 2);
    assert!(matches!(status_for_code(HWND_A, 2), Status::Failed));
    // Any code the worker never posts reads as up to date, not as an offer.
    assert!(matches!(status_for_code(HWND_A, 7), Status::UpToDate));
}

/// Two About windows (About, and Check for updates) keep their own results apart.
#[test]
fn each_about_window_takes_only_its_own_check_result() {
    let (a, b) = (0x5000_isize, 0x6000_isize);
    let release = |tag: &str| update::LatestRelease {
        tag: tag.into(),
        published_unix: None,
        security: false,
    };
    post_code(a, update::UpdateCheck::Available(release("1.0.0")));
    post_code(b, update::UpdateCheck::Available(release("2.0.0")));
    assert!(matches!(status_for_code(b, 1), Status::Available(r) if r.tag == "2.0.0"));
    assert!(matches!(status_for_code(a, 1), Status::Available(r) if r.tag == "1.0.0"));
}
