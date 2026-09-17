//! Verifies the Options settings actually gate the real thumbnail provider,
//! driven in-process exactly like Explorer (DllGetClassObject → CreateInstance
//! → Initialize(IStream) → GetThumbnail) against the freshly-built cdylib.
//!
//! HERMETIC: instead of mutating the developer's live `HKCU\Software\SageThumbs2K`
//! (the old approach — it save/restored, but a panic mid-test could leak a changed
//! EnableThumbs/MaxSize onto the box), this redirects the DLL's settings reads to a
//! THROWAWAY subkey via `ST2K_SETTINGS_ROOT` (honored by `settings::hkcu_root`). The
//! test writes EnableThumbs/MaxSize into that scratch key and reads the provider's
//! response; the user's real settings are never touched. The scratch key is wiped at
//! the start (clean slate) and end. Redirection makes it safe to run in the normal
//! suite, so it's no longer `#[ignore]`d.
//!
//! Run via `scripts/test.ps1` (build before test) so LoadLibrary gets a fresh cdylib —
//! plain `cargo test` does not refresh target/<profile>/*.dll.
#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
use windows::Win32::System::Com::{
    CoInitializeEx, IClassFactory, IStream, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IThumbnailProvider, SHCreateMemStream, WTSAT_UNKNOWN, WTS_ALPHATYPE,
};
use windows_registry::CURRENT_USER;

const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);
/// Throwaway HKCU subkey the DLL's settings reads are redirected to (via ST2K_SETTINGS_ROOT),
/// so this test never touches the developer's real `Software\SageThumbs2K` values. A child of
/// the real root, but isolated: creating/removing it leaves the parent's own values alone.
const TEST_ROOT: &str = r"Software\SageThumbs2K\__test_gate";

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// Run a full GetThumbnail handshake on `bytes`; Ok means a thumbnail was
/// produced, Err means the provider declined (disabled / oversized / undecodable).
unsafe fn get_thumbnail(bytes: &[u8], cx: u32) -> Result<()> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let path = common::dll_path();
    // The cdylib is a build artifact that MUST be present for this in-process
    // probe — there is no meaningful "skip" here. If it's missing the harness
    // built the test but not the DLL (e.g. `cargo test` without first running
    // `cargo build` in the same profile), so PANIC loudly rather than letting a missing
    // artifact look like a pass.
    assert!(
        path.exists(),
        "cdylib not built at {path:?} — run `cargo build` in this test profile first (this is a \
         build-artifact precondition, not an environment skip)"
    );
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(
        &CLSID_THUMBNAIL_PROVIDER,
        &IClassFactory::IID,
        &mut factory_ptr,
    )
    .ok()?;
    let factory = IClassFactory::from_raw(factory_ptr);
    let init: IInitializeWithStream = factory.CreateInstance(None)?;
    let stream: IStream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    init.Initialize(&stream, 0)?;
    let provider: IThumbnailProvider = init.cast()?;
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
    provider.GetThumbnail(cx, &mut hbmp, &mut alpha)?;
    if !hbmp.is_invalid() {
        let _ = DeleteObject(hbmp.into());
    }
    Ok(())
}

/// The same handshake as [`get_thumbnail`], but over a stream with a BACKING FILE, bound the
/// way Explorer binds it.
///
/// The point is to find out what the provider can actually learn about the file from such a
/// stream. Answer, measured rather than assumed: only its LEAF NAME — see the note on
/// [`oversized_file_backed_stream_is_rescued`].
unsafe fn get_thumbnail_from_file(path: &std::path::Path, cx: u32) -> Result<()> {
    use windows::Win32::UI::Shell::{BHID_Stream, IShellItem, SHCreateItemFromParsingName};

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let dll = common::dll_path();
    assert!(dll.exists(), "cdylib not built at {dll:?}");
    let wide: Vec<u16> = dll
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(
        &CLSID_THUMBNAIL_PROVIDER,
        &IClassFactory::IID,
        &mut factory_ptr,
    )
    .ok()?;
    let factory = IClassFactory::from_raw(factory_ptr);
    let init: IInitializeWithStream = factory.CreateInstance(None)?;

    // `BHID_Stream` off an `IShellItem` is the object Explorer itself hands the provider, so
    // this is the real thing rather than a stand-in built with `SHCreateStreamOnFileEx`.
    // Both, as it turns out, report only a leaf name from `Stat`.
    let file_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(file_wide.as_ptr()), None)?;
    let stream: IStream = item.BindToHandler(None, &BHID_Stream)?;
    init.Initialize(&stream, 0)?;
    let provider: IThumbnailProvider = init.cast()?;
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
    provider.GetThumbnail(cx, &mut hbmp, &mut alpha)?;
    if !hbmp.is_invalid() {
        let _ = DeleteObject(hbmp.into());
    }
    Ok(())
}

fn encode(img: RgbaImage, fmt: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), fmt)
        .unwrap();
    bytes
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
    }
    img
}

/// Write a DWORD into the throwaway settings root the DLL is redirected to. No save/restore:
/// the whole key is scratch and gets wiped at the end, so we just set what each step needs.
fn put(name: &str, value: u32) {
    CURRENT_USER
        .create(TEST_ROOT)
        .unwrap()
        .set_u32(name, value)
        .unwrap();
}

/// Delete the throwaway settings key (idempotent). The user's real `Software\SageThumbs2K`
/// values live directly under the parent and are untouched by removing this child subtree.
fn reset_scratch() {
    let _ = CURRENT_USER.remove_tree(TEST_ROOT);
}

/// Every test in this file drives the SAME scratch settings key, and it cannot be otherwise:
/// `settings::hkcu_root` caches `ST2K_SETTINGS_ROOT` once per process, so a second root would
/// simply be ignored. Cargo runs tests in threads, so without this they interleave — one
/// test's `MaxSize = 1` lands while the other is asserting under `MaxSize = 100`, and the
/// failure looks like a real provider bug. (It did: this appeared as an intermittent failure
/// the moment a second test joined the file.)
static SETTINGS_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the lock, tolerating a previous test having panicked while holding it.
fn lock_settings() -> std::sync::MutexGuard<'static, ()> {
    SETTINGS_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Point the DLL's business-licence reads at a scratch home and force Business mode, so the
/// lock (`licence_state::shell_locked`) can be exercised with no HKLM write and no touch of
/// the real `%ProgramData%` breadcrumb. Both variables are read ONCE per process by the DLL
/// (exactly like `ST2K_SETTINGS_ROOT`), so EVERY test in this file sets them before its first
/// `get_thumbnail`, whichever test happens to load the DLL first. An empty scratch home reads
/// as "not locked", so the tests that never write a breadcrumb see the same provider they
/// always did.
fn redirect_licence_to_scratch() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("st2k_gate_licence_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    unsafe {
        common::set_test_env("ST2K_LICENCE_HOME", &home);
        common::set_test_env("ST2K_LICENCE_MODE", "business");
    }
    home
}

#[test]
fn settings_gate_the_provider() {
    let _serial = lock_settings();
    // Redirect the DLL's settings reads to the scratch key BEFORE it's loaded — the first
    // get_thumbnail LoadLibrary's the cdylib, whose `settings::hkcu_root` caches this env var
    // once. Only the ROOT PATH is cached; the provider still re-reads the VALUES per
    // GetThumbnail (see settings::thumb_settings), so flipping them between calls takes effect.
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_ROOT) };
    redirect_licence_to_scratch();
    reset_scratch(); // clean slate — no stale values from a prior aborted run

    let small = encode(solid(80, 60, [10, 200, 30, 255]), ImageFormat::Png);
    // Uncompressed BMP that is comfortably over 1 MB but under the 100 MB default.
    let big = encode(solid(700, 700, [120, 60, 200, 255]), ImageFormat::Bmp);
    assert!(
        big.len() > 1024 * 1024,
        "BMP fixture should exceed 1 MB, got {}",
        big.len()
    );

    // --- EnableThumbs gate ---
    put("EnableThumbs", 1);
    let enabled = unsafe { get_thumbnail(&small, 64) };
    put("EnableThumbs", 0);
    let disabled = unsafe { get_thumbnail(&small, 64) };

    // --- MaxSize gate (EnableThumbs back on for this part) ---
    put("EnableThumbs", 1);
    put("MaxSize", 100);
    let under_limit = unsafe { get_thumbnail(&big, 64) };
    put("MaxSize", 1);
    let over_limit = unsafe { get_thumbnail(&big, 64) };

    reset_scratch(); // drop the throwaway key; the user's real settings were never touched

    assert!(enabled.is_ok(), "EnableThumbs=1 should produce a thumbnail");
    assert!(disabled.is_err(), "EnableThumbs=0 should decline (E_FAIL)");
    assert!(
        under_limit.is_ok(),
        "a ~1.9 MB file under a 100 MB MaxSize should thumbnail"
    );
    assert!(
        over_limit.is_err(),
        "the same file over a 1 MB MaxSize should be skipped"
    );
}

/// Time OUR provider on one file, through the real COM handshake. `#[ignore]`d and driven by
/// an env var because it needs a fixture the repo does not ship (a large video, say):
///
/// ```text
/// $env:ST2K_TIME_FILE = 'D:\big.mp4'
/// cargo test --test settings_gate time_one_file -- --ignored --nocapture
/// ```
///
/// Exists because timing a thumbnail through EXPLORER cannot tell our code from Windows' own
/// handler for the same format, so it can never answer "is OUR path fast". This can.
#[test]
#[ignore = "needs ST2K_TIME_FILE; a measurement tool, not a gate"]
fn time_one_file() {
    let _serial = lock_settings();
    redirect_licence_to_scratch();
    let Ok(path) = std::env::var("ST2K_TIME_FILE") else {
        panic!("set ST2K_TIME_FILE to the file to time");
    };
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_ROOT) };
    reset_scratch();
    put("EnableThumbs", 1);
    put("MaxSize", 0); // unlimited, so the size gate is never what is being measured

    let p = std::path::PathBuf::from(&path);
    // First call warms the DLL load and any codec init; report both so a big gap is visible
    // rather than being averaged into a misleading single number.
    let t0 = std::time::Instant::now();
    let first = unsafe { get_thumbnail_from_file(&p, 256) };
    let first_ms = t0.elapsed().as_millis();
    let t1 = std::time::Instant::now();
    let second = unsafe { get_thumbnail_from_file(&p, 256) };
    let second_ms = t1.elapsed().as_millis();
    reset_scratch();

    println!("  {path}");
    println!("  first call : {first_ms} ms  ({first:?})");
    println!("  second call: {second_ms} ms ({second:?})");
    assert!(first.is_ok(), "provider declined the file: {first:?}");
    assert!(second.is_ok());
}

/// What an over-cap file does at the REAL provider boundary, driven the way Explorer drives it.
///
/// Written to close a verification gap — the shell loads the DLL into an isolated surrogate
/// that reads a different registry view, so flipping a setting and watching Explorer proves
/// nothing — and it immediately earned its keep by disproving an assumption instead of
/// confirming one.
///
/// THE FINDING, recorded here because it is easy to re-assume: a shell stream does NOT hand
/// the provider a path. Both `SHCreateStreamOnFileEx` and a shell item bound through
/// `BHID_Stream` report only a bare LEAF NAME from `Stat`. An earlier version of this test
/// appeared to pass, but only because the fixture happened to sit in the test process's
/// working directory, so the leaf name resolved by accident — the same accident that could
/// have made the provider decode an unrelated same-named file. `stream_path` now requires an
/// ABSOLUTE path, which is why the on-disk case below is expected to be REFUSED here.
///
/// So the by-path rescue is real and measured, but on the front ends that genuinely have a
/// path (the `st2k` CLI and the Quick preview — a 528 MB PNG goes from refused to a 512x384
/// thumbnail). Whether it can ever fire inside Explorer depends on the shell handing over a
/// full path, which nothing here has demonstrated it does.
#[test]
fn oversized_file_backed_stream_is_rescued() {
    let _serial = lock_settings();
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_ROOT) };
    redirect_licence_to_scratch();
    reset_scratch();

    // Uncompressed BMP: comfortably over the 1 MB cap set below, and a format the OS
    // codecs read, which is the population this rescue is for.
    let big = encode(solid(700, 700, [120, 60, 200, 255]), ImageFormat::Bmp);
    assert!(big.len() > 1024 * 1024, "fixture must exceed the 1 MB cap");

    // Process-id suffixed so concurrent `cargo test` runs cannot collide.
    // Deliberately NOT the temp dir, to rule out anything special about it.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let img_path = dir.join(format!("st2k_rescue_{}.bmp", std::process::id()));
    std::fs::write(&img_path, &big).expect("stage the oversized fixture");

    // A file the OS codecs CANNOT open, to prove the size gate is still real and the rescue
    // is not just waving everything through.
    let junk_path = dir.join(format!("st2k_rescue_junk_{}.bmp", std::process::id()));
    std::fs::write(&junk_path, vec![0x7Au8; big.len()]).expect("stage the junk fixture");

    put("EnableThumbs", 1);
    put("MaxSize", 1); // 1 MB — both fixtures are over it

    let from_file = unsafe { get_thumbnail_from_file(&img_path, 64) };
    let from_memory = unsafe { get_thumbnail(&big, 64) };
    let junk_from_file = unsafe { get_thumbnail_from_file(&junk_path, 64) };

    reset_scratch();
    let _ = std::fs::remove_file(&img_path);
    let _ = std::fs::remove_file(&junk_path);

    // All three must be REFUSED, and the reason is now the USER's setting rather than any
    // limitation of ours. The oversized rescue deliberately declines to overrule a MaxSize the
    // user chose: routing around our own buffering ceiling is our business, second-guessing
    // their "do not bother with files over 1 MB" is not. That the rescue itself works without
    // a path is proven at the cascade level by
    // `streamsrc::tests::oversized_stream_is_rescued_without_any_path`.
    assert!(
        from_file.is_err(),
        "a file over the USER's MaxSize stays refused, rescue or no rescue: {from_file:?}"
    );
    assert!(
        from_memory.is_err(),
        "same bytes, same user cap, same answer"
    );
    assert!(
        junk_from_file.is_err(),
        "an over-cap file the OS codecs cannot decode must still be refused"
    );
}

/// The business-licence lock through the real COM handshake (`licence_state::shell_locked`
/// inside `GetThumbnail`): a Business copy whose 7-day evaluation and 3-day notice have both
/// run out declines like the master switch does; the same copy one day short of the lock is
/// still served; and a redeemed key on record is served again on the very next call, with no
/// restart of the host. The breadcrumb is the one the DLL reads (its path is redirected by
/// `redirect_licence_to_scratch`), written through the same `write_history` the app uses.
#[test]
fn the_business_licence_lock_gates_the_provider() {
    use sagethumbs2k_core::licence_state::{write_history, History, LOCK_GRACE_SECS, TRIAL_SECS};
    let _serial = lock_settings();
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_ROOT) };
    let home = redirect_licence_to_scratch();
    reset_scratch();
    let crumb = home.join("license-history.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let small = encode(solid(80, 60, [10, 200, 30, 255]), ImageFormat::Png);
    let never_licensed = |trial_started_unix: u64| History {
        was_business: true,
        trial_started_unix,
        ..Default::default()
    };

    // An evaluation whose notice period ran out a minute ago.
    assert!(write_history(
        &crumb,
        &never_licensed(now - TRIAL_SECS - LOCK_GRACE_SECS - 60)
    ));
    let locked = unsafe { get_thumbnail(&small, 64) };
    // The same clock a day before the lock lands: on notice, still served.
    assert!(write_history(
        &crumb,
        &never_licensed(now - TRIAL_SECS - LOCK_GRACE_SECS + 24 * 60 * 60)
    ));
    let on_notice = unsafe { get_thumbnail(&small, 64) };
    // A redeemed key on record, with a long-expired evaluation clock beside it.
    assert!(write_history(
        &crumb,
        &History {
            was_business: true,
            last_status: "active".into(),
            last_positive_unix: now,
            key_prefix: "esk_TEST".into(),
            trial_started_unix: now - 400 * 24 * 60 * 60,
            ..Default::default()
        }
    ));
    let licensed = unsafe { get_thumbnail(&small, 64) };

    let _ = std::fs::remove_file(&crumb);
    reset_scratch();

    assert!(
        locked.is_err(),
        "a locked business copy must decline (E_FAIL)"
    );
    assert!(
        on_notice.is_ok(),
        "the notice period still serves every thumbnail"
    );
    assert!(
        licensed.is_ok(),
        "a redeemed key unlocks the very next thumbnail"
    );
}

/// The MPEG-1/2 tier through the REAL COM handshake, which is the only way to reach
/// `streamsrc`'s stream cascade (tier 7, `mpeg12::mpeg_frame`).
///
/// Why this test exists rather than another corpus row: the corpus regression renders BY PATH
/// (`st2k thumbnail <file>`), so it exercises `decode.rs`'s by-bytes cascade and never touches
/// the shell-IStream one. The two cascades are maintained in parallel by hand and have drifted
/// before, which is the whole reason `streamsrc` carries a "the by-bytes twin of this gate"
/// comment on every tier. The FLV and VP9 tiers have the same shape and are covered here only
/// by the installed-Explorer script, which needs a registered DLL; this runs in CI.
///
/// The three files are the shapes Media Foundation cannot open on ANY Windows, so the answer
/// does not depend on whether the machine has the Store MPEG-2 extension: an MPEG-1 system
/// stream, a bare MPEG-1 elementary stream, and a bare MPEG-2 one. Corpus-gated like every
/// other sample-backed test; the helper `st2k.exe` sits beside the cdylib in the same profile,
/// so a build that produced the DLL produced it too.
#[test]
fn the_mpeg_tier_answers_through_the_shell_stream_cascade() {
    let _serial = lock_settings();
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_ROOT) };
    redirect_licence_to_scratch();
    reset_scratch();
    put("EnableThumbs", 1);

    let corpus = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-corpus");
    let mut proved = 0;
    // ST2K_NO_MF=1 makes this process behave like a machine with NO Media Foundation, which
    // is what this test has to be: with the Store MPEG-2 Video Extension installed (as it is
    // on the machine this was written on) MF answers for a transport stream long before our
    // tier is reached, so without this the assertions below would pass while proving nothing
    // about our own decoder. It is the same masking `check-magick-reliance.ps1` exists for.
    unsafe { common::set_test_env("ST2K_NO_MF", "1") };

    // The transport shapes ride the SAME tier since 2026-09-17, and they are the ones the
    // stream cascade can get wrong on its own: a transport stream has no magic at offset
    // zero, so the shape is read from a probe of the head, and this is the only place that
    // probe runs against a real IStream rather than a Cursor.
    for name in [
        "sample.mpeg",
        "real.m1v",
        "real-es.m2v",
        "sample.ts",
        "sample.m2ts",
        "sample.mts",
    ] {
        let Ok(bytes) = std::fs::read(corpus.join(name)) else {
            continue; // corpus-gated, like the other sample-backed tests
        };
        let got = unsafe { get_thumbnail(&bytes, 96) };
        assert!(
            got.is_ok(),
            "{name} must thumbnail through the shell stream cascade: {got:?}"
        );
        proved += 1;
    }

    // A stream that claims the pack-header magic and holds nothing else must be DECLINED,
    // not crashed on: the gate is the magic, and the demux is what has to survive the rest.
    let mut liar = vec![0x00, 0x00, 0x01, 0xBA];
    liar.extend(std::iter::repeat_n(0x5Au8, 64 * 1024));
    let junk = unsafe { get_thumbnail(&liar, 96) };

    reset_scratch();
    // Give Media Foundation back before the settings lock drops, or every later test in this
    // process would run against a machine that suddenly has no video codecs.
    unsafe { common::remove_test_env("ST2K_NO_MF") };
    assert!(
        junk.is_err(),
        "a file that only claims to be an MPEG program stream must be refused"
    );
    if corpus.join("sample.mpeg").exists() {
        assert!(
            proved > 0,
            "the corpus is present but no MPEG shape decoded"
        );
    }
}
