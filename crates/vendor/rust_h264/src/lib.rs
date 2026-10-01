//! Pure Rust H.264/AVC video decoder.
//!
//! A standalone, portable software H.264 decoder. Supports Baseline, Main,
//! High and High 10 profile (4:2:0, 8- to 14-bit samples), with both CAVLC
//! and CABAC entropy coding, B-frames, multi-reference, weighted prediction,
//! long-term references, and multi-slice frames. Decoded planes hold one
//! `u16` per sample; `Frame::bit_depth_luma` / `bit_depth_chroma` give the
//! sample bit depth.
//!
//! # Quick start
//!
//! ```no_run
//! use rust_h264::decoder::Decoder;
//! use rust_h264::nal::parse_annex_b;
//!
//! let bitstream = std::fs::read("input.h264").unwrap();
//! let nals = parse_annex_b(&bitstream);
//! let mut decoder = Decoder::new();
//!
//! for nal in &nals {
//!     if let Ok(Some(frame)) = decoder.decode_nal(nal) {
//!         // `frame` has u16 y/u/v planes (4:2:0), width, height, bit depths,
//!         // pic_order_cnt
//!     }
//! }
//! if let Some(frame) = decoder.flush() {
//!     // final pending frame
//! }
//! ```
//!
//! # Input formats
//!
//! Two parsers are provided in the [`nal`] module:
//!
//! - [`nal::parse_annex_b`] for start-code delimited streams (`.h264` files,
//!   RTP, broadcast TS).
//! - [`nal::parse_avcc`] + [`nal::parse_avcc_config`] for length-prefixed
//!   streams from MP4/MKV containers.
//!
//! Both produce [`nal::NalUnit`] values that can be fed to
//! [`decoder::Decoder::decode_nal`].
//!
//! # Frame ordering
//!
//! [`decoder::Decoder::decode_nal`] returns frames in **decode order**, which
//! differs from display order whenever B-frames are present. To present
//! frames in display order, sort by `pic_order_cnt` within each GOP.
//!
//! Note that `decode_nal` returns the *previous* picture when it sees a new
//! one, so be careful about IDR boundary tracking — increment your GOP
//! counter **after** the call, not before. See `examples/dump_frames.rs` and
//! `examples/play.rs` for working patterns.
//!
//! # Not supported
//!
//! - Interlaced coding (MBAFF, field pictures)
//! - 4:2:2 / 4:4:4 / monochrome chroma formats, lossless (transform bypass)
//! - SP/SI slice types
//! - Slice groups / FMO

#[allow(dead_code)]
mod bitstream;
mod cabac;
mod cabac_tables;
mod cavlc;
#[allow(dead_code)]
mod deblock;
mod decode_cabac;
mod decode_cavlc;
pub mod decoder;
#[allow(dead_code)]
mod dpb;
pub mod error;
mod inter_pred;
mod intra_pred;
mod mv_pred;
pub mod nal;
mod neighbor;
#[cfg(feature = "dev-internals")]
#[allow(dead_code)]
pub mod pps;
#[cfg(not(feature = "dev-internals"))]
#[allow(dead_code)]
mod pps;

#[allow(dead_code)]
mod residual;
#[allow(dead_code)]
mod sei;

#[cfg(feature = "dev-internals")]
#[allow(dead_code)]
pub mod slice;
#[cfg(not(feature = "dev-internals"))]
#[allow(dead_code)]
mod slice;

mod slice_context;

#[cfg(feature = "dev-internals")]
#[allow(dead_code)]
pub mod sps;
#[cfg(not(feature = "dev-internals"))]
#[allow(dead_code)]
mod sps;
