# Position and Scale Algorithm

## Coordinate model

For one card, every normalized source coordinate `u` on one axis is mapped independently:

```text
observed = scale × u + offset
```

The four fiducial centers have known normalized coordinates. Their detected centers are fitted with least squares. With three or four usable markers, residuals above the screen-size-dependent tolerance are removed before a second fit.

The nominal mapping fills the captured frame:

```text
nominal_scale = frame_extent
scale_ratio = scale / nominal_scale
stretch_percent = (scale_ratio - 1) × 100
```

X and Y are fitted independently, so horizontal and vertical stretch are reported separately.

## Two offset representations

Let `c` be the frame center. The fitted transform can be rewritten as:

```text
observed = c + scale × (u - c) + center_offset
center_offset = offset + scale / 2 - frame_extent / 2
```

This is the **scale around center, then translate** representation.

The same transform can also be written as:

```text
observed = scale × (u + pre_scale_offset) + origin_offset
pre_scale_offset = center_offset / scale_ratio
```

This is the **translate, then scale** representation. Both produce the same observed frame. A single static frame therefore identifies the affine map but cannot prove which physical pipeline stage happened first. The application reports both parameterizations so the correction ratios remain inspectable.

## Letter OCR

The marker alphabet is rendered with a 5×7 block template. After orientation correction and removal of the four fiducial components, the glyph bounding box is divided into 35 cells. Each cell is sampled at its center and compared with templates A–G. Confidence combines template agreement with the margin over the second-best glyph.

The recognizer expects exactly the generated marker language, so it does not depend on OCR language packs, network access, or Tesseract.

## Stability

Three frames are captured per card. For the selected orientation, the center offset is measured independently on each frame. The exported jitter is the sample standard deviation of the three X and Y offsets. This is a short observation window intended to expose unstable capture geometry, not a long-term drift profile.

## Deliberate exclusions

- Compression loss is ignored.
- The transform is axis-aligned; rotation is handled only in 90° increments.
- Perspective and shear are outside the model.
- A card with fewer than two independent marker coordinates on either axis cannot produce a scale estimate and must be corrected manually.

