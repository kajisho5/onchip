#!/usr/bin/env python3
"""Generate synthetic benchmark fixtures with FFmpeg. No real footage, ever.

Usage: python3 bench/fixtures/gen.py [--seconds 30] [--out bench/fixtures]
Produces:
  fixture_bars_1080p_nv12.mp4      H.264, 8-bit, BT.709 limited
  fixture_bars_2160p_nv12.mp4
  fixture_bars_1080p_p010.mp4      HEVC, 10-bit
  fixture_bars_2160p_p010.mp4
  logo_sample.png                  256x128 RGBA overlay
"""
import argparse, pathlib, shutil, subprocess, sys

def run(cmd):
    print("+", " ".join(cmd)); subprocess.run(cmd, check=True)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=int, default=30)
    ap.add_argument("--out", default=str(pathlib.Path(__file__).parent))
    ap.add_argument("--ffmpeg", default="ffmpeg")
    a = ap.parse_args()
    if shutil.which(a.ffmpeg) is None:
        sys.exit("ffmpeg not found on PATH (it is only used to *generate* fixtures)")
    out = pathlib.Path(a.out); out.mkdir(parents=True, exist_ok=True)
    # moving color bars + zoom/pan + film-grain-ish noise so the encoder has real work to do
    src = ("testsrc2=size={w}x{h}:rate=30,"
           "zoompan=z='1.0+0.05*sin(on/60)':d=1:x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':s={w}x{h},"
           "noise=alls=8:allf=t")
    specs = [
        ("1080p", 1920, 1080, "nv12", "libx264", "yuv420p"),
        ("2160p", 3840, 2160, "nv12", "libx264", "yuv420p"),
        ("1080p", 1920, 1080, "p010", "libx265", "yuv420p10le"),
        ("2160p", 3840, 2160, "p010", "libx265", "yuv420p10le"),
    ]
    for tag, w, h, fmt, codec, pix in specs:
        dst = out / f"fixture_bars_{tag}_{fmt}.mp4"
        run([a.ffmpeg, "-y", "-f", "lavfi", "-i", src.format(w=w, h=h), "-t", str(a.seconds),
             "-c:v", codec, "-pix_fmt", pix, "-preset", "medium", "-crf", "18",
             "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
             "-color_range", "tv", "-movflags", "+faststart", str(dst)])
    run([a.ffmpeg, "-y", "-f", "lavfi", "-i", "color=c=0x80FF40@0.9:size=256x128,format=rgba",
         "-frames:v", "1", str(out / "logo_sample.png")])
    print("fixtures written to", out)

if __name__ == "__main__":
    main()
