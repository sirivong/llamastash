#!/usr/bin/env python3
"""Pull stills and sub-clips out of a recorded asciinema cast.

The harness records a full tour cast; this turns it into the README assets:
single-frame PNG screenshots and a shorter GIF (e.g. just the init wizard).
Works on both v2 (absolute timestamps) and v3 (relative inter-event deltas).

Subcommands:

  timeline <cast> [--step S] [--from A] [--to B]
      Render the cast with pyte and print the screen text at every S-second
      checkpoint (default 1.0). Use it to find the cast time of a moment you
      want — the dashboard, the chat reply, a theme, the finale — before
      extracting a frame there. --from/--to bound the window.

  clip <cast> <out.cast> [--end B] [--start A]
      Write a sub-cast covering [A, B] (default A=0), re-based to start at 0,
      header preserved. Feed it to `agg` for a tighter GIF. Note: the TUI only
      repaints changed cells, so A>0 opens on a partial screen — keep A=0 for a
      clean GIF (this is the init.gif path: clip 0..~7.5s, then agg).

  frame <cast> <T> <out.png> [--font-size N]
      Extract the screen exactly as painted at cast time T to a PNG. Internally:
      clip 0..T, render with `agg`, take that GIF's last frame. Deterministic —
      no GIF-time-vs-cast-time seek guessing. Needs `agg` and ImageMagick
      (`magick`) on PATH. Repeatable per README screenshot.

Requires `pyte` (for `timeline`). `frame` also shells out to `agg` + `magick`.
Pair with `agg --font-size 16` (the canonical demo size) so stills line up with
assets/tui.gif. See also harness.py (records the cast) and trim_cast.py (evens
out its cadence)."""
import json
import os
import subprocess
import sys
import tempfile


def _load(src):
  lines = open(src).read().splitlines()
  header = json.loads(lines[0])
  v3 = header.get("version") == 3
  if v3:
    cols, rows = header["term"]["cols"], header["term"]["rows"]
  else:
    cols, rows = header["width"], header["height"]
  events = [json.loads(l) for l in lines[1:] if l.strip()]
  return lines[0], header, v3, cols, rows, events


def _cumulative(events, v3):
  """Yield (abs_time, kind, data, raw_delta_or_abs) so v2/v3 share a walk."""
  clock = 0.0
  for ev in events:
    t, kind, data = ev[0], ev[1], ev[2]
    clock = clock + t if v3 else t
    yield clock, kind, data, t


def _take(args, name, default=None):
  if name in args:
    i = args.index(name)
    val = args[i + 1]
    del args[i:i + 2]
    return val
  return default


def cmd_timeline(args):
  src = args[0]
  step = float(_take(args, "--step", "1.0"))
  lo = float(_take(args, "--from", "0"))
  hi = float(_take(args, "--to", "inf"))
  import pyte
  _, _, v3, cols, rows, events = _load(src)
  screen = pyte.Screen(cols, rows)
  stream = pyte.Stream(screen)
  next_cp = step
  print(f"# {src}: {cols}x{rows}, {len(events)} events, v3={v3}")
  for clock, kind, data, _raw in _cumulative(events, v3):
    if kind == "o":
      stream.feed(data)
    if clock >= next_cp:
      if lo <= clock <= hi:
        nonblank = sum(1 for ln in screen.display if ln.strip())
        print(f"\n===== t={clock:7.2f}s  nonblank_rows={nonblank} =====")
        for i, ln in enumerate(screen.display):
          r = ln.rstrip()
          if r:
            print(f"{i:2} {r}")
      while next_cp <= clock:
        next_cp += step
  print(f"\n# total duration: {clock:.2f}s")


def cmd_clip(args):
  src = args[0]
  dst = args[1]
  start = float(_take(args, "--start", "0"))
  end = float(_take(args, "--end", "inf"))
  header_line, _, v3, _, _, events = _load(src)
  out = [header_line]
  prev_kept = None
  base = None
  for clock, kind, data, _raw in _cumulative(events, v3):
    if clock < start or clock > end:
      continue
    if base is None:
      base = clock
    if v3:
      # re-base: first kept event lands at 0, rest keep their inter-deltas
      delta = 0.0 if prev_kept is None else round(clock - prev_kept, 6)
      out.append(json.dumps([delta, kind, data]))
    else:
      out.append(json.dumps([round(clock - base, 6), kind, data]))
    prev_kept = clock
  open(dst, "w").write("\n".join(out) + "\n")
  print(f"clip {src} [{start},{end}] -> {dst} ({len(out) - 1} events)")


def cmd_frame(args):
  src = args[0]
  t = float(args[1])
  out_png = args[2]
  font = _take(args, "--font-size", "16")
  with tempfile.TemporaryDirectory() as td:
    clip_path = os.path.join(td, "clip.cast")
    gif_path = os.path.join(td, "clip.gif")
    cmd_clip([src, clip_path, "--end", str(t)])
    subprocess.run(["agg", "--font-size", font, clip_path, gif_path], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # agg writes diff frames (GIF disposal), so the raw last frame is only the
    # delta. Coalesce to composite every frame, then drop all but the last: that
    # leaves the full screen as painted at time t.
    subprocess.run(["magick", gif_path, "-coalesce", "-delete", "0--2", out_png],
                   check=True)
  print(f"frame {src} @ {t}s -> {out_png}")


def main():
  if len(sys.argv) < 2:
    print(__doc__)
    sys.exit(2)
  sub, args = sys.argv[1], sys.argv[2:]
  {"timeline": cmd_timeline, "clip": cmd_clip, "frame": cmd_frame}.get(
      sub, lambda _a: (print(f"unknown subcommand: {sub}"), sys.exit(2)))(args)


if __name__ == "__main__":
  main()
