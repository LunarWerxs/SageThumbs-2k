#![cfg(test)]

use super::{
    extract_url, interpret_response, mime_escape, parse_hosts_config, write_recovery_copy,
};

#[test]
fn upload_response_requires_an_exact_web_scheme() {
    assert_eq!(
        extract_url("https://files.example.test/a.png", false).as_deref(),
        Some("https://files.example.test/a.png")
    );
    assert_eq!(extract_url("httpx://files.example.test/a.png", false), None);
    assert_eq!(
        extract_url("javascript:https://files.example.test/a.png", false),
        None
    );
    assert_eq!(
        extract_url("https://files.example.test/a b.png", false),
        None
    );
    assert_eq!(
        extract_url(r#"{"url":"https:\/\/files.example.test\/a.png"}"#, true).as_deref(),
        Some("https://files.example.test/a.png")
    );
}

#[test]
fn upload_config_rejects_ambiguous_https_authorities() {
    let text = "\
https://good.example/upload | file | text
https://user@bad.example/upload | file | text
https://bad.example:8443/upload | file | text
https://bad example/upload | file | text
";
    let (hosts, rejected) = parse_hosts_config(text);
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].host, "good.example");
    assert_eq!(hosts[0].path, "/upload");
    assert_eq!(
        rejected.len(),
        3,
        "every unusable active line is reported: {rejected:?}"
    );
}

/// 2026-09-19 audit F22: an all-commented file means the built-ins; a file whose only
/// active line is unusable means NOTHING usable, and must never read as the built-ins.
#[test]
fn an_all_commented_file_and_an_all_invalid_file_are_told_apart() {
    let (hosts, rejected) = parse_hosts_config("# https://x.example/upload | file | text\n\n");
    assert!(hosts.is_empty() && rejected.is_empty());
    let (hosts, rejected) = parse_hosts_config("http://my-own-host.example/upload | file | text\n");
    assert!(hosts.is_empty());
    assert_eq!(
        rejected,
        vec!["http://my-own-host.example/upload | file | text"]
    );
}

#[test]
fn non_2xx_status_is_rejected_even_when_the_body_contains_a_url() {
    // A 4xx/5xx error page that happens to embed something URL-shaped (an ad
    // link, a status-page link, …) must never be reported to the user as their
    // own upload link — the status has to gate the body scrape, not the body alone.
    let body = b"<html>502 Bad Gateway. See https://status.example.test/incident/1</html>";
    let err = interpret_response(502, body, false).unwrap_err();
    assert!(
        err.contains("502"),
        "failure reason should surface the status code, got: {err}"
    );
}

/// A filename or extra-field value carrying a `"` or an embedded CR/LF must
/// not be able to break out of its `Content-Disposition` quoted string or inject a raw
/// header/multipart-boundary line into the body. NTFS refuses these through the ordinary
/// Win32 API, but the NT native API, a WSL mount, or a non-Windows SMB share can all
/// create a filename that carries them, and it reaches `upload_one` unchanged.
#[test]
fn mime_escape_neutralizes_quotes_and_line_breaks() {
    assert_eq!(mime_escape("plain.png"), "plain.png");

    let injected = "evil\".png\r\nContent-Disposition: form-data; name=\"x";
    let escaped = mime_escape(injected);
    assert!(
        !escaped.contains('\r') && !escaped.contains('\n'),
        "no CR/LF may survive — a raw one would let extra header/boundary lines through"
    );
    assert!(
        escaped.contains("evil\\\".png") && escaped.contains("name=\\\"x"),
        "the quote must survive, but only in escaped (backslash-preceded) form: {escaped}"
    );

    let escaped = mime_escape("a\\b\"c\r\nd");
    assert!(!escaped.contains('\r') && !escaped.contains('\n'));
    // Every remaining `"` must be preceded by a backslash (properly escaped), and every
    // backslash must itself have been doubled — otherwise a `\"` sequence produced by
    // escaping could be misread as an unescaped quote by the receiving parser.
    let mut chars = escaped.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            assert!(
                matches!(chars.next(), Some('\\') | Some('"')),
                "a lone backslash must not appear unescaped"
            );
        } else {
            assert_ne!(c, '"', "an unescaped quote must never survive");
        }
    }
}

#[test]
fn a_2xx_plain_reply_still_extracts_the_url() {
    assert_eq!(
        interpret_response(200, b"https://files.example.test/a.png", false).as_deref(),
        Ok("https://files.example.test/a.png")
    );
}

#[test]
fn an_unqueryable_status_is_treated_as_failure_not_success() {
    // query_status returns None (mapped to 0 by the caller) on a request WinInet
    // couldn't report a status for — that must not be silently treated as OK.
    let err = interpret_response(0, b"https://files.example.test/a.png", false).unwrap_err();
    assert!(err.contains('0'));
}

#[test]
fn upload_failure_recovery_writes_the_bytes_back_to_disk() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_upload_recovery_test_{}_{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let bytes = b"not a real png, just proving the bytes survive";

    let path = write_recovery_copy(&dir, bytes).expect("recovery write should succeed");

    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two recovery copies landing in the same second (the real trigger — a batch
/// upload where more than one file fails, or a recovery racing an ordinary Ctrl+S save)
/// used to write `dir.join(timestamped_name())` directly, so the second write silently
/// clobbered the first. Routing through `output::write_reserved` must give the second
/// call its own path and leave the first file's bytes intact.
#[test]
fn same_second_recovery_copies_do_not_clobber_each_other() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_upload_recovery_collision_{}_{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let first = b"first failed upload's bytes";
    let second = b"a second, different, failed upload's bytes";

    let p1 = write_recovery_copy(&dir, first).expect("first recovery write should succeed");
    let p2 = write_recovery_copy(&dir, second).expect("second recovery write should succeed");

    assert_ne!(p1, p2, "the second write must not collide with the first");
    assert_eq!(
        std::fs::read(&p1).unwrap(),
        first,
        "the first file must survive untouched"
    );
    assert_eq!(std::fs::read(&p2).unwrap(), second);
    let _ = std::fs::remove_dir_all(&dir);
}
