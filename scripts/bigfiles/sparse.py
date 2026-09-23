"""Sparse output files for the big-file gate's growers: a hole costs no disk."""

import ctypes
import msvcrt
import os
from ctypes import wintypes

FSCTL_SET_SPARSE = 0x000900C4


CHUNK = 1 << 20


def _set_sparse(f):
    handle = msvcrt.get_osfhandle(f.fileno())
    returned = wintypes.DWORD()
    ok = ctypes.windll.kernel32.DeviceIoControl(
        wintypes.HANDLE(handle), FSCTL_SET_SPARSE, None, 0, None, 0, ctypes.byref(returned), None)
    if not ok:
        raise OSError("FSCTL_SET_SPARSE failed")


class SparseWriter:
    """A seekable file whose all-zero writes become holes instead of disk writes."""

    def __init__(self, path):
        self.f = open(path, "w+b")
        _set_sparse(self.f)

    def write(self, b):
        # `count` runs in C; `any(b)` walks a megabyte as Python ints and made a 5 GB twin
        # take minutes.
        if b and bytes(b).count(0) == len(b):
            self.f.seek(len(b), os.SEEK_CUR)
            return len(b)
        return self.f.write(b)

    def zeros(self, n):
        self.f.seek(n, os.SEEK_CUR)

    def tell(self):
        return self.f.tell()

    def seek(self, *a):
        return self.f.seek(*a)

    def flush(self):
        self.f.flush()

    def close(self):
        end = self.f.tell()
        self.f.seek(0, os.SEEK_END)
        if self.f.tell() < end:
            # A trailing hole still has to count as file length. NOT `truncate`: on Windows it
            # extends by WRITING zeros (the CRT's _chsize), 5 GB of real disk writes per twin.
            # One real byte at the very end leaves everything before it a hole.
            self.f.seek(end - 1)
            self.f.write(b"\0")
        self.f.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
