"""Whether a grown twin draws what its normal-size file draws (see bigfiles.py)."""

import os

import numpy as np
from PIL import Image


def load(path):
    if not path or not os.path.isfile(path):
        return None
    try:
        return np.asarray(Image.open(path).convert("RGB"), dtype=np.float32)
    except OSError:
        return None


def sharpness(a):
    g = a.mean(axis=2)
    return float(np.abs(np.diff(g, axis=0)).mean() + np.abs(np.diff(g, axis=1)).mean())


def budget(surface, normal_ms):
    floor = {"cli": 10000, "thumb": 10000, "pane": 15000, "quick": 15000}[surface]
    return max(floor, 3 * normal_ms + 3000)


def judge_reference(a, b):
    """A big picture against its reference, each fitted by its own surface: the same shape, and
    the same picture once both are brought to the smaller size."""
    (ah, aw), (bh, bw) = a.shape[:2], b.shape[:2]
    if abs(aw / ah - bw / bh) > 0.05 * (aw / ah):
        return "FAIL", f"a {bw}x{bh} picture where the reference is {aw}x{ah}"
    w, h = min(aw, bw), min(ah, bh)
    fit = lambda x: np.asarray(Image.fromarray(x.astype(np.uint8)).resize((w, h), Image.BOX), np.float32)
    a, b = fit(a), fit(b)
    diff = float(np.abs(a - b).mean())
    sa, sb = sharpness(a), sharpness(b)
    if diff > 12 or (sa > 1 and sb / sa < 0.7):
        return "FAIL", f"not the reference picture (mean diff {diff:.1f}, sharpness {sb:.1f} vs {sa:.1f})"
    return "PASS", ""


def judge(surface, normal, big, reference=False, same_frame=True):
    """normal/big = (png path or None, ms, dims or None). ('PASS'|'FAIL'|'SKIP', why). With
    `reference`, `normal` is the reference picture of a genuinely big file (the pixel axis).
    Without `same_frame` (a video grown by repeating its content), the big twin is a longer
    video and may rightly show another frame of it: its size and sharpness are still judged,
    its pixels are not compared."""
    n_png, n_ms, n_dims = normal
    b_png, b_ms, b_dims = big
    a, b = load(n_png), load(b_png)
    if a is None:
        return "SKIP", "the normal-size sample renders nothing here either"
    if b is None:
        return "FAIL", "nothing rendered"
    if reference:
        verdict, why = judge_reference(a, b)
        if verdict == "FAIL":
            return verdict, why
        if b_ms > budget(surface, n_ms) * 4:
            return "FAIL", f"too slow: {b_ms} ms against {n_ms} ms for the reference"
        return "PASS", f"{b_ms} ms"
    if n_dims and b_dims and n_dims != b_dims:
        return "FAIL", f"a {b_dims[0]}x{b_dims[1]} picture where the normal file gets {n_dims[0]}x{n_dims[1]}"
    if a.shape != b.shape:
        return "FAIL", f"a {b.shape[1]}x{b.shape[0]} picture where the normal file gets {a.shape[1]}x{a.shape[0]}"
    diff = float(np.abs(a - b).mean()) if same_frame else 0.0
    sa, sb = sharpness(a), sharpness(b)
    if diff > 6 or (sa > 1 and sb / sa < 0.85):
        return "FAIL", f"a different or blurrier picture (mean diff {diff:.1f}, sharpness {sb:.1f} vs {sa:.1f})"
    if b_ms > budget(surface, n_ms):
        return "FAIL", f"too slow: {b_ms} ms against {n_ms} ms at normal size"
    return "PASS", f"{b_ms} ms"
