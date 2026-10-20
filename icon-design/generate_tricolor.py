#!/usr/bin/env python3
"""">>>" 三色版：三枚箭头分别取 A/B/C 三套方案各自的渐变。

A 极光  #8CE0FF → #2DD4BF（青→青绿）
B 磷光  #E2FCA0 → #22C55E（浅黄绿→绿）
C 素白  #FFFFFF → #B4C0D0（白→银灰）

背景沿用 A 的深海军蓝。

用法：python3 generate_tricolor.py  → icon-f-abc.svg / icon-g-cab.svg
"""
import os

OUT = os.path.dirname(os.path.abspath(__file__))
SIZE = 1024
INSET = 100
BOX = SIZE - 2 * INSET
RADIUS = 185
L, T = INSET, INSET
Rt, B = INSET + BOX, INSET + BOX

SQUIRCLE = (
    f"M {L + RADIUS},{T} H {Rt - RADIUS} "
    f"A {RADIUS},{RADIUS} 0 0 1 {Rt},{T + RADIUS} "
    f"V {B - RADIUS} A {RADIUS},{RADIUS} 0 0 1 {Rt - RADIUS},{B} "
    f"H {L + RADIUS} A {RADIUS},{RADIUS} 0 0 1 {L},{B - RADIUS} "
    f"V {T + RADIUS} A {RADIUS},{RADIUS} 0 0 1 {L + RADIUS},{T} Z"
)

Y = 512
HALF = 168
DEPTH = 112
STROKE = 58
STEP = 196
X0 = 512 - (2 * STEP + DEPTH) / 2

# 三套渐变：(起色, 止色)
GRADS = {
    "A": ("#8CE0FF", "#2DD4BF"),
    "B": ("#E2FCA0", "#22C55E"),
    "C": ("#FFFFFF", "#B4C0D0"),
}


def chevron(idx: int) -> str:
    x = X0 + idx * STEP
    return f"M {x:.1f} {Y - HALF} L {x + DEPTH:.1f} {Y} L {x:.1f} {Y + HALF}"


TEMPLATE = """<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0.55" y2="1">
      <stop offset="0" stop-color="#1A2A48"/>
      <stop offset="1" stop-color="#070B16"/>
    </linearGradient>
    <radialGradient id="halo" cx="0.3" cy="0.18" r="0.68">
      <stop offset="0" stop-color="#5EEAD4" stop-opacity="0.16"/>
      <stop offset="1" stop-color="#5EEAD4" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.10"/>
      <stop offset="0.28" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
{grad_defs}
    <clipPath id="clip"><path d="{squircle}"/></clipPath>
  </defs>

  <path d="{squircle}" fill="url(#bg)"/>
  <g clip-path="url(#clip)">
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#halo)"/>
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#sheen)"/>
  </g>

  <g stroke-width="{stroke}" stroke-linecap="round" stroke-linejoin="round" fill="none">
{strokes}
  </g>

  <path d="{squircle}" fill="none" stroke="#FFFFFF" stroke-opacity="0.11" stroke-width="2.5"/>
</svg>
"""


def build(order: str) -> str:
    grad_defs, strokes = [], []
    for i, key in enumerate(order):
        c0, c1 = GRADS[key]
        x = X0 + i * STEP
        grad_defs.append(
            f'    <linearGradient id="g{i}" x1="{x:.1f}" y1="{Y - HALF}" '
            f'x2="{x + DEPTH:.1f}" y2="{Y + HALF}" gradientUnits="userSpaceOnUse">\n'
            f'      <stop offset="0" stop-color="{c0}"/>\n'
            f'      <stop offset="1" stop-color="{c1}"/>\n'
            f"    </linearGradient>"
        )
        strokes.append(f'    <path d="{chevron(i)}" stroke="url(#g{i})"/>')
    return TEMPLATE.format(
        size=SIZE,
        squircle=SQUIRCLE,
        stroke=STROKE,
        grad_defs="\n".join(grad_defs),
        strokes="\n".join(strokes),
    )


def main():
    for key, order in (("f-abc", "ABC"), ("g-cab", "CAB"), ("h-bca", "BCA")):
        path = os.path.join(OUT, f"icon-{key}.svg")
        with open(path, "w") as f:
            f.write(build(order))
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
