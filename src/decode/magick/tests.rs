#![cfg(test)]

use super::{
    add_magick_limits, add_metafile_magick_limits, apply_magick_environment, encode_wait_decision,
    magick_output_extensions, magick_output_supported, magick_stdin_spec, output_coder, EncodeWait,
    FULL_FIDELITY_PNG_CAP, MAGICK_CPU_BUDGET, MAGICK_PNG_CAP, MAX_ISOBMFF_TOP_LEVEL_BOXES,
    METAFILE_MAGICK_CPU_BUDGET, METAFILE_MAGICK_MAP_LIMIT, METAFILE_MAGICK_MEMORY_LIMIT,
    METAFILE_MAGICK_TIMEOUT, METAFILE_MAGICK_TIME_LIMIT,
};
use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

fn isobmff_box(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(8 + body.len()).unwrap();
    [&size.to_be_bytes()[..], &typ[..], body].concat()
}

fn ftyp_with_minor(
    major_brand: &[u8; 4],
    minor_version: &[u8; 4],
    compatible: &[[u8; 4]],
) -> Vec<u8> {
    let mut body = Vec::from(&major_brand[..]);
    body.extend_from_slice(minor_version);
    for brand in compatible {
        body.extend_from_slice(brand);
    }
    isobmff_box(b"ftyp", &body)
}

fn ftyp(major_brand: &[u8; 4], compatible: &[[u8; 4]]) -> Vec<u8> {
    ftyp_with_minor(major_brand, &[0; 4], compatible)
}

#[test]
fn mini_avif_uses_explicit_avif_stdin_spec() {
    let mut bytes = ftyp_with_minor(b"mif3", b"avif", &[]);
    bytes.extend(isobmff_box(b"mini", &[0x80, 0x01, 0xFE]));
    assert_eq!(magick_stdin_spec(&bytes), "avif:-");

    // A derived structural major brand may carry mif3 as compatible.
    let mut compatible = ftyp_with_minor(b"mif1", b"avif", &[*b"mif3"]);
    compatible.extend(isobmff_box(b"mini", &[0x80]));
    assert_eq!(magick_stdin_spec(&compatible), "avif:-");
}

#[test]
fn ordinary_avif_keeps_magick_auto_detection() {
    let mut bytes = ftyp(b"avif", &[*b"mif1"]);
    bytes.extend(isobmff_box(b"meta", &[0, 0, 0, 0]));
    assert_eq!(magick_stdin_spec(&bytes), "-");
}

#[test]
fn mini_stdin_routing_rejects_malformed_or_hostile_boxes() {
    // A `mini` byte sequence outside a checked top-level box is not enough.
    assert_eq!(magick_stdin_spec(b"not an avif mini"), "-");

    // The declared ftyp length extends beyond the buffer.
    assert_eq!(
        magick_stdin_spec(&[0, 0, 0, 32, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f']),
        "-"
    );

    // An extended-size box must have its complete 16-byte header and body.
    let mut truncated_extended = ftyp_with_minor(b"mif3", b"avif", &[]);
    truncated_extended.extend_from_slice(&[0, 0, 0, 1, b'm', b'i', b'n', b'i']);
    assert_eq!(magick_stdin_spec(&truncated_extended), "-");

    // Stop before an attacker-controlled run of arbitrarily many tiny boxes.
    let mut flooded = ftyp_with_minor(b"mif3", b"avif", &[]);
    for _ in 0..MAX_ISOBMFF_TOP_LEVEL_BOXES {
        flooded.extend(isobmff_box(b"free", &[]));
    }
    flooded.extend(isobmff_box(b"mini", &[0x80]));
    assert_eq!(magick_stdin_spec(&flooded), "-");
}

#[test]
fn non_avif_mini_keeps_magick_auto_detection() {
    let mut bytes = ftyp_with_minor(b"mif3", &[0; 4], &[]);
    bytes.extend(isobmff_box(b"mini", &[0x80]));
    assert_eq!(magick_stdin_spec(&bytes), "-");
}

#[test]
fn ftyp_minor_version_cannot_spoof_an_avif_brand() {
    let mut bytes = ftyp_with_minor(b"mif1", b"avif", &[]);
    // The AV1 codec signal alone is not enough without the mif3 structure.
    bytes.extend(isobmff_box(b"mini", &[0x80]));
    assert_eq!(magick_stdin_spec(&bytes), "-");
}

#[test]
fn every_advertised_magick_output_uses_an_explicit_coder() {
    let expected = [
        ("avif", "AVIF"),
        ("jxl", "JXL"),
        ("psd", "PSD"),
        ("dds", "DDS"),
        ("jp2", "JP2"),
        ("pcx", "PCX"),
        ("sgi", "SGI"),
        ("pfm", "PFM"),
        ("dpx", "DPX"),
        ("fits", "FITS"),
        ("xpm", "XPM"),
        ("pict", "PICT"),
        ("ras", "RAS"),
        ("palm", "PALM"),
    ];

    for (extension, coder) in expected {
        assert_eq!(output_coder(extension), Some(coder));
        assert_eq!(output_coder(&extension.to_ascii_uppercase()), Some(coder));
        assert!(magick_output_supported(extension));
        assert!(magick_output_supported(&extension.to_ascii_uppercase()));
    }

    assert_eq!(output_coder(""), None);
    assert_eq!(output_coder("png"), None);
    assert_eq!(output_coder("not-a-real-format"), None);
    assert!(!magick_output_supported(""));
    assert!(!magick_output_supported("png"));
    assert!(!magick_output_supported("not-a-real-format"));

    let extensions = magick_output_extensions();
    assert_eq!(
        extensions.len(),
        expected.len(),
        "magick_output_extensions() drifted from the coder table"
    );
    for (extension, _) in expected {
        assert!(
            extensions.contains(&extension),
            "{extension} missing from magick_output_extensions()"
        );
    }
}

#[test]
fn metafile_limits_override_the_shared_magick_budget() {
    let mut command = Command::new("magick.exe");
    add_magick_limits(&mut command, crate::decode::MAGICK_TIMEOUT);
    add_metafile_magick_limits(&mut command);
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        args,
        [
            "-limit",
            "memory",
            "512MiB",
            "-limit",
            "map",
            "1GiB",
            "-limit",
            "time",
            "120",
            "-limit",
            "memory",
            METAFILE_MAGICK_MEMORY_LIMIT,
            "-limit",
            "map",
            METAFILE_MAGICK_MAP_LIMIT,
            "-limit",
            "time",
            METAFILE_MAGICK_TIME_LIMIT,
        ]
    );
    // Metafiles keep their much tighter CPU budget; only the elapsed-time backstop is
    // widened, so a busy machine cannot fail a metafile that needed 0.1 s of real work.
    assert_eq!(
        METAFILE_MAGICK_CPU_BUDGET,
        std::time::Duration::from_secs(3)
    );
    assert_eq!(METAFILE_MAGICK_TIMEOUT, std::time::Duration::from_secs(18));
    assert_eq!(
        METAFILE_MAGICK_TIME_LIMIT.parse::<u64>().unwrap(),
        METAFILE_MAGICK_TIMEOUT.as_secs(),
        "magick's own elapsed limit must match the metafile wall backstop",
    );
}

#[test]
fn magick_command_is_pinned_to_its_own_module_tree() {
    let root = std::env::temp_dir().join(format!(
        "st2k-magick-env-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("modules").join("coders")).unwrap();
    std::fs::create_dir_all(root.join("modules").join("filters")).unwrap();
    std::fs::write(root.join("policy.xml"), b"<policymap/>").unwrap();
    let exe = root.join("magick.exe");
    let coders = root.join("modules").join("coders");
    let filters = root.join("modules").join("filters");

    let mut command = Command::new(&exe);
    apply_magick_environment(&mut command, &exe);
    let environment: HashMap<_, _> = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect();

    assert_eq!(
        environment
            .get(std::ffi::OsStr::new("MAGICK_HOME"))
            .map(std::ffi::OsString::as_os_str),
        Some(root.as_os_str())
    );
    assert_eq!(
        environment
            .get(std::ffi::OsStr::new("MAGICK_CODER_MODULE_PATH"))
            .map(std::ffi::OsString::as_os_str),
        Some(coders.as_os_str())
    );
    assert_eq!(
        environment
            .get(std::ffi::OsStr::new("MAGICK_FILTER_MODULE_PATH"))
            .map(std::ffi::OsString::as_os_str),
        Some(filters.as_os_str())
    );
    assert_eq!(
        environment
            .get(std::ffi::OsStr::new("MAGICK_CONFIGURE_PATH"))
            .map(std::ffi::OsString::as_os_str),
        Some(root.as_os_str())
    );

    let _ = std::fs::remove_dir_all(root);
}

/// A starved-but-alive encode child (near-zero CPU burned, wall deadline still far
/// off) must trip on CPU budget, not just coast until the wall ceiling. Before this
/// branch existed, `encode_via_magick`'s wait loop had no CPU check at all, so this
/// case fell through to `EncodeWait::Continue` regardless of `cpu`.
#[test]
fn encode_wait_trips_cpu_budget_before_the_wall_deadline() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(600); // wall ceiling nowhere close
    let decision = encode_wait_decision(
        Some(MAGICK_CPU_BUDGET + Duration::from_millis(1)),
        MAGICK_CPU_BUDGET,
        now,
        deadline,
    );
    assert_eq!(decision, EncodeWait::CpuExceeded);
}

#[test]
fn encode_wait_keeps_polling_a_busy_but_within_budget_child() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(600);
    let decision = encode_wait_decision(
        Some(Duration::from_millis(1)),
        MAGICK_CPU_BUDGET,
        now,
        deadline,
    );
    assert_eq!(decision, EncodeWait::Continue);
}

#[test]
fn encode_wait_falls_back_to_the_wall_ceiling_when_cpu_time_is_unknown() {
    // `child_cpu_time` returns `None` when the OS won't say (see its own doc comment);
    // the loop must still fail closed via the wall deadline rather than spin forever.
    let now = Instant::now();
    let deadline = now - Duration::from_millis(1); // already past
    assert_eq!(
        encode_wait_decision(None, MAGICK_CPU_BUDGET, now, deadline),
        EncodeWait::TimedOut
    );
}

/// The decode and encode magick harnesses now cap their stdout reads the same way
/// `flv.rs`'s sibling child harness caps its own (`FLASH_PNG_CAP`) — this pins the
/// value so it can't silently drift below what `-resize {MAGICK_MAX_EDGE_PX}x...>`
/// can legitimately produce.
#[test]
fn magick_png_cap_covers_the_geometry_ceiling() {
    const MAGICK_MAX_EDGE_PX: u64 = 4096;
    let worst_case_raw_rgba = MAGICK_MAX_EDGE_PX * MAGICK_MAX_EDGE_PX * 4;
    assert!(
        MAGICK_PNG_CAP as u64 >= worst_case_raw_rgba,
        "MAGICK_PNG_CAP must cover a full {MAGICK_MAX_EDGE_PX}x{MAGICK_MAX_EDGE_PX} RGBA frame"
    );
}

/// The full-fidelity cap must comfortably cover the MEASURED hand-back that broke the
/// native RAW path: the bundled Q16 magick writes 16-BIT PNGs, and a 21 MP Mamiya `.mef`
/// at native size is 107 MB — silently "failing" under the 64 MiB tier cap, so the whole
/// feature shipped and did nothing. Pinned at 2x that so a merely-bigger camera does not
/// re-open the same hole one model later.
#[test]
fn full_fidelity_png_cap_covers_the_measured_native_raw() {
    let measured_mef_native_png: usize = 107_389_077;
    assert!(
        FULL_FIDELITY_PNG_CAP >= 2 * measured_mef_native_png,
        "FULL_FIDELITY_PNG_CAP must cover a native medium-format 16-bit PNG with headroom"
    );
}

/// A Convert target whose file name or full path is too long for ImageMagick's raw,
/// unprefixed `coder:path` spec must be refused before spawning, not silently
/// truncated by the OS.
#[test]
fn encode_target_length_is_refused_past_windows_limits() {
    let dir = std::env::temp_dir();

    assert!(super::encode_target_length_ok(&dir.join("thumbnail.dds")));

    let long_name = format!("{}.dds", "a".repeat(300));
    assert!(!super::encode_target_length_ok(&dir.join(long_name)));

    let mut deep = dir.clone();
    for _ in 0..5000 {
        deep = deep.join("segment");
    }
    deep = deep.join("thumbnail.dds");
    assert!(!super::encode_target_length_ok(&deep));
}
