//! The Matroska element ids this reader knows.

// EBML / Matroska element IDs (full IDs incl. the length-marker, as a big-endian integer).
// `pub(crate)`: `crate::fuzz`'s synthetic_mkv seed builds the same element tree these parsers
// walk, and used to keep its own drifting copy of this table (see `encode_vint`/`elem` below
// for the rest of that consolidation).
pub(crate) const ID_EBML: u64 = 0x1A45_DFA3;

pub(crate) const ID_SEGMENT: u64 = 0x1853_8067;

pub(super) const ID_SEEKHEAD: u64 = 0x114D_9B74;

pub(super) const ID_SEEK: u64 = 0x4DBB;

pub(super) const ID_SEEK_ID: u64 = 0x53AB;

pub(super) const ID_SEEK_POSITION: u64 = 0x53AC;

pub(crate) const ID_INFO: u64 = 0x1549_A966;

pub(crate) const ID_TIMECODE_SCALE: u64 = 0x2AD7B1;

pub(crate) const ID_DURATION: u64 = 0x4489;

pub(crate) const ID_TRACKS: u64 = 0x1654_AE6B;

pub(crate) const ID_TRACK_ENTRY: u64 = 0xAE;

pub(crate) const ID_TRACK_NUMBER: u64 = 0xD7;

pub(crate) const ID_TRACK_TYPE: u64 = 0x83;

/// `TrackEntry ▸ Video`, and the projection sub-tree inside it that carries rotation
/// (issue #32 — Matroska's answer to the MP4 display matrix).
pub(super) const ID_VIDEO: u64 = 0xE0;

pub(super) const ID_PROJECTION: u64 = 0x7670;

/// `ProjectionPoseRoll`. **0x7675, verified against a file ffmpeg actually wrote** — the first
/// draft of this used 0x7BBD, which is not a projection element at all, and the consequence
/// would have been the worst kind: an id that never matches makes this read silently return
/// "upright", which is indistinguishable from a video that IS upright. Nothing would have
/// failed; rotation would simply never have worked.
pub(super) const ID_PROJECTION_POSE_ROLL: u64 = 0x7675;

pub(crate) const ID_CUES: u64 = 0x1C53_BB6B;

pub(crate) const ID_CUE_POINT: u64 = 0xBB;

pub(crate) const ID_CUE_TIME: u64 = 0xB3;

pub(crate) const ID_CUE_TRACK_POSITIONS: u64 = 0xB7;

pub(crate) const ID_CUE_TRACK: u64 = 0xF7;

pub(crate) const ID_CUE_CLUSTER_POSITION: u64 = 0xF1;

pub(crate) const ID_CLUSTER: u64 = 0x1F43_B675;

pub(crate) const ID_CLUSTER_TIMECODE: u64 = 0xE7;

pub(super) const ID_SIMPLE_BLOCK: u64 = 0xA3;

pub(super) const ID_BLOCK_GROUP: u64 = 0xA0;

pub(super) const ID_BLOCK: u64 = 0xA1;

pub(super) const ID_REFERENCE_BLOCK: u64 = 0xFB;

pub(crate) const ID_CODEC_ID: u64 = 0x86;

pub(crate) const ID_CODEC_PRIVATE: u64 = 0x63A2;

pub(crate) const ID_ATTACHMENTS: u64 = 0x1941_A469;

pub(crate) const ID_ATTACHED_FILE: u64 = 0x61A7;

pub(crate) const ID_FILE_NAME: u64 = 0x466E;

pub(crate) const ID_FILE_MIME: u64 = 0x4660;

pub(crate) const ID_FILE_DATA: u64 = 0x465C;

pub(super) const TRACK_TYPE_VIDEO: u64 = 1;
