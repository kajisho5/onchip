# Reference images

Reference PNGs are generated with FFmpeg and are **not committed** (see `.gitignore`).
Regenerate them with the commands below before running `cargo test -- --ignored`.
Inputs are synthetic; never use real footage here.

```sh
# 1. synthetic source frame (BT.709 limited range, 8-bit)
ffmpeg -y -f lavfi -i "testsrc2=size=1920x1080:rate=1,format=yuv420p" -frames:v 1 \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
  tests/ref/src_1080p_yuv420p.png

# 2. scale reference (lanczos), scaled by swscale as 4:2:0 like onchip's path.
#    (Scaling the RGB PNG directly keeps full-resolution chroma, which no 4:2:0
#    pipeline can reproduce: swscale's own RGB-chain and 4:2:0-chain outputs differ
#    by ~37 dB on this source. The test therefore asserts luma PSNR only.)
ffmpeg -y -i tests/ref/src_1080p_yuv420p.png \
  -vf "format=yuv420p,scale=1280:720:flags=lanczos,format=rgb24" tests/ref/scale_720p_lanczos_yuv420.png

# 3. overlay reference
ffmpeg -y -f lavfi -i "color=c=0x80FF40@0.9:size=256x128,format=rgba" -frames:v 1 tests/ref/logo_sample.png
ffmpeg -y -i tests/ref/src_1080p_yuv420p.png -i tests/ref/logo_sample.png \
  -filter_complex "overlay=32:32" tests/ref/overlay_1080p.png
```

Pass criterion: luma PSNR ≥ 40 dB against the reference for the scale test (50 dB is the M3
target); color-conversion tests are near-exact by construction.
