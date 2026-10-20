#!/usr/bin/env python3
"""生成 NexusTerm 图标的 SVG 源文件（三种配色）。

几何：1024 画布，824×824 圆角底（半径 185，macOS 图标网格），
主体是终端提示符 ">_"，">" 用双色渐变描边，"_" 是实心光标条。

用法：python3 generate.py   → 在当前目录写出 icon-*.svg
"""
import os

OUT = os.path.dirname(os.path.abspath(__file__))
SIZE = 1024
INSET = 100
BOX = SIZE - 2 * INSET  # 824
RADIUS = 185

# 圆角矩形路径（顺时针，从左上直线段起点开始）
L, T = INSET, INSET
Rt, B = INSET + BOX, INSET + BOX
squircle = (
    f"M {L + RADIUS},{T} H {Rt - RADIUS} "
    f"A {RADIUS},{RADIUS} 0 0 1 {Rt},{T + RADIUS} "
    f"V {B - RADIUS} A {RADIUS},{RADIUS} 0 0 1 {Rt - RADIUS},{B} "
    f"H {L + RADIUS} A {RADIUS},{RADIUS} 0 0 1 {L},{B - RADIUS} "
    f"V {T + RADIUS} A {RADIUS},{RADIUS} 0 0 1 {L + RADIUS},{T} Z"
)

# ">_" 的几何：切角 + 底部光标条，整体在画布内居中
CHEVRON = "M 320 290 L 520 512 L 320 734"
CURSOR = dict(x=580, y=666, w=180, h=68, rx=24)
STROKE = 92

PALETTES = {
    "a": {
        "name": "aurora",
        "bg_from": "#1A2A48",
        "bg_to": "#070B16",
        "glow": "#5EEAD4",
        "glow_opacity": 0.16,
        "mark_from": "#8CE0FF",
        "mark_to": "#2DD4BF",
        "cursor": "#FBBF24",
    },
    "b": {
        "name": "phosphor",
        "bg_from": "#151C12",
        "bg_to": "#050806",
        "glow": "#A3E635",
        "glow_opacity": 0.12,
        "mark_from": "#E2FCA0",
        "mark_to": "#22C55E",
        "cursor": "#ECFCCB",
    },
    "c": {
        "name": "mono",
        "bg_from": "#1D2129",
        "bg_to": "#080A0E",
        "glow": "#93C5FD",
        "glow_opacity": 0.12,
        "mark_from": "#FFFFFF",
        "mark_to": "#B4C0D0",
        "cursor": "#38BDF8",
    },
}

TEMPLATE = """<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0.55" y2="1">
      <stop offset="0" stop-color="{bg_from}"/>
      <stop offset="1" stop-color="{bg_to}"/>
    </linearGradient>
    <radialGradient id="halo" cx="0.3" cy="0.18" r="0.68">
      <stop offset="0" stop-color="{glow}" stop-opacity="{glow_opacity}"/>
      <stop offset="1" stop-color="{glow}" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.10"/>
      <stop offset="0.28" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="mark" x1="320" y1="290" x2="520" y2="734" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{mark_from}"/>
      <stop offset="1" stop-color="{mark_to}"/>
    </linearGradient>
    <clipPath id="clip">
      <path d="{squircle}"/>
    </clipPath>
  </defs>

  <!-- 底色：渐变 + 左上柔光 + 顶部高光 -->
  <path d="{squircle}" fill="url(#bg)"/>
  <g clip-path="url(#clip)">
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#halo)"/>
    <rect x="0" y="0" width="{size}" height="{size}" fill="url(#sheen)"/>
  </g>

  <!-- 提示符 >_ -->
  <path d="{chevron}" stroke="url(#mark)" stroke-width="{stroke}" stroke-linecap="round"
        stroke-linejoin="round" fill="none"/>
  <rect x="{cx}" y="{cy}" width="{cw}" height="{ch}" rx="{crx}" fill="{cursor}"/>

  <!-- 内描边：让图标在浅色桌面上也有边界 -->
  <path d="{squircle}" fill="none" stroke="#FFFFFF" stroke-opacity="0.11" stroke-width="2.5"/>
</svg>
"""


def main():
    for key, p in PALETTES.items():
        svg = TEMPLATE.format(
            size=SIZE,
            squircle=squircle,
            chevron=CHEVRON,
            stroke=STROKE,
            cx=CURSOR["x"],
            cy=CURSOR["y"],
            cw=CURSOR["w"],
            ch=CURSOR["h"],
            crx=CURSOR["rx"],
            **p,
        )
        path = os.path.join(OUT, f"icon-{key}-{p['name']}.svg")
        with open(path, "w") as f:
            f.write(svg)
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
