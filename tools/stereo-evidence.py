#!/usr/bin/env python3
"""Measures a packed stereo frame against the mono one (design/STEREO-UI.md, "Evidence").

    QUIVER_DEMO=1 QUIVER_STEREO_SHOT=DIR QUIVER_STEREO_SHOT_NAME=library quiver-app
    tools/stereo-evidence.py DIR library [more names…]

Reads DIR/NAME-mono.png, DIR/NAME-sbs.png (two eyes side by side) and DIR/NAME-scopes.json
(each depth scope's depth, its element's bounds and the shifts the window packed it with),
and for every scope finds, by matching its own pixels in each eye against the mono frame,
how far each eye actually moved it. Writes:

- NAME-anaglyph.png: red = left eye, cyan = right eye (red/cyan glasses, or just look at the
  fringes: the wider the fringe, the nearer);
- NAME-disparity.png: the mono frame, dimmed, each measured element tinted by its total
  disparity (0 px dark blue … 7 px yellow), with a legend;
- NAME-evidence.json and a summary on stdout: per rung, how many elements measured as
  packed, and for each control how far it pops from the surface it is on.

Only "textured" pixels (edges, text, art) can tell a shift; flat fills match any shift and
are ignored. A scope's own pixels exclude the elements drawn inside it with depths of
their own, and a margin as wide as the largest shift at its sides.
"""

import json
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

MAX_DX = 9


def load(path):
    return np.asarray(Image.open(path).convert("RGB")).astype(np.int16)


def texture(img):
    g = img.mean(axis=2)
    gx = np.abs(np.diff(g, axis=1, prepend=g[:, :1]))
    gy = np.abs(np.diff(g, axis=0, prepend=g[:1, :]))
    return (gx + gy) > 18


def measure(eye, mono, mask):
    """Best integer dx (eye[x + dx] == mono[x]) over the masked pixels, its exact-match
    fraction, and the second best's (how unambiguous it is)."""
    ys, xs = np.nonzero(mask)
    if len(xs) < 24:
        return None
    w = mono.shape[1]
    best = []
    ref = mono[ys, xs]
    for dx in range(-MAX_DX, MAX_DX + 1):
        x2 = xs + dx
        ok = (x2 >= 0) & (x2 < w)
        if ok.sum() < 0.8 * len(xs):
            continue
        d = np.abs(eye[ys[ok], x2[ok]] - ref[ok]).max(axis=1)
        exact = float((d <= 2).mean())
        best.append((exact, -abs(dx), dx, float(d.mean())))
    if not best:
        return None
    best.sort(reverse=True)
    exact, _, dx, err = best[0]
    second = best[1][0] if len(best) > 1 else 0.0
    return {"dx": dx, "exact": round(exact, 3), "second": round(second, 3), "err": round(err, 2), "pixels": int(len(xs))}


def analyse(d, name):
    mono = load(d / f"{name}-mono.png")
    sbs = load(d / f"{name}-sbs.png")
    doc = json.loads((d / f"{name}-scopes.json").read_text())
    h, w = mono.shape[:2]
    eyes = [sbs[:, :w], sbs[:, w : 2 * w]]
    tex = texture(mono)
    scopes = doc["scopes"]

    def rect(s):
        b = s.get("bounds")
        if not b:
            return None
        x, y, bw, bh = b
        x0, y0 = max(0, int(np.floor(x))), max(0, int(np.floor(y)))
        x1, y1 = min(w, int(np.ceil(x + bw))), min(h, int(np.ceil(y + bh)))
        if x1 - x0 < 4 or y1 - y0 < 4:
            return None
        return x0, y0, x1, y1

    rects = [rect(s) for s in scopes]
    full = lambda r: r is not None and r[0] <= 1 and r[2] >= w - 1 and r[1] <= 1 and r[3] >= h - 1

    # The parent of each scope: the latest earlier scope whose bounds contain it.
    parents = []
    for i, r in enumerate(rects):
        p = 0
        if r is not None:
            for j in range(i - 1, 0, -1):
                q = rects[j]
                if q is not None and q[0] <= r[0] and q[1] <= r[1] and q[2] >= r[2] and q[3] >= r[3]:
                    p = j
                    break
        parents.append(p)

    results = []
    disp_map = np.full((h, w), -1, dtype=np.int16)
    for i, s in enumerate(scopes):
        r = rects[i]
        if r is None or full(r):
            continue
        x0, y0, x1, y1 = r
        m = np.zeros((h, w), dtype=bool)
        pad = 5
        if x1 - x0 <= 2 * pad + 2:
            continue
        m[y0:y1, x0 + pad : x1 - pad] = True
        # Not the parts covered by elements drawn later with their own depth (children,
        # overlays), give or take their shift.
        for j in range(i + 1, len(scopes)):
            q = rects[j]
            if q is None or full(q):
                continue
            m[max(0, q[1] - 1) : q[3] + 1, max(0, q[0] - pad) : q[2] + pad] = False
        m &= tex
        got = [measure(eyes[e], mono, m) for e in (0, 1)]
        if got[0] is None or got[1] is None:
            continue
        want = [int(round(v)) for v in s["shift"]]
        res = {
            "index": i,
            "depth": round(s["depth"], 3),
            "parent": parents[i],
            "parent_depth": round(scopes[parents[i]]["depth"], 3),
            "bounds": [x0, y0, x1 - x0, y1 - y0],
            "want": want,
            "got": [got[0]["dx"], got[1]["dx"]],
            "exact": [got[0]["exact"], got[1]["exact"]],
            "pixels": got[0]["pixels"],
        }
        res["ok"] = res["got"] == want and min(res["exact"]) >= 0.9
        results.append(res)
        if res["ok"]:
            disp_map[m] = want[0] - want[1]

    # Pops: each measured scope's disparity minus its parent's (as packed).
    by_index = {r["index"]: r for r in results}
    for r in results:
        p = r["parent"]
        pw = [int(round(v)) for v in scopes[p]["shift"]]
        r["pop_px"] = (r["got"][0] - r["got"][1]) - (pw[0] - pw[1])
        r["parent_measured"] = p in by_index and by_index[p]["ok"]

    # Images: the anaglyph and the disparity map.
    l, rr = eyes
    ana = np.zeros_like(mono)
    ana[..., 0] = l.mean(axis=2)
    ana[..., 1] = rr.mean(axis=2)
    ana[..., 2] = rr.mean(axis=2)
    Image.fromarray(ana.clip(0, 255).astype(np.uint8)).save(d / f"{name}-anaglyph.png")
    ramp = np.array([[30, 40, 110], [40, 90, 170], [40, 150, 190], [60, 180, 140], [130, 200, 80], [200, 210, 60], [240, 200, 40], [250, 240, 90]])
    base = (mono * 0.35).astype(np.int16)
    tint = base.copy()
    sel = disp_map >= 0
    col = ramp[np.clip(disp_map[sel], 0, 7)]
    tint[sel] = (base[sel] * 0.3 + col * 0.7).astype(np.int16)
    img = Image.fromarray(tint.clip(0, 255).astype(np.uint8))
    dr = ImageDraw.Draw(img)
    for k in range(8):
        dr.rectangle([16 + k * 46, h - 34, 16 + k * 46 + 40, h - 14], fill=tuple(int(c) for c in ramp[k]))
        dr.text((16 + k * 46 + 14, h - 31), f"{k}", fill=(0, 0, 0))
    dr.text((16, h - 52), "total disparity, px (measured)", fill=(230, 230, 230))
    img.save(d / f"{name}-disparity.png")

    # Text and art stay crisp: in every measured scope the eye's pixels equal mono's.
    ok = [r for r in results if r["ok"]]
    exact = [min(r["exact"]) for r in ok]
    summary = {
        "name": name,
        "eye_width": w,
        "percent": doc.get("percent"),
        "scopes": len(scopes),
        "measured": len(results),
        "as_packed": len(ok),
        "exact_min": round(min(exact), 3) if exact else None,
        "exact_mean": round(float(np.mean(exact)), 4) if exact else None,
        "rungs": {},
        "pops": {},
    }
    for r in ok:
        t = r["want"][0] - r["want"][1]
        summary["rungs"].setdefault(t, 0)
        summary["rungs"][t] += 1
        if r["pop_px"] > 0 and r["parent_measured"]:
            key = f"{r['parent_depth']}->{r['depth']}"
            summary["pops"].setdefault(key, {"count": 0, "px": set()})
            summary["pops"][key]["count"] += 1
            summary["pops"][key]["px"].add(r["pop_px"])
    for v in summary["pops"].values():
        v["px"] = sorted(v["px"])
    summary["rungs"] = dict(sorted(summary["rungs"].items()))
    bad = [r for r in results if not r["ok"]]
    (d / f"{name}-evidence.json").write_text(json.dumps({"summary": summary, "scopes": results}, indent=1))
    return summary, bad


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    d = Path(sys.argv[1])
    for name in sys.argv[2:]:
        s, bad = analyse(d, name)
        rungs = ", ".join(f"{t} px ×{n}" for t, n in s["rungs"].items())
        pops = "; ".join(f"{k}: {v['count']}× +{'/'.join(map(str, v['px']))} px" for k, v in sorted(s["pops"].items()))
        print(f"{name}: {s['measured']} of {s['scopes']} scopes measurable, {s['as_packed']} as packed "
              f"(exact pixels: min {s['exact_min']}, mean {s['exact_mean']})")
        print(f"  rungs: {rungs}")
        print(f"  pops (parent depth -> depth): {pops}")
        for r in bad[:8]:
            print(f"  ? scope {r['index']} depth {r['depth']} at {r['bounds']}: want {r['want']} got {r['got']} exact {r['exact']}")


if __name__ == "__main__":
    main()
