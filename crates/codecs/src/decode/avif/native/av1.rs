//! One AV1 picture through `rav1d` (the Rust port of dav1d), and a bounds-checked view of its
//! planes. The decoder's C-shaped API is used exactly as dav1d documents it: open, send the
//! item's OBUs, take the one picture, release it.

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::container::Nclx;

/// dav1d's "not yet, call again" (`DAV1D_ERR(EAGAIN)`).
const EAGAIN: i32 = -11;
/// Most send/receive rounds one still picture may take; a real one takes two or three.
const MAX_ROUNDS: usize = 256;

/// How the chroma planes are subsampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    Mono,
    I420,
    I422,
    I444,
}

/// One plane's row: 8-bit samples, or 16-bit for a 10/12-bit picture.
pub(super) enum Row<'a> {
    Eight(&'a [u8]),
    Sixteen(&'a [u16]),
}

impl Row<'_> {
    #[inline]
    pub(super) fn get(&self, x: usize) -> u16 {
        match self {
            Row::Eight(r) => u16::from(r[x]),
            Row::Sixteen(r) => r[x],
        }
    }
}

/// A decoded picture, released when dropped.
pub(super) struct Frame {
    pic: Dav1dPicture,
    /// Each plane's first row, checked non-null when the picture arrived.
    planes: [*const u8; 3],
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bits: u8,
    pub(super) layout: Layout,
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { dav1d_picture_unref(NonNull::new(&mut self.pic)) };
    }
}

impl Frame {
    /// Width and height of plane `plane` (0 = luma).
    pub(super) fn plane_size(&self, plane: usize) -> (u32, u32) {
        let (w, h) = (self.width, self.height);
        match (plane, self.layout) {
            (0, _) | (_, Layout::I444) => (w, h),
            (_, Layout::I420) => (w.div_ceil(2), h.div_ceil(2)),
            (_, Layout::I422) => (w.div_ceil(2), h),
            (_, Layout::Mono) => (0, 0),
        }
    }

    /// Row `y` of plane `plane`. Panics past the plane, like a slice index.
    pub(super) fn row(&self, plane: usize, y: u32) -> Row<'_> {
        let (w, h) = self.plane_size(plane);
        assert!(
            y < h && plane < 3,
            "row {y} of plane {plane} is past the picture"
        );
        let stride = self.pic.stride[usize::from(plane > 0)];
        let base = self.planes[plane];
        // SAFETY: dav1d allocated `h` rows of `stride` bytes for this plane, each holding at
        // least `w` samples of the picture's width (checked against the stride in `decode`),
        // and the picture stays referenced until `self` drops.
        unsafe {
            let row = base.offset(stride * y as isize);
            if self.bits > 8 {
                Row::Sixteen(std::slice::from_raw_parts(row as *const u16, w as usize))
            } else {
                Row::Eight(std::slice::from_raw_parts(row, w as usize))
            }
        }
    }

    /// The colour description in the AV1 sequence header (what a file without `nclx` has).
    pub(super) fn sequence_colour(&self) -> Option<Nclx> {
        // SAFETY: the sequence header lives as long as the picture referencing it.
        let hdr = unsafe { self.pic.seq_hdr?.as_ref() };
        Some(Nclx {
            primaries: hdr.pri as u16,
            transfer: hdr.trc as u16,
            matrix: hdr.mtrx as u16,
            full_range: hdr.color_range != 0,
        })
    }
}

/// Decode the one picture in `obus` (an AVIF item's data: a sequence header and a frame) on at
/// most `threads` threads, refusing a frame larger than `max_pixels`.
pub(super) fn decode(obus: &[u8], threads: u32, max_pixels: u32) -> Option<Frame> {
    unsafe {
        let mut settings = MaybeUninit::<Dav1dSettings>::zeroed();
        dav1d_default_settings(NonNull::new(settings.as_mut_ptr())?);
        let mut settings = settings.assume_init();
        settings.n_threads = threads.clamp(1, 16) as i32;
        settings.max_frame_delay = 1;
        settings.frame_size_limit = max_pixels;
        settings.all_layers = 0;
        let mut ctx: Option<Dav1dContext> = None;
        if dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut settings)).0 != 0 {
            return None;
        }
        let frame = receive(ctx, obus);
        close(ctx);
        frame
    }
}

/// Close a decoder and free it on this thread, once its worker threads have let go of it.
///
/// `dav1d_close` only tells rav1d's workers to stop (dav1d's own close joins them). Each holds
/// the decoder until it has stopped, and the last one frees the decoder with its frame buffers
/// and task queues (some 170 KB for a 512x384 picture) on its own thread, whenever it gets
/// there: in the middle of whatever this thread does next. Holding a reference of our own
/// through the close leaves that free to us. The wait is bounded like `spawn_budgeted`'s, for a
/// worker the OS holds up; past it the decoder is freed by whichever thread lets go last.
unsafe fn close(ctx: Option<Dav1dContext>) {
    const WORKERS_WAIT: Duration = Duration::from_millis(500);
    let Some(raw) = ctx else { return };
    // `raw` is `dav1d_open`'s and used for nothing after this: the one reference it held goes
    // to `dav1d_close` below, and `ours` is a second.
    let decoder = raw.into_arc();
    let ours = Arc::clone(&decoder);
    dav1d_close(NonNull::new(&mut Some(Dav1dContext::from_arc(decoder))));
    let start = Instant::now();
    while Arc::strong_count(&ours) > 1 && start.elapsed() < WORKERS_WAIT {
        std::thread::yield_now();
    }
}

/// Send `obus` to an open decoder and take its first picture.
unsafe fn receive(ctx: Option<Dav1dContext>, obus: &[u8]) -> Option<Frame> {
    let mut data = MaybeUninit::<Dav1dData>::zeroed().assume_init();
    let buf = dav1d_data_create(NonNull::new(&mut data), obus.len());
    if buf.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(obus.as_ptr(), buf, obus.len());
    let mut pic = MaybeUninit::<Dav1dPicture>::zeroed().assume_init();
    let mut got = false;
    for _ in 0..MAX_ROUNDS {
        if data.sz > 0 {
            let sent = dav1d_send_data(ctx, NonNull::new(&mut data)).0;
            if sent != 0 && sent != EAGAIN {
                break;
            }
        }
        match dav1d_get_picture(ctx, NonNull::new(&mut pic)).0 {
            0 => {
                got = true;
                break;
            }
            EAGAIN => {}
            _ => break,
        }
    }
    if data.sz > 0 {
        dav1d_data_unref(NonNull::new(&mut data));
    }
    if !got {
        return None;
    }
    let layout = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => Layout::Mono,
        DAV1D_PIXEL_LAYOUT_I420 => Layout::I420,
        DAV1D_PIXEL_LAYOUT_I422 => Layout::I422,
        _ => Layout::I444,
    };
    let base = |p: usize| pic.data[p].map_or(std::ptr::null(), |d| d.as_ptr() as *const u8);
    // Built before it is checked, so a picture that fails the checks is still released.
    let frame = Frame {
        planes: [base(0), base(1), base(2)],
        width: pic.p.w.max(0) as u32,
        height: pic.p.h.max(0) as u32,
        bits: pic.p.bpc.clamp(0, 16) as u8,
        layout,
        pic,
    };
    // Every plane the conversion will read must be there and wide enough for its rows.
    let bytes = if frame.bits > 8 { 2 } else { 1 };
    let planes = if layout == Layout::Mono { 1 } else { 3 };
    let sane = frame.width > 0
        && frame.height > 0
        && (0..planes).all(|p| {
            let stride = frame.pic.stride[usize::from(p > 0)];
            !frame.planes[p].is_null()
                && stride > 0
                && stride as u64 >= u64::from(frame.plane_size(p).0) * bytes
        });
    sane.then_some(frame)
}
