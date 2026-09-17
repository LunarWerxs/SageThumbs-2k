//! MPEG-1 / MPEG-2 video thumbnails for `.mpg` / `.mpeg` / `.m1v` / `.m2v` / `.vob` and for
//! MPEG-2 inside a TRANSPORT stream (`.ts` / `.m2ts` / `.mts`), decoded OUT OF PROCESS.
//!
//! Media Foundation has no source at all for an MPEG-1 SYSTEM stream (`00 00 01 BA` with the
//! MPEG-1 pack syntax: VideoCD and what late-1990s cameras and capture cards wrote) or for a
//! bare MPEG video ELEMENTARY stream (`00 00 01 B3`: `.m1v`, `.m2v`, ES-named `.mpg`), and it
//! opens an MPEG-2 PROGRAM stream (`.vob`, DVD rips) only once the Store "MPEG-2 Video
//! Extension" is installed. Measured 2026-09-17 with that extension present: every one of the
//! corpus's MPEG-1 and bare-ES samples still failed every MF tier while the file, our
//! registration and the magic gate were all healthy. The last MPEG-2 patent expired in 2018
//! (MPEG-1's earlier), so the `container/` doctrine against codec patents is satisfied.
//!
//! The same Store extension gates MPEG-2 in a TRANSPORT stream, which is what a DVB or
//! set-top-box recording is and what half the corpus's `.ts`-family samples are (the other
//! half is H.264, which Windows decodes itself and which never reaches this tier). Without
//! the extension those files showed the stock icon; since 2026-09-17 they are demuxed here
//! instead. A transport stream has no magic - it is recognised by its packet CLOCK, four
//! `0x47` sync bytes at a fixed stride: 188 for broadcast `.ts`, 192 for the M2TS/AVCHD
//! arrival-timestamp prefix (`.m2ts`, `.mts`), 204 for the Reed-Solomon parity some DVB
//! recorders keep. See [`ts_layout`].
//!
//! Division of labour across the process boundary, the same split as `crate::flv` (VP6 /
//! Sorenson) and `crate::vp9` (Profile 2/3):
//!
//! * THIS side, in the shell, pure parsing and bounded: the DEMUX. A window of the file around
//!   the user's video offset is read, a program stream is unwrapped to its video elementary
//!   stream (pack header in both syntaxes, system header, PES packets of stream ids
//!   `E0..EF` with the MPEG-1 and MPEG-2 PES header layouts), a bare ES passes through, and
//!   [`intra_slice`] cuts out ONE decodable unit: the sequence header with its extensions,
//!   the GOP header when one sits right there, and the first I-picture at or after the last
//!   GOP before the mark. That is all a thumbnail needs, and it is the only thing the child
//!   is ever handed, so no P- or B-picture and no second sequence ever reaches the decoder.
//! * The CHILD (`st2k mpeg-frame`, `src/bin/vdec/mpeg.rs`, feature `mpeg-video`): the
//!   pure-Rust `oxideav-mpeg12video` decoder, under the 512 MiB job-object memory cap, with
//!   its input size-capped here and there. A 0.0.x decoder crate is exactly what the
//!   out-of-process shape exists for: a crash is a non-zero exit the parent shrugs at, never
//!   a dead Explorer (the workspace builds `panic = "abort"`, where `catch_unwind` cannot
//!   help). The crate never links into the shell DLL: `cargo tree -p sagethumbs2k-dll` must
//!   never list it.
//!
//! ORDERING: this tier runs LAST in both cascades (`decode.rs`, `streamsrc.rs`), after every
//! Media Foundation tier came back empty. A `.vob` on a machine with the Store extension, or
//! a transport stream named `.mpg`, keeps hitting the hardware-accelerated in-process MF path;
//! only otherwise-blank tiles pay for a spawn. The two magics gate it, so every other
//! container skips the tier for the cost of a four-byte compare.

use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

/// The largest frame either end will decode, enforced by the child BEFORE the decoder
/// allocates and re-checked by the parent on the PNG it gets back. Tighter than the
/// shell-wide `decode::limits::MAX_DIM` (16384) on purpose: MPEG-2's 14-bit sizes top out
/// at 16383, so that cap would refuse nothing, while no real MPEG-1/2 stream exceeds High
/// Level's 1920x1152 (Main Profile) or the 4:2:2 profiles' 1920x1088. 4096 leaves room for
/// every oddball encoder and still bounds a hostile header at 25 MB of planes.
pub const MPEG_MAX_DIM: u32 = 4096;

/// Cap on the elementary-stream slice handed to the `st2k mpeg-frame` child (and on what
/// that child will accept from stdin — the two ends share this constant). One intra picture
/// at MPEG-2's 4:2:2 Profile @ High Level ceiling (80 Mbit/s, 4:2:2, 1920x1080) is a few MB;
/// a "picture" bigger than this is a crafted file.
pub const MPEG_INPUT_CAP: usize = 16 * 1024 * 1024;
/// Cap on the PNG read back from the child. The child refuses frames past
/// `decode::limits::MAX_DIM` and PNG shrinks raw RGBA; more means a broken/hostile child.
const MPEG_PNG_CAP: usize = 64 * 1024 * 1024;
/// CPU budget for one child decode — one intra picture through a software MPEG-2 decoder is
/// tens of milliseconds even at 1080i (measured 54 ms for a 720x480 DVD I-picture), so this
/// is pure headroom — plus the elapsed backstop for a child that hangs without burning CPU
/// on a loaded machine (the same split the ImageMagick, FLV and VP9 watchdogs use).
const MPEG_CPU_BUDGET: Duration = Duration::from_secs(20);
const MPEG_WALL_CEILING: Duration = Duration::from_secs(60);

/// How much of the source is read around the user's mark: `[mark - WINDOW_BACK, mark +
/// WINDOW_FORWARD)`. A GOP is half a second of video, which is 600 KB at DVD rates and
/// ~3 MB at MPEG-2's 50 Mbit/s studio profiles, so the back window always holds the
/// sequence header that opens the GOP the mark falls in, and the forward window holds its
/// I-picture many times over. Bounded reads are the point: a 4 GB `.vob` costs at most
/// this much I/O, and the shell IStream path coalesces it into a few big block reads.
const WINDOW_BACK: u64 = 4 * 1024 * 1024;
const WINDOW_FORWARD: u64 = 12 * 1024 * 1024;

/// The three shapes this module accepts, decided by the head of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `00 00 01 BA`: an MPEG-1 system stream or an MPEG-2 program stream (pack headers
    /// wrapping PES packets). Which of the two is read off the pack header's marker bits.
    ProgramStream,
    /// `00 00 01 B3`: a bare video elementary stream (nothing to demux).
    ElementaryStream,
    /// No magic at all: `0x47` sync bytes on a fixed clock. Carries its geometry, because
    /// the demux has to walk packets and only the head knows how far apart they are.
    TransportStream(TsLayout),
}

/// The packet geometry of a transport stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsLayout {
    /// Distance between consecutive sync bytes. 188 is the ISO packet itself (broadcast
    /// `.ts`); 192 is M2TS/AVCHD, whose 4-byte arrival-timestamp header sits BEFORE each
    /// packet; 204 is 188 plus 16 bytes of Reed-Solomon parity, which some DVB recorders
    /// keep in the file. In all three the packet is the 188 bytes that START at the sync.
    pub stride: usize,
    /// Where the first sync byte sits in the file's head. Recorded for the tests and the
    /// doctor note; the demux resynchronises on its own window rather than trusting it.
    pub offset: usize,
}

/// The packet strides worth testing, narrowest first, so the simplest geometry that can
/// explain four consecutive sync bytes wins.
const TS_STRIDES: [usize; 3] = [188, 192, 204];
/// The transport packet is always this long, whatever the stride wraps around it.
const TS_PACKET: usize = 188;
const TS_SYNC: u8 = 0x47;
/// Bytes of the file's head read to decide the shape: enough for four syncs at the widest
/// stride from the largest offset one may start at (204 + 3 * 204 = 816), rounded up.
const TS_PROBE: u64 = 2048;
/// How many program-map PIDs one Program Association Table may contribute. A stream with
/// more programs than this is either a full multiplex (where the first programs are as good
/// a choice as any) or hostile; either way the walk stays bounded.
const TS_MAX_PROGRAMS: usize = 16;

/// Which video standard the elementary stream is coded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// ISO/IEC 11172-2: a sequence header with NO sequence extension after it.
    Mpeg1,
    /// ISO/IEC 13818-2 (H.262): the sequence header is followed by `sequence_extension()`.
    Mpeg2,
}

/// What the head of the file says, or `None` for anything that is not MPEG-1/2 at all.
/// Give it at least [`TS_PROBE`] bytes: the two program/elementary magics are decided by the
/// first four, but a transport stream can only be recognised from several packets.
pub fn shape(head: &[u8]) -> Option<Shape> {
    match head.get(..4)? {
        [0x00, 0x00, 0x01, 0xBA] => return Some(Shape::ProgramStream),
        [0x00, 0x00, 0x01, 0xB3] => return Some(Shape::ElementaryStream),
        _ => {}
    }
    ts_layout(head).map(Shape::TransportStream)
}

/// Recognise a transport stream by its packet clock: FOUR consecutive sync bytes at one of
/// the known strides. Four rather than two because a lone `0x47` is a one-in-256 accident in
/// any binary, and because an M2TS would otherwise also answer at stride 188 counting from
/// its fifth byte — at four in a row that costs 2^-24. The first sync must sit inside the
/// first packet: a real file starts on a packet boundary (or, for M2TS, four bytes into
/// one), and allowing an arbitrary lead-in would turn this into a hunt through the whole
/// head for any byte that happens to be `0x47`.
fn ts_layout(head: &[u8]) -> Option<TsLayout> {
    TS_STRIDES.into_iter().find_map(|stride| {
        (0..stride)
            .find(|&offset| syncs_at(head, offset, stride))
            .map(|offset| TsLayout { stride, offset })
    })
}

/// Four sync bytes, `stride` apart, starting at `offset`.
fn syncs_at(buf: &[u8], offset: usize, stride: usize) -> bool {
    (0..4).all(|n| buf.get(offset + n * stride) == Some(&TS_SYNC))
}

/// The video codec inside a program or elementary stream, for `doctor` (`crate::vcodec`):
/// MPEG-1 or MPEG-2, decided by whether a `sequence_extension` follows the first sequence
/// header of the video ES. Reads at most the forward window from the file's head; `None`
/// for a non-MPEG source or one with no video sequence in that window (an audio-only
/// program stream, for instance).
pub fn identify<R: Read + Seek>(r: &mut R) -> Option<Codec> {
    let es = elementary_window(r, 0, WINDOW_FORWARD)?;
    let seq = find_start_code(&es, 0, |c| c == SC_SEQUENCE)?;
    // The sequence header is 12 bytes; each optional quantiser matrix adds 64. The next
    // start code after the header decides: `B5` with extension id 1 is MPEG-2's mandatory
    // `sequence_extension`, anything else means an MPEG-1 sequence (§6.1.1.6 / §2.4.2.3).
    let next = find_start_code(&es, seq + 4, |_| true)?;
    let is_ext = es.get(next + 3) == Some(&SC_EXTENSION)
        && es.get(next + 4).is_some_and(|b| b >> 4 == EXT_SEQUENCE);
    Some(if is_ext { Codec::Mpeg2 } else { Codec::Mpeg1 })
}

// Elementary-stream start codes (§6.2.1 / §2.4.2.2) this module dispatches on.
const SC_PICTURE: u8 = 0x00;
const SC_SEQUENCE: u8 = 0xB3;
const SC_EXTENSION: u8 = 0xB5;
const SC_SEQUENCE_END: u8 = 0xB7;
const SC_GROUP: u8 = 0xB8;
// System-level start codes (ISO/IEC 13818-1 Table 2-18 / 11172-1 §2.4.3).
const SC_PROGRAM_END: u8 = 0xB9;
const SC_PACK: u8 = 0xBA;
// `extension_start_code_identifier` values (Table 6-2) read off the high nibble after `B5`.
const EXT_SEQUENCE: u8 = 1;
const EXT_PICTURE_CODING: u8 = 8;
// `picture_coding_type` (Table 6-12 / §2.4.3.4): I and MPEG-1's DC-only D pictures are the
// two kinds that decode with no reference picture.
const PIC_I: u8 = 1;
const PIC_D: u8 = 4;
// `picture_structure` (Table 6-14): anything but a frame picture is one field of a pair.
const STRUCT_FRAME: u8 = 3;

/// Position of the first `00 00 01 xx` at or after `from` whose `xx` satisfies `want`.
fn find_start_code(es: &[u8], from: usize, want: impl Fn(u8) -> bool) -> Option<usize> {
    let mut i = from;
    while i + 4 <= es.len() {
        // Scan for the two-zero prefix without a byte-by-byte closure call on every position.
        let z = es[i..].iter().position(|&b| b == 0)?;
        i += z;
        if es.get(i + 1) == Some(&0) && es.get(i + 2) == Some(&1) {
            if let Some(&code) = es.get(i + 3) {
                if want(code) {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Unwrap a program stream (MPEG-1 system or MPEG-2 program syntax) to the concatenated
/// payload of its video PES packets (stream ids `E0..EF`). Bounded by its input; tolerant of
/// a window that starts mid-stream (it resynchronises on the next pack), a truncated tail, a
/// PES length of zero (legal only in transport streams, but seen: the payload then runs to
/// the next system-level start code) and every other stream kind (audio, private, padding,
/// navigation), which are skipped by their declared length. Never panics: every index is
/// checked, and a hostile length only ever ends the walk early.
pub fn demux_program_stream(ps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(j) = find_start_code(ps, i, |c| c >= SC_PROGRAM_END) {
        let code = ps[j + 3];
        let next = match code {
            SC_PROGRAM_END => None,
            SC_PACK => pack_header_end(ps, j),
            _ => {
                let (start, end) = packet_bounds(ps, j);
                if (0xE0..=0xEF).contains(&code) {
                    append_video_payload(&mut out, ps.get(start..end));
                }
                Some(end)
            }
        };
        // Every arm moves past `j` (a pack header is 12+ bytes, a packet body starts at
        // `j + 6`), so the walk always makes progress; a header cut off by the window ends it.
        match next {
            Some(n) => i = n,
            None => break,
        }
    }
    out
}

/// Where the pack header starting at `j` ends: MPEG-2's (`01` marker) is 14 bytes plus its
/// stuffing count, MPEG-1's (`0010`) is 12. `None` when the window ends inside it.
fn pack_header_end(ps: &[u8], j: usize) -> Option<usize> {
    let marker = *ps.get(j + 4)?;
    if marker & 0xC0 == 0x40 {
        let stuffing = usize::from(ps.get(j + 13)? & 7);
        Some(j + 14 + stuffing)
    } else {
        Some(j + 12)
    }
}

/// The body bounds of the length-prefixed packet (system header or PES) starting at `j`:
/// `(start, end)` with `end` clipped to the buffer. A zero length (legal only in transport
/// streams, but seen) runs to the next system-level start code; a length field cut off by
/// the window reads as zero and does the same, which ends the walk cleanly.
fn packet_bounds(ps: &[u8], j: usize) -> (usize, usize) {
    let start = j + 6;
    let plen = ps
        .get(j + 4..j + 6)
        .map_or(0, |len| usize::from(u16::from_be_bytes([len[0], len[1]])));
    let end = if plen == 0 {
        find_start_code(ps, start, |c| c >= SC_PROGRAM_END).unwrap_or(ps.len())
    } else {
        start.saturating_add(plen).min(ps.len())
    };
    (start, end)
}

/// Append the ES payload of one video PES body (the bytes after its header), when the body
/// is intact enough to have one.
fn append_video_payload(out: &mut Vec<u8>, body: Option<&[u8]>) {
    if let Some(body) = body {
        if let Some(skip) = pes_header_len(body) {
            out.extend_from_slice(&body[skip..]);
        }
    }
}

/// How many bytes of a video PES packet body are header (skip) before the ES payload:
/// the MPEG-2 layout (`10` marker, flags, `PES_header_data_length`) or the MPEG-1 layout
/// (stuffing `FF`s, an optional STD-buffer pair, then a PTS / PTS+DTS / `0F` field).
/// `None` when the body ends inside its own header.
fn pes_header_len(body: &[u8]) -> Option<usize> {
    let first = *body.first()?;
    if first & 0xC0 == 0x80 {
        let hdl = usize::from(*body.get(2)?);
        let skip = 3 + hdl;
        return (skip <= body.len()).then_some(skip);
    }
    let mut p = 0usize;
    while body.get(p) == Some(&0xFF) {
        p += 1;
    }
    if body.get(p).is_some_and(|b| b & 0xC0 == 0x40) {
        p += 2;
    }
    let b = *body.get(p)?;
    p += match b & 0xF0 {
        0x20 => 5,
        0x30 => 10,
        _ if b == 0x0F => 1,
        // Not a PES header we know: treat the whole body as payload rather than lose a
        // packet (the start-code search downstream tolerates leading junk).
        _ => 0,
    };
    (p <= body.len()).then_some(p)
}

/// Unwrap a transport stream to the elementary stream of its MPEG-1/2 video PID.
///
/// `layout` comes from the FILE's head, but `ts` is a WINDOW that may start anywhere, so the
/// walk resynchronises on the first run of sync bytes it can see rather than trusting the
/// recorded offset. Two passes over the window: one to decide which PID carries MPEG video,
/// one to concatenate it. Bounded by its input, and empty for every stream that is not ours
/// — an H.264 or HEVC transport stream (the usual AVCHD `.m2ts`) answers nothing here, which
/// is right: Media Foundation decodes those in process and ran long before this tier.
pub fn demux_transport_stream(ts: &[u8], layout: TsLayout) -> Vec<u8> {
    // `TsLayout` is constructible by anyone, and a stride of 0 (or anything below the packet
    // itself) would make the packet walks below advance by nothing and spin forever. Only the
    // geometries `ts_layout` can produce are walked; everything else is not a transport stream.
    if !TS_STRIDES.contains(&layout.stride) {
        return Vec::new();
    }
    let Some(start) = ts_resync(ts, layout.stride) else {
        return Vec::new();
    };
    let Some(pid) = video_pid(ts, start, layout.stride) else {
        return Vec::new();
    };
    collect_pid(ts, start, layout.stride, pid)
}

/// Where the first whole packet in this buffer begins. A window cut at an arbitrary byte is
/// at most one packet out of phase, so one packet of slack either way is all that is needed;
/// the bound matters because this runs on hostile input.
fn ts_resync(ts: &[u8], stride: usize) -> Option<usize> {
    (0..stride.saturating_mul(2)).find(|&o| syncs_at(ts, o, stride))
}

/// Which PID carries MPEG-1 or MPEG-2 video, preferring what the stream says about itself.
///
/// The Program Association Table names the program maps, and a Program Map Table names its
/// elementary streams with their types; `stream_type` 0x01 (ISO 11172-2) and 0x02 (ISO
/// 13818-2) are ours and nothing else is. Both tables repeat several times a second, so a
/// mid-file window nearly always holds them — but "nearly" is not "always" (a short file, a
/// clipped window, a recorder that writes them sparsely), so a PID whose packets open a
/// video PES is kept as a fallback. The fallback cannot mislead the decoder: a wrong PID
/// yields an elementary stream with no sequence header, and the slicer declines it.
fn video_pid(ts: &[u8], start: usize, stride: usize) -> Option<u16> {
    let mut pmt_pids: Vec<u16> = Vec::new();
    let mut sniffed: Option<u16> = None;
    let mut i = start;
    while let Some(pkt) = ts.get(i..i + TS_PACKET) {
        i += stride;
        let Some((pid, pusi, payload)) = packet_payload(pkt) else {
            continue;
        };
        if pid == 0 {
            collect_pat(payload, pusi, &mut pmt_pids);
        } else if pmt_pids.contains(&pid) {
            if let Some(video) = pmt_video_pid(payload, pusi) {
                return Some(video);
            }
        } else if sniffed.is_none() && pusi && starts_video_pes(payload) {
            sniffed = Some(pid);
        }
    }
    sniffed
}

/// The payload of one 188-byte transport packet, with its PID and `payload_unit_start`
/// flag. `None` for a packet that carries none: a lost sync, the transport error indicator
/// set, a scrambled payload (undecodable, and never ours to descramble), an adaptation
/// field with no payload after it, or an adaptation length that runs past the packet.
fn packet_payload(pkt: &[u8]) -> Option<(u16, bool, &[u8])> {
    if *pkt.first()? != TS_SYNC || *pkt.get(1)? & 0x80 != 0 {
        return None;
    }
    let b1 = *pkt.get(1)?;
    let b3 = *pkt.get(3)?;
    // transport_scrambling_control != '00', or adaptation_field_control with no payload bit.
    if b3 & 0xC0 != 0 || b3 & 0x10 == 0 {
        return None;
    }
    let mut p = 4usize;
    if b3 & 0x20 != 0 {
        p += 1 + usize::from(*pkt.get(4)?);
    }
    let pid = (u16::from(b1 & 0x1F) << 8) | u16::from(*pkt.get(2)?);
    Some((pid, b1 & 0x40 != 0, pkt.get(p..)?))
}

/// The PSI section a packet payload starts, past the `pointer_field` that says how much of
/// a PREVIOUS section still has to be skipped. Only the section a packet STARTS is read,
/// which is all a PAT or a PMT for one program ever needs; a table big enough to span
/// packets is a full broadcast multiplex, where the video PES sniff still answers.
fn psi_section(payload: &[u8], pusi: bool) -> Option<&[u8]> {
    if !pusi {
        return None;
    }
    payload.get(1 + usize::from(*payload.first()?)..)
}

/// The variable part of a PSI section: everything after `table_id`, the `section_length`
/// field and `fixed` bytes of table-specific header, with the trailing CRC-32 dropped and
/// the whole thing clipped to what the packet actually holds.
///
/// The CRC is deliberately NOT verified. Every field read out of here is bounds-checked, so
/// a corrupt section can only produce a wrong PID — which yields an elementary stream with
/// no sequence header and a clean decline — while checking it would throw away a section
/// that a window boundary merely clipped, costing a thumbnail that works.
fn section_body(sec: &[u8], fixed: usize) -> Option<&[u8]> {
    let len = usize::from(u16::from_be_bytes([*sec.get(1)? & 0x0F, *sec.get(2)?]));
    let end = 3usize.checked_add(len)?.min(sec.len());
    sec.get(3 + fixed..end.checked_sub(4)?)
}

/// Record the `program_map_PID` of every program a Program Association Table names. Program
/// number 0 is the network information table, not a program.
fn collect_pat(payload: &[u8], pusi: bool, out: &mut Vec<u16>) {
    let Some(sec) = psi_section(payload, pusi) else {
        return;
    };
    if sec.first() != Some(&0x00) {
        return;
    }
    let Some(body) = section_body(sec, 5) else {
        return;
    };
    for entry in body.as_chunks::<4>().0 {
        let program = u16::from_be_bytes([entry[0], entry[1]]);
        let pid = (u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]);
        if program != 0 && !out.contains(&pid) && out.len() < TS_MAX_PROGRAMS {
            out.push(pid);
        }
    }
}

/// The elementary PID a Program Map Table gives to MPEG-1 (`stream_type` 0x01) or MPEG-2
/// (0x02) video. Everything else — H.264 (0x1B), HEVC (0x24), AC-3, DVB subtitles, teletext
/// — is skipped by its own descriptor length, so an AVCHD `.m2ts` answers `None`.
fn pmt_video_pid(payload: &[u8], pusi: bool) -> Option<u16> {
    let sec = psi_section(payload, pusi)?;
    if sec.first() != Some(&0x02) {
        return None;
    }
    let body = section_body(sec, 5)?;
    // PCR_PID (2 bytes) then program_info_length (12 bits) worth of descriptors.
    let info = usize::from(u16::from_be_bytes([*body.get(2)? & 0x0F, *body.get(3)?]));
    let mut p = 4usize.checked_add(info)?;
    while let Some(entry) = body.get(p..p + 5) {
        if matches!(entry[0], 0x01 | 0x02) {
            return Some((u16::from(entry[1] & 0x1F) << 8) | u16::from(entry[2]));
        }
        let es_info = usize::from(u16::from_be_bytes([entry[3] & 0x0F, entry[4]]));
        p = p.checked_add(5)?.checked_add(es_info)?;
    }
    None
}

/// Does this payload open a video PES packet (`00 00 01 E0..EF`)?
fn starts_video_pes(payload: &[u8]) -> bool {
    matches!(payload.get(..4), Some([0x00, 0x00, 0x01, c]) if (0xE0..=0xEF).contains(c))
}

/// Concatenate the elementary stream carried on `pid`: a packet that STARTS a PES packet
/// contributes the bytes after that PES header, a continuation packet contributes its whole
/// payload. The PES `packet_length` is deliberately ignored — in a transport stream a video
/// PES is normally declared as length 0 (unbounded) and simply runs to the next one.
fn collect_pid(ts: &[u8], start: usize, stride: usize, pid: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = start;
    while let Some(pkt) = ts.get(i..i + TS_PACKET) {
        i += stride;
        match packet_payload(pkt) {
            Some((p, true, payload)) if p == pid && starts_video_pes(payload) => {
                append_video_payload(&mut out, payload.get(6..));
            }
            Some((p, false, payload)) if p == pid => out.extend_from_slice(payload),
            _ => {}
        }
    }
    out
}

/// `picture_coding_type` of the picture header starting at `p` (a `00 00 01 00` code).
fn picture_type(es: &[u8], p: usize) -> Option<u8> {
    es.get(p + 5).map(|b| (b >> 3) & 7)
}

/// `picture_structure` from the `picture_coding_extension` that follows the picture header
/// at `p`, or `None` for an MPEG-1 picture (no extension: always a frame). Only the very
/// next start code is looked at: the extension is required to follow the header directly.
fn picture_structure(es: &[u8], p: usize) -> Option<u8> {
    let ext = find_start_code(es, p + 4, |_| true)?;
    if es.get(ext + 3) != Some(&SC_EXTENSION) || es.get(ext + 4)? >> 4 != EXT_PICTURE_CODING {
        return None;
    }
    es.get(ext + 6).map(|b| b & 3)
}

/// Cut ONE decodable unit out of an elementary stream, as close to `target` (a byte offset
/// into `es`) as a GOP boundary allows: the sequence header with everything up to its first
/// GOP or picture (sequence extension, display extension, user data, quantiser matrices),
/// then the GOP header if one immediately precedes the picture, then the first intra
/// picture at or after the last GOP header before `target` (or the sequence header's own
/// first picture when there is no GOP), through the end of its slices — plus its partner
/// field when it is a field picture (an MPEG-2 I-frame coded as two fields is two picture
/// headers, and the decoder assembles the pair). `None` when the stream holds no sequence
/// header or no intra picture after the chosen anchor, or when the unit would exceed
/// [`MPEG_INPUT_CAP`].
///
/// Why GOP-aligned rather than "the last I-picture before target": a `quant_matrix_extension`
/// loaded by an earlier picture in the same GOP would apply to a later one, and a GOP's
/// first picture is defined to be intra (§6.3.8), so anchoring on the GOP is both simpler and
/// the seek every player performs.
pub fn intra_slice(es: &[u8], target: usize) -> Option<Vec<u8>> {
    let seq = governing_sequence(es, target)?;
    // The prelude: the sequence header and every extension / user-data block after it, up
    // to the first GOP or picture (or the end of this sequence).
    let prelude_end = find_start_code(es, seq + 4, |c| {
        matches!(c, SC_GROUP | SC_PICTURE | SC_SEQUENCE | SC_SEQUENCE_END)
    })
    .unwrap_or(es.len());
    // The anchor: the last GOP header between the prelude and the mark, when the mark lies
    // inside this sequence; otherwise the prelude's end (the sequence's first GOP/picture).
    let anchor = if seq <= target {
        last_gop_before(es, prelude_end, target)
    } else {
        prelude_end
    };
    let pic = first_intra_after(es, anchor)?;
    let (unit_start, unit_end) = unit_bounds(es, anchor, pic);
    let prelude = es.get(seq..prelude_end)?;
    let unit = es.get(unit_start..unit_end)?;
    let total = prelude.len().checked_add(unit.len())?;
    if total > MPEG_INPUT_CAP {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(prelude);
    out.extend_from_slice(unit);
    Some(out)
}

/// The sequence header that governs `target`: the last one at or before it, else the first
/// one after it (a stream whose only sequence header is ahead of the mark).
fn governing_sequence(es: &[u8], target: usize) -> Option<usize> {
    let mut seq = None;
    let mut pos = 0;
    while let Some(s) = find_start_code(es, pos, |c| c == SC_SEQUENCE) {
        if s > target {
            return seq.or(Some(s));
        }
        seq = Some(s);
        pos = s + 4;
    }
    seq
}

/// The last GOP header between `from` and `target`, or `from` itself when the sequence has
/// no GOP before the mark. Stops at the next sequence header: a GOP past it belongs to
/// another sequence.
fn last_gop_before(es: &[u8], from: usize, target: usize) -> usize {
    let mut anchor = from;
    let mut pos = from;
    while let Some(g) = find_start_code(es, pos, |c| c == SC_GROUP || c == SC_SEQUENCE) {
        if g > target || es[g + 3] == SC_SEQUENCE {
            break;
        }
        anchor = g;
        pos = g + 4;
    }
    anchor
}

/// The first intra picture (I, or MPEG-1's DC-only D) at or after `from`, staying inside
/// this sequence: a sequence header or end code before one means there is none to take.
fn first_intra_after(es: &[u8], from: usize) -> Option<usize> {
    let mut pos = from;
    while let Some(p) = find_start_code(es, pos, |c| {
        matches!(c, SC_PICTURE | SC_SEQUENCE | SC_SEQUENCE_END)
    }) {
        if es[p + 3] != SC_PICTURE {
            return None;
        }
        if matches!(picture_type(es, p), Some(PIC_I | PIC_D)) {
            return Some(p);
        }
        pos = p + 4;
    }
    None
}

/// Where the decodable unit around the intra picture at `pic` starts and ends. A GOP header
/// directly before the picture travels with it (harmless to the decoder, and it is where a
/// stream's `closed_gop`/`broken_link` live). The picture ends at the next picture / GOP /
/// sequence start code, and a field picture takes its partner field along: the pair is one
/// frame to the decoder.
fn unit_bounds(es: &[u8], anchor: usize, pic: usize) -> (usize, usize) {
    let start = if anchor < pic
        && es[anchor + 3] == SC_GROUP
        && find_start_code(es, anchor + 4, |_| true) == Some(pic)
    {
        anchor
    } else {
        pic
    };
    let picture_end = |from: usize| {
        find_start_code(es, from + 4, |c| {
            matches!(c, SC_PICTURE | SC_GROUP | SC_SEQUENCE | SC_SEQUENCE_END)
        })
        .unwrap_or(es.len())
    };
    let mut end = picture_end(pic);
    if picture_structure(es, pic).is_some_and(|s| s != STRUCT_FRAME)
        && es.get(end + 3) == Some(&SC_PICTURE)
    {
        end = picture_end(end);
    }
    (start, end)
}

/// Read `[start, start + len)` of the source and return its video elementary stream: the
/// bytes themselves for a bare ES, the demuxed video PES payload for a program stream.
/// `None` for a source that is neither (decided on the FILE's head, not the window's, so a
/// window starting mid-file still knows which syntax it is looking at).
fn elementary_window<R: Read + Seek>(r: &mut R, start: u64, len: u64) -> Option<Vec<u8>> {
    // TS_PROBE bytes, not four: a transport stream has no magic and is recognised from its
    // packet clock. `by_ref` so the reader survives the take and can be seeked again.
    let mut head = Vec::new();
    r.seek(SeekFrom::Start(0)).ok()?;
    r.by_ref().take(TS_PROBE).read_to_end(&mut head).ok()?;
    let shape = shape(&head)?;
    r.seek(SeekFrom::Start(start)).ok()?;
    let mut window = Vec::new();
    r.take(len).read_to_end(&mut window).ok()?;
    Some(match shape {
        Shape::ProgramStream => demux_program_stream(&window),
        Shape::ElementaryStream => window,
        Shape::TransportStream(layout) => demux_transport_stream(&window, layout),
    })
}

/// The elementary-stream unit the child decodes for a representative frame at `fraction`
/// of the source: a window around the mark is read (see [`WINDOW_BACK`]), demuxed, and cut
/// by [`intra_slice`] with the mark mapped into the window's ES proportionally. A window
/// that yields nothing (the mark sits in a stretch with no sequence header, or the file
/// is tiny) falls back once to the head of the file. Public so the child's corpus tests
/// (a separate bin crate) can walk exactly what the parent walks.
pub fn intra_slice_bytes<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    let total = r.seek(SeekFrom::End(0)).ok()?;
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // `total as f64` is exact below 2^53 and only ever a mark, never an index.
    let mark = (total as f64 * fraction) as u64;
    let start = mark.saturating_sub(WINDOW_BACK);
    let len = WINDOW_BACK + WINDOW_FORWARD;
    if let Some(es) = elementary_window(r, start, len) {
        if !es.is_empty() {
            // Map the mark into the window's ES by proportion (a PS packs video with audio
            // and navigation, so ES bytes are a fraction of file bytes; the demux keeps
            // their order, so the proportion lands in the right GOP or the one beside it).
            let span = len.min(total.saturating_sub(start)).max(1);
            let in_window = mark.saturating_sub(start).min(span);
            let target = ((es.len() as u128 * u128::from(in_window)) / u128::from(span)) as usize;
            if let Some(unit) = intra_slice(&es, target.min(es.len())) {
                return Some(unit);
            }
        }
    }
    if start == 0 {
        return None;
    }
    let es = elementary_window(r, 0, len)?;
    intra_slice(&es, 0)
}

/// Decode a representative intra picture of an MPEG-1/2 program or elementary stream to a
/// frame — OUT OF PROCESS via the sibling `st2k.exe` (see the module docs for why).
/// Self-gated on the two magics; any failure — no sibling exe (DLL-only or feature-less
/// build), a stream with no decodable picture, hostile input, a child crash/abort,
/// over-budget — is a clean `None`, exactly the pre-support behaviour.
pub(crate) fn mpeg_frame<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<image::DynamicImage> {
    let unit = intra_slice_bytes(r, fraction)?;
    if unit.is_empty() || unit.len() > MPEG_INPUT_CAP {
        return None;
    }
    let png = crate::flv::child_frame_png(
        "mpeg-frame",
        &unit,
        MPEG_CPU_BUDGET,
        MPEG_WALL_CEILING,
        MPEG_PNG_CAP,
    )?;
    // Bounded parse of OUR OWN child's output: the PNG is size-capped above and the child
    // caps its frame at MPEG_MAX_DIM², so this in-process decode is small by construction.
    // The declared dimensions are read off the header and checked BEFORE the pixels are
    // decoded (the review of this tier asked for the check ahead of the allocation, not
    // after it), then re-checked on the decoded image so a lying header changes nothing.
    let max = MPEG_MAX_DIM;
    let (w, h) =
        image::ImageReader::with_format(std::io::Cursor::new(&png), image::ImageFormat::Png)
            .into_dimensions()
            .ok()?;
    if w == 0 || h == 0 || w > max || h > max {
        crate::safety::log_debug("mpeg decode: child declared out-of-bounds dimensions");
        return None;
    }
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?;
    if img.width() != w || img.height() != h {
        crate::safety::log_debug("mpeg decode: child returned out-of-bounds dimensions");
        return None;
    }
    Some(img)
}

/// Synthetic streams in the exact shapes the demux and slicer walk, for the fuzz harness
/// (`crate::fuzz`) and the tests below. Test-only.
#[cfg(test)]
pub(crate) mod fuzzseed {
    /// A minimal elementary stream: sequence header (64x48), an MPEG-2 sequence extension
    /// when `mpeg2`, a GOP header, one I-picture with a coding extension (MPEG-2) and a slice
    /// of arbitrary bytes, then a P-picture, then a second GOP + I-picture. Two GOPs so the
    /// anchor choice has something to choose between.
    pub(crate) fn elementary(mpeg2: bool) -> Vec<u8> {
        let mut es = Vec::new();
        // sequence_header: horizontal 64, vertical 48, aspect 1, frame rate 3 (25 Hz),
        // bit rate 0x3FFFF (variable), marker, vbv 0, constrained 0, no matrices.
        es.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xB3, 0x04, 0x00, 0x30, 0x13, 0xFF, 0xFF, 0xE0, 0x18,
        ]);
        if mpeg2 {
            // sequence_extension: id 1, profile/level Main@Main (0x48), progressive, 4:2:0.
            es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x14, 0x82, 0x00, 0x01, 0x00, 0x00]);
        }
        for (gop, tref) in [(true, 0u8), (false, 1), (true, 0)] {
            if gop {
                // group_of_pictures_header: time code 0, closed_gop 1, broken_link 0.
                es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
            }
            let ptype: u8 = if gop { 1 } else { 2 };
            // picture_header: temporal_reference (10 bits) then picture_coding_type (3).
            es.extend_from_slice(&[
                0x00,
                0x00,
                0x01,
                0x00,
                tref >> 2,
                ((tref & 3) << 6) | (ptype << 3) | 7,
                0xFF,
                0xF8,
            ]);
            if mpeg2 {
                // picture_coding_extension: id 8, f_codes 15, intra_dc_precision 0,
                // picture_structure FRAME (3), frame_pred_frame_dct 1, ...
                es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x8F, 0xFF, 0xF3, 0x98, 0x00]);
            }
            // slice 1 with a few bytes of "coded macroblocks".
            es.extend_from_slice(&[0x00, 0x00, 0x01, 0x01, 0x0A, 0xB4, 0x5C, 0x33, 0x80]);
        }
        es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB7]);
        es
    }

    /// The elementary stream above wrapped as an MPEG-1 SYSTEM stream: an MPEG-1 pack header,
    /// a system header, then the ES cut into PES packets of stream id `E0` with the MPEG-1
    /// PES header (stuffing, STD buffer, PTS), interleaved with an audio PES (`C0`) that the
    /// demux must skip and a padding packet (`BE`).
    pub(crate) fn mpeg1_system(es: &[u8]) -> Vec<u8> {
        let mut ps = Vec::new();
        let pack = [
            0x00, 0x00, 0x01, 0xBA, 0x21, 0x00, 0x01, 0x00, 0x01, 0x80, 0x1B, 0x91,
        ];
        ps.extend_from_slice(&pack);
        // system_header, length 12: rate bound, audio/video bounds, one stream entry.
        ps.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xBB, 0x00, 0x0C, 0x80, 0x1B, 0x91, 0x04, 0xE1, 0xFF, 0xE0, 0xE0,
            0xE8, 0xC0, 0xC0, 0x20,
        ]);
        for (n, chunk) in es.chunks(40).enumerate() {
            if n % 2 == 1 {
                ps.extend_from_slice(&pack);
                // audio PES: MPEG-1 header, 4 payload bytes.
                ps.extend_from_slice(&[
                    0x00, 0x00, 0x01, 0xC0, 0x00, 0x05, 0x0F, 0xDE, 0xAD, 0xBE, 0xEF,
                ]);
            }
            // MPEG-1 PES header: two stuffing bytes, STD buffer (2), PTS (5) = 9 bytes.
            let hdr = [0xFF, 0xFF, 0x40, 0x20, 0x21, 0x00, 0x01, 0x00, 0x01];
            let len = (hdr.len() + chunk.len()) as u16;
            ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
            ps.extend_from_slice(&len.to_be_bytes());
            ps.extend_from_slice(&hdr);
            ps.extend_from_slice(chunk);
        }
        // padding_stream, 4 bytes of 0xFF, then program end.
        ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xBE, 0x00, 0x04, 0xFF, 0xFF, 0xFF, 0xFF]);
        ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xB9]);
        ps
    }

    /// The elementary stream wrapped as an MPEG-2 PROGRAM stream: MPEG-2 pack headers (with
    /// stuffing bytes), PES packets of stream id `E0` with the MPEG-2 PES header (flags,
    /// `PES_header_data_length`, a PTS), a private-stream-1 PES (`BD`) to skip, and one
    /// zero-length PES (payload runs to the next pack) so that arm is walked too.
    pub(crate) fn mpeg2_program(es: &[u8]) -> Vec<u8> {
        let mut ps = Vec::new();
        // MPEG-2 pack header, 14 bytes + 2 stuffing bytes (low 3 bits of byte 13 = 2).
        let pack = [
            0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xC3, 0xFA,
            0xFF, 0xFF,
        ];
        ps.extend_from_slice(&pack);
        let chunks: Vec<&[u8]> = es.chunks(48).collect();
        for (n, chunk) in chunks.iter().enumerate() {
            ps.extend_from_slice(&pack);
            if n % 3 == 2 {
                // private_stream_1 (AC-3 on a DVD): MPEG-2 header, 3 payload bytes.
                ps.extend_from_slice(&[
                    0x00, 0x00, 0x01, 0xBD, 0x00, 0x06, 0x81, 0x00, 0x00, 0x80, 0x01, 0x02,
                ]);
            }
            // MPEG-2 PES header: '10' + flags, PTS flag, header_data_length 5, PTS (5).
            let hdr = [0x81, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01];
            let last = n + 1 == chunks.len();
            let len: u16 = if last {
                0
            } else {
                (hdr.len() + chunk.len()) as u16
            };
            ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
            ps.extend_from_slice(&len.to_be_bytes());
            ps.extend_from_slice(&hdr);
            ps.extend_from_slice(chunk);
        }
        ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xB9]);
        ps
    }

    /// The elementary stream wrapped as a TRANSPORT stream. `stride` picks the geometry
    /// (188 broadcast, 192 M2TS arrival-timestamp prefix, 204 DVB parity). With `tables`,
    /// a PAT and a PMT name the video PID the way a real recording does; without them the
    /// demux has to fall back to sniffing a video PES, which is the mid-file-window case.
    /// An audio PID and an adaptation-field-only packet are woven in for the demux to skip.
    pub(crate) fn transport_stream(es: &[u8], stride: usize, tables: bool) -> Vec<u8> {
        const VIDEO: u16 = 0x0100;
        let mut ts = Vec::new();
        if tables {
            // PAT: one program (number 1) whose map lives on PID 0x1000.
            ts.extend(packet(
                stride,
                0,
                true,
                &[
                    0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
                    0x00, 0x00, 0x00, 0x00,
                ],
            ));
            // PMT: PCR on the video PID, MPEG-2 video (type 0x02) on 0x0100, MPEG audio
            // (0x03) on 0x0101. The CRC-32 is zeros - nothing here verifies it, on purpose.
            ts.extend(packet(
                stride,
                0x1000,
                true,
                &[
                    0x00, 0x02, 0xB0, 0x17, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00,
                    0x02, 0xE1, 0x00, 0xF0, 0x00, 0x03, 0xE1, 0x01, 0xF0, 0x00, 0x00, 0x00, 0x00,
                    0x00,
                ],
            ));
        }
        // The PES header this seed writes: '10' marker + flags, PTS only, 5 bytes of it.
        let pes = [0x81u8, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01];
        // One unbounded PES packet: its header rides the first packet, every later packet of
        // the PID is a continuation. That is exactly how a real encoder writes video.
        let mut first = true;
        for chunk in es.chunks(48) {
            let mut payload = Vec::new();
            if first {
                // packet_length 0: the normal declaration for video in a transport stream.
                payload.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00]);
                payload.extend_from_slice(&pes);
            }
            payload.extend_from_slice(chunk);
            ts.extend(packet(stride, VIDEO, first, &payload));
            first = false;
            // An audio packet and a packet with no payload at all, both to be stepped over.
            ts.extend(packet(stride, 0x0101, true, &[0xDE, 0xAD, 0xBE, 0xEF]));
            ts.extend(adaptation_only(stride, VIDEO));
        }
        ts
    }

    /// One transport packet: the 4-byte header, an adaptation field of stuffing when the
    /// payload is short of 184 bytes, then the payload — wrapped in whatever the stride
    /// adds (M2TS's arrival timestamp before, DVB's parity after).
    fn packet(stride: usize, pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let body = &payload[..payload.len().min(184)];
        let stuff = 184 - body.len();
        let mut p = Vec::with_capacity(stride);
        if stride == 192 {
            p.extend_from_slice(&[0x40, 0x00, 0x00, 0x00]);
        }
        p.push(0x47);
        p.push((u8::from(pusi) << 6) | ((pid >> 8) as u8 & 0x1F));
        p.push((pid & 0xFF) as u8);
        // adaptation_field_control: '11' when stuffing is needed, '01' when not.
        p.push(if stuff > 0 { 0x31 } else { 0x11 });
        if stuff > 0 {
            p.push((stuff - 1) as u8);
            if stuff > 1 {
                p.push(0x00);
                p.resize(p.len() + stuff - 2, 0xFF);
            }
        }
        p.extend_from_slice(body);
        if stride == 204 {
            p.extend_from_slice(&[0u8; 16]);
        }
        p
    }

    /// A packet carrying only an adaptation field: legal, common (it is how a PCR is sent),
    /// and the demux must step over it without taking any bytes from it.
    fn adaptation_only(stride: usize, pid: u16) -> Vec<u8> {
        let mut p = Vec::with_capacity(stride);
        if stride == 192 {
            p.extend_from_slice(&[0x40, 0x00, 0x00, 0x00]);
        }
        p.push(0x47);
        p.push((pid >> 8) as u8 & 0x1F);
        p.push((pid & 0xFF) as u8);
        p.push(0x20);
        p.push(183);
        p.push(0x00);
        p.resize(p.len() + 182, 0xFF);
        if stride == 204 {
            p.extend_from_slice(&[0u8; 16]);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    fn corpus(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join(name)
    }

    /// The three synthetic shapes all demux to the SAME elementary stream, byte for byte:
    /// the program-stream demux is lossless over both PES header layouts, skips audio,
    /// private and padding packets, and handles a zero-length PES.
    #[test]
    fn synthetic_wrappers_demux_to_their_elementary_stream() {
        for mpeg2 in [false, true] {
            let es = fuzzseed::elementary(mpeg2);
            assert_eq!(demux_program_stream(&fuzzseed::mpeg1_system(&es)), es);
            assert_eq!(demux_program_stream(&fuzzseed::mpeg2_program(&es)), es);
            assert_eq!(
                identify(&mut Cursor::new(fuzzseed::mpeg2_program(&es))),
                Some(if mpeg2 { Codec::Mpeg2 } else { Codec::Mpeg1 })
            );
        }
    }

    /// The slicer hands back the sequence prelude plus exactly ONE intra picture (with its
    /// GOP header), never the P-picture, and picks the GOP the mark falls in.
    #[test]
    fn slicer_returns_one_intra_picture_from_the_marked_gop() {
        for mpeg2 in [false, true] {
            let es = fuzzseed::elementary(mpeg2);
            let first_gop = find_start_code(&es, 0, |c| c == SC_GROUP).unwrap();
            let second_gop = find_start_code(&es, first_gop + 4, |c| c == SC_GROUP).unwrap();
            // Mark at the start: prelude + first GOP + its I-picture.
            let head = intra_slice(&es, 0).expect("slice at 0");
            assert_eq!(&head[..first_gop], &es[..first_gop], "prelude verbatim");
            assert_eq!(
                head[first_gop + 3],
                SC_GROUP,
                "GOP header travels with the picture"
            );
            let pics: Vec<usize> =
                std::iter::successors(find_start_code(&head, 0, |c| c == SC_PICTURE), |&p| {
                    find_start_code(&head, p + 4, |c| c == SC_PICTURE)
                })
                .collect();
            assert_eq!(pics.len(), 1, "exactly one picture");
            assert_eq!(picture_type(&head, pics[0]), Some(PIC_I));
            assert!(!head.ends_with(&[0x00, 0x00, 0x01, 0xB7]));
            // Mark inside the second GOP: the same prelude, the second GOP's I-picture.
            let tail = intra_slice(&es, second_gop + 2).expect("slice in GOP 2");
            assert_eq!(&tail[..first_gop], &es[..first_gop]);
            assert_eq!(&tail[first_gop..], &es[second_gop..es.len() - 4]);
            // Mark past the end: still the last GOP, never None.
            assert_eq!(intra_slice(&es, es.len() + 100), Some(tail));
        }
    }

    /// Through the reader entry point, all three shapes yield a unit the child could decode,
    /// and a source that is not MPEG at all yields nothing without a spawn.
    #[test]
    fn reader_entry_point_covers_every_shape_and_declines_junk() {
        let es = fuzzseed::elementary(true);
        for src in [
            es.clone(),
            fuzzseed::mpeg1_system(&es),
            fuzzseed::mpeg2_program(&es),
            fuzzseed::transport_stream(&es, 188, true),
            fuzzseed::transport_stream(&es, 192, true),
            fuzzseed::transport_stream(&es, 204, false),
        ] {
            for at in [0.0, 0.3, 1.0, f64::NAN, -5.0] {
                let unit = intra_slice_bytes(&mut Cursor::new(&src), at).expect("a unit");
                assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]));
            }
        }
        assert!(intra_slice_bytes(&mut Cursor::new(b"junk"), 0.3).is_none());
        assert!(intra_slice_bytes(&mut Cursor::new(&[0u8; 256]), 0.3).is_none());
        assert!(mpeg_frame(&mut Cursor::new(b"RIFF....AVI "), 0.3).is_none());
        assert!(shape(b"\x00\x00\x01\xBA").is_some() && shape(b"\x00\x00\x01").is_none());
    }

    /// Truncations and stomps of every seed must come back `None`/`Some` and never panic
    /// (the always-on `crate::fuzz` gate mutates these far harder; this is the smoke test
    /// that runs even with fuzzing filtered out).
    #[test]
    fn truncations_never_panic() {
        let es = fuzzseed::elementary(true);
        for src in [
            es.clone(),
            fuzzseed::mpeg1_system(&es),
            fuzzseed::mpeg2_program(&es),
        ] {
            for n in 0..src.len() {
                let _ = intra_slice_bytes(&mut Cursor::new(&src[..n]), 0.3);
                let _ = identify(&mut Cursor::new(&src[..n]));
                let _ = demux_program_stream(&src[..n]);
                let _ = intra_slice(&src[..n], n / 2);
            }
        }
        // The transport walk gets the same treatment, at every stride and against every
        // layout (including a layout that does NOT match the bytes, which is what a head
        // that lied would hand it), in steps so a multi-packet seed stays a quick test.
        for stride in TS_STRIDES {
            let ts = fuzzseed::transport_stream(&es, stride, true);
            for n in (0..ts.len()).step_by(7) {
                let _ = intra_slice_bytes(&mut Cursor::new(&ts[..n]), 0.3);
                let _ = identify(&mut Cursor::new(&ts[..n]));
                for other in TS_STRIDES {
                    let _ = demux_transport_stream(
                        &ts[..n],
                        TsLayout {
                            stride: other,
                            offset: 0,
                        },
                    );
                }
            }
        }
        assert!(packet_payload(&[]).is_none());
        assert!(
            packet_payload(&[TS_SYNC, 0x80, 0x00, 0x10]).is_none(),
            "error bit"
        );
        assert!(
            packet_payload(&[TS_SYNC, 0x00, 0x00, 0x90]).is_none(),
            "scrambled"
        );
        assert!(
            packet_payload(&[TS_SYNC, 0x00, 0x00, 0x20, 0x00]).is_none(),
            "no payload"
        );
        assert!(
            packet_payload(&[TS_SYNC, 0x00, 0x00, 0x30, 0xFF]).is_none(),
            "af overruns"
        );
        assert!(section_body(&[0x00, 0xB0, 0x02, 0x00], 5).is_none());
        // A stride nobody can reach through `ts_layout` must not be walked at all: a zero
        // stride would otherwise advance the packet loops by nothing and hang the shell.
        let ts = fuzzseed::transport_stream(&es, 188, true);
        for stride in [0usize, 1, 187, 189, 4096] {
            assert!(
                demux_transport_stream(&ts, TsLayout { stride, offset: 0 }).is_empty(),
                "stride {stride} is not a transport geometry"
            );
        }
        // A PES whose declared length overruns the buffer, and one whose header claims more
        // header bytes than exist.
        assert!(pes_header_len(&[0x80, 0x80, 0xFF]).is_none());
        assert_eq!(pes_header_len(&[0xFF, 0xFF, 0x0F, 0xAA]), Some(3));
        assert!(pes_header_len(&[0xFF, 0xFF, 0x21]).is_none());
        assert_eq!(
            demux_program_stream(&[0, 0, 1, 0xE0, 0xFF, 0xFF, 0x0F, 0x42]),
            vec![0x42]
        );
    }

    /// The real corpus files, every shape this tier targets: MPEG-2 ES (`sample.mpg` /
    /// `sample.m2v`), MPEG-1 system stream (`sample.mpeg`), MPEG-2 program streams
    /// (`sample.vob`, `real.m2v`, `real.vob`), the real MPEG-1 elementary stream
    /// (`real.m1v`) and — since 2026-09-17 — MPEG-2 inside a TRANSPORT stream at all three
    /// packet strides (`sample.ts` 188, `sample.m2ts` 192, `sample.mts`, and the corpus's
    /// `real.mpg`, which is a 188-byte transport stream wearing a program-stream name).
    /// Each yields a unit that starts with a sequence header and holds one intra picture.
    /// Corpus-gated, like every sample-backed test.
    #[test]
    fn corpus_streams_slice_to_one_intra_picture() {
        let mut seen = 0;
        for (name, codec) in [
            ("sample.mpg", Codec::Mpeg2),
            ("sample.m2v", Codec::Mpeg2),
            ("sample.mpeg", Codec::Mpeg1),
            ("sample.vob", Codec::Mpeg2),
            ("real.m2v", Codec::Mpeg2),
            ("real.vob", Codec::Mpeg2),
            ("real.m1v", Codec::Mpeg1),
            ("real-vcd.mpg", Codec::Mpeg1),
            ("real-es.m2v", Codec::Mpeg2),
            ("sample.ts", Codec::Mpeg2),
            ("sample.m2ts", Codec::Mpeg2),
            ("sample.mts", Codec::Mpeg2),
            ("real.mpg", Codec::Mpeg2),
        ] {
            let Ok(bytes) = std::fs::read(corpus(name)) else {
                continue;
            };
            seen += 1;
            let unit = intra_slice_bytes(&mut Cursor::new(&bytes), 0.30)
                .unwrap_or_else(|| panic!("{name}: no intra unit"));
            assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]), "{name}");
            assert!(unit.len() <= MPEG_INPUT_CAP, "{name}");
            // The unit holds ONE frame. That is one picture for a frame-coded stream, and a
            // PAIR for a field-coded one, where the second field is legitimately coded as P
            // (predicted from the first field of the same frame) — `unit_bounds` takes the
            // partner field on purpose, because the two fields ARE one frame to the decoder.
            // The corpus's `real.mpg`, a field-coded DVB recording, is the sample that
            // proves it; every earlier sample was frame-coded, so this assertion used to
            // read "every picture is intra" and would have failed the moment one arrived.
            let mut pictures = Vec::new();
            let mut pos = 0;
            while let Some(p) = find_start_code(&unit, pos, |c| c == SC_PICTURE) {
                pictures.push((picture_type(&unit, p), picture_structure(&unit, p)));
                pos = p + 4;
            }
            assert!(
                (1..=2).contains(&pictures.len()),
                "{name}: {} pictures in the unit",
                pictures.len()
            );
            assert!(
                matches!(pictures[0].0, Some(PIC_I | PIC_D)),
                "{name}: the first picture is not intra"
            );
            if pictures.len() == 2 {
                assert!(
                    pictures[0].1.is_some_and(|s| s != STRUCT_FRAME),
                    "{name}: a second picture is only allowed as the partner FIELD"
                );
            }
            assert_eq!(identify(&mut Cursor::new(&bytes)), Some(codec), "{name}");
        }
        // The H.264 transport streams in the same corpus are recognised as transport
        // streams and then DECLINED, because their program map names stream_type 0x1B and
        // nothing in them is MPEG-1/2 video. Media Foundation decodes those in process and
        // runs long before this tier; an answer here would be the wrong one.
        for name in ["real.ts", "real.m2ts", "real.mts"] {
            let Ok(bytes) = std::fs::read(corpus(name)) else {
                continue;
            };
            assert!(
                matches!(shape(&bytes), Some(Shape::TransportStream(_))),
                "{name}: should still be recognised as a transport stream"
            );
            assert!(
                intra_slice_bytes(&mut Cursor::new(&bytes), 0.30).is_none(),
                "{name}: H.264 is not ours"
            );
            assert!(identify(&mut Cursor::new(&bytes)).is_none(), "{name}");
        }
        if seen > 0 {
            eprintln!("corpus_streams_slice_to_one_intra_picture: {seen} corpus streams sliced");
        }
    }

    /// The packet clock is read correctly at all three strides, from a head alone, and the
    /// M2TS geometry is not mistaken for a plain one (its sync bytes also sit 188 apart
    /// once, four bytes in — which is exactly why the probe demands four in a row).
    #[test]
    fn every_transport_geometry_is_recognised_and_demuxes_to_its_elementary_stream() {
        for mpeg2 in [false, true] {
            let es = fuzzseed::elementary(mpeg2);
            for (stride, offset) in [(188, 0), (192, 4), (204, 0)] {
                for tables in [true, false] {
                    let ts = fuzzseed::transport_stream(&es, stride, tables);
                    assert_eq!(
                        shape(&ts),
                        Some(Shape::TransportStream(TsLayout { stride, offset })),
                        "stride {stride}, tables {tables}"
                    );
                    let out = demux_transport_stream(&ts, TsLayout { stride, offset });
                    assert_eq!(out, es, "stride {stride}, tables {tables}");
                    let unit = intra_slice_bytes(&mut Cursor::new(&ts), 0.30)
                        .unwrap_or_else(|| panic!("stride {stride}: no unit"));
                    assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]));
                    assert_eq!(
                        identify(&mut Cursor::new(&ts)),
                        Some(if mpeg2 { Codec::Mpeg2 } else { Codec::Mpeg1 })
                    );
                }
            }
        }
    }

    /// A window cut at an arbitrary byte — which is what `intra_slice_bytes` hands the demux
    /// for any file bigger than the read window — resynchronises on the packet clock instead
    /// of losing the stream.
    #[test]
    fn a_transport_window_that_starts_mid_packet_resynchronises() {
        let es = fuzzseed::elementary(true);
        for stride in TS_STRIDES {
            let ts = fuzzseed::transport_stream(&es, stride, true);
            let layout = TsLayout { stride, offset: 0 };
            for cut in [1usize, 7, 93, stride - 1, stride + 5] {
                let out = demux_transport_stream(&ts[cut..], layout);
                assert!(
                    !out.is_empty()
                        && es
                            .windows(4)
                            .any(|w| out.starts_with(w) || out.ends_with(w)),
                    "stride {stride}, cut {cut}: lost the stream"
                );
                assert!(
                    find_start_code(&out, 0, |c| c == SC_SEQUENCE).is_some(),
                    "stride {stride}, cut {cut}: no sequence header survived"
                );
            }
        }
    }

    /// A transport stream whose program map names only H.264 is declined, and so is one
    /// whose video PID is scrambled — neither may produce bytes for the decoder.
    #[test]
    fn a_transport_stream_that_is_not_ours_yields_nothing() {
        let es = fuzzseed::elementary(true);
        let layout = TsLayout {
            stride: 188,
            offset: 0,
        };
        // A program map whose only elementary streams are H.264 (0x1B) and AC-3 (0x81)
        // names no video of ours. Found by its own bytes rather than by arithmetic, so a
        // change to the seed's packing cannot quietly turn this into a test of nothing.
        let mut h264 = fuzzseed::transport_stream(&es, 188, true);
        let sec = h264
            .windows(5)
            .position(|w| w == [0x02, 0xB0, 0x17, 0x00, 0x01])
            .expect("the PMT section this test rewrites");
        assert_eq!(h264[sec + 12], 0x02, "the stream_type byte moved");
        h264[sec + 12] = 0x1B;
        h264[sec + 17] = 0x81;
        assert!(pmt_video_pid(&h264[sec - 1..], true).is_none());
        // A stream carrying a video PES that holds no MPEG sequence header at all — which
        // is what a real H.264 transport stream looks like to the PES sniff — is demuxed
        // and then DECLINED by the slicer rather than handed to the decoder.
        let junk = fuzzseed::transport_stream(&vec![0x5Au8; 600], 188, false);
        assert!(!demux_transport_stream(&junk, layout).is_empty());
        assert!(intra_slice_bytes(&mut Cursor::new(&junk), 0.30).is_none());
        // A scrambled video PID: transport_scrambling_control '10' on every packet of it.
        let mut scrambled = fuzzseed::transport_stream(&es, 188, false);
        for p in (0..scrambled.len() / 188).map(|n| n * 188) {
            if scrambled[p + 1] & 0x1F == 0x01 && scrambled[p + 2] == 0x00 {
                scrambled[p + 3] |= 0x80;
            }
        }
        assert!(demux_transport_stream(&scrambled, layout).is_empty());
    }

    /// A real stream decodes WHEN the helper is there, and declines cleanly when it is not.
    /// Both are correct (see `vp9::tests` for why the assertion is conditional).
    #[test]
    fn a_real_stream_decodes_when_the_helper_exists() {
        let Ok(bytes) = std::fs::read(corpus("sample.mpeg")) else {
            return;
        };
        let got = mpeg_frame(&mut Cursor::new(&bytes), 0.30);
        match crate::sibling_of_dll(crate::CLI_EXE) {
            Some(exe) if exe.exists() => {
                let img = got.expect("the helper is present, so MPEG-1 must decode");
                assert_eq!((img.width(), img.height()), (640, 360));
            }
            _ => assert!(
                got.is_none(),
                "without the helper this must decline cleanly"
            ),
        }
    }
}
