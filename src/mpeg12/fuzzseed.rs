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
        0x00, 0x00, 0x01, 0xBB, 0x00, 0x0C, 0x80, 0x1B, 0x91, 0x04, 0xE1, 0xFF, 0xE0, 0xE0, 0xE8,
        0xC0, 0xC0, 0x20,
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
        0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xC3, 0xFA, 0xFF,
        0xFF,
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
                0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x00,
                0x00, 0x00, 0x00,
            ],
        ));
        // PMT: PCR on the video PID, MPEG-2 video (type 0x02) on 0x0100, MPEG audio
        // (0x03) on 0x0101. The CRC-32 is zeros - nothing here verifies it, on purpose.
        ts.extend(packet(
            stride,
            0x1000,
            true,
            &[
                0x00, 0x02, 0xB0, 0x17, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x02,
                0xE1, 0x00, 0xF0, 0x00, 0x03, 0xE1, 0x01, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00,
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
