#!/usr/bin/env python3
"""Regenerate the synthetic DICOM fixtures used by scripts\\check-dicom.ps1.

These guard the .dcm decode path (src/decode.rs: looks_like_dicom -> `dcm:-`
+ `-auto-level`). We ship SYNTHETIC files, never real patient data:

  color_stripes.dcm  120x120 RGB, six known vertical hue stripes. The hue check
                     proves `-auto-level` stays hue-preserving (a per-channel
                     regression would desaturate/shift the warm 'gold' stripe).
  mono1_grad.dcm     120x120 MONOCHROME1 (0=white) L->R 0..255 gradient. Proves
                     magick applies the MONOCHROME1 inversion (renders white->black).

Requires: pydicom, numpy  (pip install pydicom numpy). Run from anywhere:
  python tests/fixtures/dicom/make_fixtures.py
The committed .dcm files are the output of this script; regenerate + recommit if
you change the shapes (and update the expected colours in check-dicom.ps1).
"""
import os
import numpy as np
import pydicom
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian, generate_uid, SecondaryCaptureImageStorage

HERE = os.path.dirname(os.path.abspath(__file__))
SIZE = 120

# Deterministic UIDs so regenerating doesn't churn the committed bytes.
ROOT = "1.2.826.0.1.3680043.2.1143.9999"

def _ds():
    ds = Dataset()
    m = FileMetaDataset()
    m.MediaStorageSOPClassUID = SecondaryCaptureImageStorage
    m.MediaStorageSOPInstanceUID = ROOT + ".1"
    m.TransferSyntaxUID = ExplicitVRLittleEndian
    m.ImplementationClassUID = ROOT + ".2"
    ds.file_meta = m
    ds.is_little_endian = True
    ds.is_implicit_VR = False
    ds.SOPClassUID = SecondaryCaptureImageStorage
    ds.SOPInstanceUID = ROOT + ".1"
    ds.StudyInstanceUID = ROOT + ".3"
    ds.SeriesInstanceUID = ROOT + ".4"
    ds.PatientName = "TEST^SYNTHETIC"
    ds.PatientID = "0"
    ds.Modality = "OT"
    ds.Rows = SIZE
    ds.Columns = SIZE
    ds.PixelRepresentation = 0
    return ds

def make_color():
    ds = _ds()
    a = np.zeros((SIZE, SIZE, 3), np.uint8)
    stripes = [(220, 20, 20), (20, 200, 40), (30, 50, 210),
               (180, 150, 40), (150, 150, 150), (20, 20, 20)]
    sw = SIZE // 6
    for i, c in enumerate(stripes):
        a[:, i * sw:(i + 1) * sw] = c
    ds.SamplesPerPixel = 3
    ds.PhotometricInterpretation = "RGB"
    ds.PlanarConfiguration = 0
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelData = a.tobytes()
    ds.save_as(os.path.join(HERE, "color_stripes.dcm"), enforce_file_format=True)
    print("wrote color_stripes.dcm", a.shape)

def make_mono1():
    ds = _ds()
    x = np.linspace(0, 255, SIZE).astype(np.uint8)
    a = np.tile(x, (SIZE, 1))
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME1"  # 0 = white
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelData = a.tobytes()
    ds.save_as(os.path.join(HERE, "mono1_grad.dcm"), enforce_file_format=True)
    print("wrote mono1_grad.dcm (0=white)")

if __name__ == "__main__":
    make_color()
    make_mono1()
