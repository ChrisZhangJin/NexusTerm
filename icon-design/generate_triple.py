#!/usr/bin/env python3
"""生成 NexusTerm 的 ">>>" 版图标（两个变体）。

几何与 icon-a-aurora 完全共用：1024 画布、824 圆角底、同一套配色，
只把主体从 ">_" 换成三个横向排列的 ">"。

用法：python3 generate_triple.py   → 写出 icon-d-stack.svg / icon-e-trail.svg
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

# 三个 chevron 的几何参数
Y = 512           # 垂直中心
HALF = 168        # 单个 chevron 的上下跨度
DEPTH = 112       # 尖端横向伸出
STROKE = 58       # 笔画粗细
STEP = 196        # 相邻 chevron 起点间距
X0 = 512 - (2 * STEP + DEPTH) / 2   # 使整组（含笔画）水平居中


def chevron(idx: int) -> str:
    x = X0 + idx * STEP
    return f"M {x:.1f} {Y - HALF} L {x + DEPTH:.1f} {Y} L {x:.1f} {Y + HALF}"


D = {
    "key": "d",
    "name": "stack",
    "bg_from": "#1A2A48",
    "bg_to": "#070B16",
    "glow": "#5EEAD4",
    "mark_from": "#8CE0FF",
    "mark_to": "#2DD4BF",
    "opacity": (1.0, 1.0, 1.0),
}

E = {
    "key": "e",
    "name": "trail",
    "bg_from": "#1A2A48",
    "bg_to": "#070B16",
    "glow": "#5EEAD4",
    "mark_from": "#8CE0FF",
    "mark_to": "#2DD4BF",
    "opacity": (0.38, 0.66, 1.0),
}

TEMPLATE = """<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0.55" y2="1">
      <stop offset="0" stop-color="{bg_from}"/>
      <stop offset="1" stop-color="{bg_to}"/>
    </linearGradient>
    <radialGradient id="halo" cx="0.3" cy="0.18" r="0.68">
      <stop offset="0" stop-color="{glow}" stop-opacity="0.16"/>
      <stop offset="1" stop-color="{glow}" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.10"/>
      <stop offset="0.28" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="mark" x1="{x0}" y1="{y1}" x2="{x1}" y2="{y2}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{mark_from}"/>
      <stop offset="1" stop-color="{mark_to}"/>
    </linearGradient>
    <clipPath id="clip"><path d="{squircle}"/></clipPath>
  </defs>

  <path d="{squircle}" fill="url(#bg)"/>
  <g clip-path="url(#clip)">
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#halo)"/>
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#sheen)"/>
  </g>

  <g stroke="url(#mark)" stroke-width="{stroke}" stroke-linecap="round"
     stroke-linejoin="round" fill="none">
    <path d="{c0}" stroke-opacity="{o0}"/>
    <path d="{c1}" stroke-opacity="{o1}"/>
    <path d="{c2}" stroke-opacity="{o2}"/>
  </g>

  <path d="{squircle}" fill="none" stroke="#FFFFFF" stroke-opacity="0.11" stroke-width="2.5"/>
</svg>
"""


def main():
    for p in (D, E):
        svg = TEMPLATE.format(
            size=SIZE,
            squircle=SQUIRCLE,
            stroke=STROKE,
            x0=X0,
            y1=Y - HALF,
            x1=X0 + 2 * STEP + DEPTH,
            y2=Y + HALF,
            c0=chevron(0),
            c1=chevron(1),
            c2=chevron(2),
            o0=p["opacity"][0],
            o1=p["opacity"][1],
            o2=p["opacity"][2],
            **p,
        )
        path = os.path.join(OUT, f"icon-{p['key']}-{p['name']}.svg")
        with open(path, "w") as f:
            f.write(svg)
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
