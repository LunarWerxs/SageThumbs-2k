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

/// Where a transport stream's content ends: just past its last sync-marked packet, or `None`
/// when the file's head is not a transport stream or its content runs to the end already.
///
/// A recorder that preallocates its file, or a download still in progress, leaves zeros past
/// the last packet written, and Media Foundation's transport source refuses such a file
/// outright (no frame, at any size, measured 2026-09-23); handed only the packets it reads it
/// like any other. The content is assumed to come first and the padding after, which is how a
/// sequential writer leaves a file, so a binary search over packet indices finds the boundary
/// with one one-byte read per step: a multi-GB padded tail costs ~30 reads.
pub fn ts_content_len<R: Read + Seek>(r: &mut R, head: &[u8], total: u64) -> Option<u64> {
    let Some(Shape::TransportStream(TsLayout { stride, offset })) = shape(head) else {
        return None;
    };
    let (stride, offset) = (stride as u64, offset as u64);
    let packets = total.checked_sub(offset)? / stride;
    let mut sync_at = |i: u64| -> bool {
        let mut b = [0u8; 1];
        r.seek(SeekFrom::Start(offset + i * stride)).is_ok()
            && r.read_exact(&mut b).is_ok()
            && b[0] == TS_SYNC
    };
    if packets == 0 || sync_at(packets - 1) {
        return None;
    }
    // Invariant: packet `lo` is synced (the head proved packet 0 is), packet `hi` is not.
    let (mut lo, mut hi) = (0u64, packets - 1);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if sync_at(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // The packet that starts at the last sync (plus, for a 204-byte stride, its parity bytes;
    // an M2TS stride's 4-byte timestamp sits before the sync and is counted by `offset`).
    let tail = (TS_PACKET as u64).max(stride - offset);
    Some((offset + lo * stride + tail).min(total))
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
        if let Some(video) = scan_video_packet(pkt, &mut pmt_pids, &mut sniffed) {
            return Some(video);
        }
    }
    sniffed
}

/// Fold one transport packet into the `video_pid` walk: record a PAT's program map PIDs, return
/// the video PID of a program map table, or remember the first PID whose packets open a video PES.
fn scan_video_packet(
    pkt: &[u8],
    pmt_pids: &mut Vec<u16>,
    sniffed: &mut Option<u16>,
) -> Option<u16> {
    let (pid, pusi, payload) = packet_payload(pkt)?;
    if pid == 0 {
        collect_pat(payload, pusi, pmt_pids);
    } else if pmt_pids.contains(&pid) {
        if let Some(video) = pmt_video_pid(payload, pusi) {
            return Some(video);
        }
    } else if sniffed.is_none() && pusi && starts_video_pes(payload) {
        *sniffed = Some(pid);
    }
    None
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
        st2k_base::safety::log_debug("mpeg decode: child declared out-of-bounds dimensions");
        return None;
    }
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?;
    if img.width() != w || img.height() != h {
        st2k_base::safety::log_debug("mpeg decode: child returned out-of-bounds dimensions");
        return None;
    }
    Some(img)
}

/// Synthetic streams in the exact shapes the demux and slicer walk, for the fuzz harness
/// (`crate::fuzz`) and the tests below. Test-only.
#[cfg(test)]
pub(crate) mod fuzzseed;

#[cfg(test)]
mod tests;
