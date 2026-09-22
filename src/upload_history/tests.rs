#![cfg(test)]

//! Unit tests for the upload history: the line format, the newest-first read, the trim, and
//! the remaining-time words.

use super::*;

fn entry(uploaded: u64, expires: Expiry, url: &str) -> Entry {
    Entry {
        uploaded,
        expires,
        host: "litterbox.catbox.moe".to_string(),
        url: url.to_string(),
        name: "shot.png".to_string(),
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("st2k-upload-history-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join(FILE_NAME)
}

#[test]
fn a_line_round_trips_every_expiry_kind() {
    for expires in [Expiry::Never, Expiry::Unknown, Expiry::At(1_758_000_000)] {
        let e = entry(1_757_000_000, expires, "https://litter.catbox.moe/abc.png");
        assert_eq!(parse_line(&format_line(&e)), Some(e));
    }
}

#[test]
fn control_characters_cannot_split_a_line_or_shift_its_columns() {
    let mut e = entry(1, Expiry::Never, "https://x0.at/a.png");
    e.name = "evil\tname\r\nwith breaks.png".to_string();
    let line = format_line(&e);
    assert!(!line.contains(['\r', '\n']));
    assert_eq!(
        line.matches('\t').count(),
        4,
        "exactly five columns: {line:?}"
    );
    let back = parse_line(&line).expect("parses");
    assert_eq!(back.name, "evil name  with breaks.png");
    assert_eq!(back.url, e.url);
}

#[test]
fn malformed_lines_are_skipped_not_fatal() {
    let text = "# header\n\
                garbage\n\
                1\tsoon\thost\thttps://a.b/c\tn\n\
                1\t2\thost\tjavascript:alert(1)\tn\n\
                1\t2\thost\n\
                \n\
                5\tnever\tcatbox.moe\thttps://files.catbox.moe/x.png\tx.png\r\n";
    let got = parse(text);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].url, "https://files.catbox.moe/x.png");
    assert_eq!(
        got[0].name, "x.png",
        "a CRLF line ending is not part of the name"
    );
}

#[test]
fn a_line_without_a_name_still_parses() {
    let e = parse_line("7\tunknown\tyour.host\thttps://your.host/f").expect("parses");
    assert_eq!(e.name, "");
    assert_eq!(e.expires, Expiry::Unknown);
}

#[test]
fn parse_reads_newest_first_and_keeps_at_most_keep() {
    let mut text = String::new();
    for i in 0..(KEEP as u64 + 7) {
        let e = entry(i, Expiry::Never, &format!("https://x0.at/{i}.png"));
        text.push_str(&format_line(&e));
        text.push('\n');
    }
    let got = parse(&text);
    assert_eq!(got.len(), KEEP);
    assert_eq!(got[0].uploaded, KEEP as u64 + 6, "newest first");
    assert_eq!(got[KEEP - 1].uploaded, 7, "the oldest seven fell off");
}

#[test]
fn record_appends_behind_a_header_and_load_reads_it_back() {
    let path = scratch("record");
    let a = entry(10, Expiry::At(20), "https://litter.catbox.moe/a.png");
    let b = entry(11, Expiry::Never, "https://files.catbox.moe/b.png");
    assert!(record_at(&path, &a));
    assert!(record_at(&path, &b));
    let text = std::fs::read_to_string(&path).expect("written");
    assert!(text.starts_with('#'), "header first: {text:?}");
    assert_eq!(text.matches('#').count(), 1, "the header is written once");
    assert_eq!(load_at(&path), vec![b, a]);
    let _ = std::fs::remove_dir_all(path.parent().expect("has a dir"));
}

#[test]
fn record_trims_to_the_newest_keep_once_the_file_doubles() {
    let path = scratch("trim");
    for i in 0..(2 * KEEP as u64 + 1) {
        assert!(record_at(
            &path,
            &entry(i, Expiry::Never, &format!("https://x0.at/{i}.png"))
        ));
    }
    let text = std::fs::read_to_string(&path).expect("written");
    // The 2*KEEP-th entry makes the file header + 2*KEEP lines, which trims it to the header
    // + KEEP; the last entry is then appended behind that.
    assert_eq!(text.lines().count(), KEEP + 2, "trimmed, then appended");
    let got = load_at(&path);
    assert_eq!(got.len(), KEEP);
    assert_eq!(got[0].uploaded, 2 * KEEP as u64, "the newest survived");
    let _ = std::fs::remove_dir_all(path.parent().expect("has a dir"));
}

#[test]
fn load_of_a_missing_file_is_empty() {
    assert!(load_at(&scratch("missing")).is_empty());
}

#[test]
fn status_counts_down_then_expires() {
    let e = entry(100, Expiry::At(200), "https://litter.catbox.moe/a.png");
    assert_eq!(e.status(150), Status::Left(50));
    assert!(e.is_live(199));
    assert_eq!(e.status(200), Status::Expired(200));
    assert!(!e.is_live(200));
    assert_eq!(
        entry(1, Expiry::Never, "https://a.b/c").status(u64::MAX),
        Status::Permanent
    );
    assert!(entry(1, Expiry::Unknown, "https://a.b/c").is_live(u64::MAX));
}

#[test]
fn expiry_follows_the_hosts_retention() {
    assert_eq!(
        Expiry::from_retention(Retention::Secs(3600), 1000),
        Expiry::At(4600)
    );
    assert_eq!(
        Expiry::from_retention(Retention::Permanent, 1000),
        Expiry::Never
    );
    assert_eq!(
        Expiry::from_retention(Retention::Unknown, 1000),
        Expiry::Unknown
    );
    assert_eq!(
        Expiry::from_retention(Retention::Secs(u64::MAX), 1),
        Expiry::At(u64::MAX)
    );
}

#[test]
fn duration_text_uses_the_two_largest_units() {
    let d = |s: u64| duration_text(s, &ENGLISH);
    assert_eq!(d(72 * 3600), "3 d", "a fresh 72-hour upload");
    assert_eq!(d(72 * 3600 - 30), "3 d", "minutes round up");
    assert_eq!(d(71 * 3600), "2 d 23 h");
    assert_eq!(d(3 * 3600), "3 h");
    assert_eq!(d(2 * 3600 + 10 * 60), "2 h 10 min");
    assert_eq!(d(4 * 60), "4 min");
    assert_eq!(d(20), "1 min", "never 0 min");
    assert_eq!(d(0), "1 min");
    assert_eq!(d(100 * 86_400), "100 d");
}

#[test]
fn duration_text_fills_a_translated_template() {
    let ja = DurationWords {
        days: "{d}日",
        days_hours: "{d}日{h}時間",
        hours: "{h}時間",
        hours_minutes: "{h}時間{m}分",
        minutes: "{m}分",
    };
    assert_eq!(duration_text(28 * 3600, &ja), "1日4時間");
}

#[test]
fn local_datetime_has_the_info_card_shape() {
    let s = local_datetime(1_758_000_000);
    assert_eq!(s.len(), 16, "{s}");
    assert_eq!(&s[4..5], "-");
    assert_eq!(&s[10..11], " ");
    assert_eq!(&s[13..14], ":");
    assert!(s.starts_with("2025-09-1"), "{s}");
}

#[test]
fn status_text_en_says_each_state() {
    let now = 1_000_000;
    let live = entry(now, Expiry::At(now + 3 * 3600), "https://uguu.se/a.png");
    assert!(
        status_text_en(&live, now).ends_with("(in 3 h)"),
        "{}",
        status_text_en(&live, now)
    );
    assert!(status_text_en(&live, now + 4 * 3600).starts_with("expired "));
    assert_eq!(
        status_text_en(&entry(1, Expiry::Never, "https://a.b/c"), now),
        "no expiry date"
    );
    assert!(status_text_en(&entry(1, Expiry::Unknown, "https://a.b/c"), now).contains("unknown"));
}
