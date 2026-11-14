#!/usr/bin/env python3
"""Summarise a layout-audit selftest log (numbers only). usage: audit_summary.py <log>"""
import json, sys
docs = []
errs = []
for line in open(sys.argv[1], errors="replace"):
    if line.startswith("[webview:result]"):
        try:
            d = json.loads(line.split("] ", 1)[1])
        except Exception:
            continue
        if "audit" in d:
            docs.append(d)
    elif line.startswith("[webview:error]") or line.startswith("[webview:console]"):
        errs.append(line.strip()[:140])
tot = dict(pages=0, overlays=0, slots=0, failed=0, overlapPairs=0, clipped=0, shrunk=0, tiny=0, offPage=0, empty=0, fallbacks=0, nestedPairs=0)
bad_pages = []
for d in docs:
    row = dict(pages=0, overlays=0, slots=0, overlapPairs=0, clipped=0, shrunk=0, tiny=0, offPage=0, empty=0, fallbacks=0, nestedPairs=0)
    for p in d["audit"]["pages"]:
        row["pages"] += 1
        for k in ("overlays", "slots", "overlapPairs", "clipped", "shrunk", "tiny", "offPage", "fallbacks", "nestedPairs"):
            row[k] += p.get(k, 0)
        tot["failed"] += p["failed"]
        if p["overlays"] == 0:
            row["empty"] += 1
        if p["overlapPairs"] or p["clipped"] or p["tiny"] or p["offPage"]:
            bad_pages.append((d["label"], p["page"], p["overlays"], p["overlapPairs"], p["clipped"], p["tiny"], p["offPage"], p["minRatio"]))
    for k in row:
        tot[k] += row[k]
    print(d["label"], d["audit"]["totalPages"], "pages total |", row)
print("TOTAL", tot)
print("pages with any breakage:", len(bad_pages), "of", tot["pages"])
for b in bad_pages[:60]:
    print("  %s p%-4d overlays=%-3d overlap=%-2d clipped=%-2d tiny=%-2d offPage=%-2d minRatio=%s" % b)
print("webview errors:", len(errs), errs[:3])
