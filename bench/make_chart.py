#!/usr/bin/env python3
"""Render a rift-vs-opencode benchmark SVG from bench/results.json.

A repo-friendly dark-theme SVG: headline cards (success, prompt tokens,
wall time) with proportional bars for each agent. results.json accumulates
runs across models, so pass --model to chart one model's suite.
"""
import argparse
import json
import os

ROOT = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(ROOT, "results.json")
OUT = os.path.join(ROOT, "..", "docs", "assets", "benchmark-50.svg")

BG, CARD, BORDER = "#0d1117", "#161b22", "#30363d"
FG, DIM = "#e6edf3", "#8b949e"
RIFT_C, OC_C, GOOD = "#39c5cf", "#8b949e", "#3fb950"


def agg(results, agent):
    rs = [r for r in results if r["agent"] == agent]
    return {
        "n": len(rs),
        "ok": sum(1 for r in rs if r["ok"]),
        "prompt": sum(r["prompt_tok"] for r in rs),
        "out": sum(r["output_tok"] for r in rs),
        "secs": round(sum(r["secs"] for r in rs), 1),
        "calls": sum(r["llm_calls"] for r in rs),
    }


def fmt_tok(n):
    if n >= 1_000_000:
        return f"{n/1_000_000:.2f}M"
    return f"{n/1000:.0f}k" if n >= 10000 else str(n)


def fmt_time(secs):
    if secs >= 90 * 60:
        return f"{secs//3600:.0f}h {secs%3600/60:.0f}m"
    return f"{secs/60:.0f}m {secs%60:.0f}s"


def bar(x, y, w_frac, width, color, label, value):
    bw = max(4, int(width * w_frac))
    return (
        f'<text x="{x}" y="{y + 11}" font-size="13" fill="{DIM}">{label}</text>'
        f'<rect x="{x + 86}" y="{y}" width="{bw}" height="16" rx="4" fill="{color}"/>'
        f'<text x="{x + 86 + bw + 8}" y="{y + 13}" font-size="13" font-weight="600" fill="{FG}">{value}</text>'
    )


def main():
    ap = argparse.ArgumentParser(description="Render benchmark SVG")
    ap.add_argument("results", nargs="?", default=RESULTS)
    ap.add_argument("out", nargs="?", default=OUT)
    ap.add_argument("--model", default="gemma4:26b",
                    help="chart only this model's entries from results.json")
    ap.add_argument("--server", default="Ollama",
                    help="server named in the subtitle (Ollama, vLLM, ...)")
    args = ap.parse_args()

    with open(args.results) as f:
        results = json.load(f)
    results = [r for r in results if r.get("model") == args.model]
    rift, oc = agg(results, "rift"), agg(results, "opencode")
    n = rift["n"]

    tok_save = (1 - rift["prompt"] / oc["prompt"]) * 100 if oc["prompt"] else 0
    speedup = oc["secs"] / rift["secs"] if rift["secs"] else 0

    cards = [
        ("tasks solved", f'{rift["ok"]}/{n}', f'{oc["ok"]}/{n}',
         rift["ok"] / n, oc["ok"] / n,
         f'{"+" if rift["ok"] >= oc["ok"] else ""}{rift["ok"] - oc["ok"]} tasks'),
        ("prompt tokens (wire-measured)", fmt_tok(rift["prompt"]), fmt_tok(oc["prompt"]),
         rift["prompt"] / max(rift["prompt"], oc["prompt"]),
         oc["prompt"] / max(rift["prompt"], oc["prompt"]),
         f'−{tok_save:.0f}% tokens'),
        ("wall time", fmt_time(rift["secs"]), fmt_time(oc["secs"]),
         rift["secs"] / max(rift["secs"], oc["secs"]),
         oc["secs"] / max(rift["secs"], oc["secs"]),
         f'{speedup:.1f}× faster'),
    ]

    W, H = 1280, 560
    parts = [
        f'<svg viewBox="0 0 {W} {H}" xmlns="http://www.w3.org/2000/svg" '
        f'font-family="-apple-system, \'Segoe UI\', Helvetica, Arial, sans-serif">',
        f'<rect width="{W}" height="{H}" rx="14" fill="{BG}"/>',
        f'<text x="55" y="56" font-size="28" font-weight="700" fill="{FG}">rift <tspan fill="{RIFT_C}">vs opencode</tspan> — {n}-task suite</text>',
        f'<text x="55" y="84" font-size="14" fill="{DIM}">same model ({args.model}) · same {args.server} server · same prompts · tokens measured on the wire by a recording proxy</text>',
        f'<text x="{W-55}" y="56" font-size="13" fill="{DIM}" text-anchor="end">github.com/exYze/rift</text>',
    ]

    card_w, card_h, gap = 380, 330, 30
    x0 = (W - 3 * card_w - 2 * gap) / 2
    y0 = 120
    bar_w = card_w - 200
    for i, (title, rv, ov, rfrac, ofrac, delta) in enumerate(cards):
        x = x0 + i * (card_w + gap)
        parts.append(f'<rect x="{x}" y="{y0}" width="{card_w}" height="{card_h}" rx="10" fill="{CARD}" stroke="{BORDER}" stroke-width="1.5"/>')
        parts.append(f'<text x="{x+24}" y="{y0+38}" font-size="15" font-weight="600" fill="{FG}">{title}</text>')
        parts.append(f'<text x="{x+24}" y="{y0+102}" font-size="40" font-weight="700" fill="{RIFT_C}">{rv}</text>')
        parts.append(f'<text x="{x+24}" y="{y0+126}" font-size="13" fill="{DIM}">rift</text>')
        parts.append(f'<text x="{x+card_w-24}" y="{y0+102}" font-size="28" font-weight="600" fill="{DIM}" text-anchor="end">{ov}</text>')
        parts.append(f'<text x="{x+card_w-24}" y="{y0+126}" font-size="13" fill="{DIM}" text-anchor="end">opencode</text>')
        parts.append(bar(x + 24, y0 + 160, rfrac, bar_w, RIFT_C, "rift", rv))
        parts.append(bar(x + 24, y0 + 190, ofrac, bar_w, OC_C, "opencode", ov))
        parts.append(f'<rect x="{x+24}" y="{y0+240}" width="{card_w-48}" height="40" rx="8" fill="{BG}" stroke="{GOOD}" stroke-width="1"/>')
        parts.append(f'<text x="{x+card_w/2}" y="{y0+266}" font-size="17" font-weight="700" fill="{GOOD}" text-anchor="middle">{delta}</text>')

    parts.append(
        f'<text x="{W/2}" y="{H-30}" font-size="12.5" fill="{DIM}" text-anchor="middle">'
        f'{n} tasks: planted bugs, missing features, multi-file fixes, refactors — pass/fail decided by per-task verification scripts · '
        f'rift {rift["calls"]} LLM calls vs opencode {oc["calls"]} · full methodology in docs/BENCHMARKS.md</text>'
    )
    parts.append("</svg>")

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(parts) + "\n")
    print(f"wrote {args.out}")
    print(f"rift:     {rift}")
    print(f"opencode: {oc}")


if __name__ == "__main__":
    main()
