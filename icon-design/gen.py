# -*- coding: utf-8 -*-
"""NexusTerm 应用图标母版生成器。

结构参考 PowerShell 图标：底板 + 一块深色斜块 + 纯白提示符，元素压到最少。
这条路成立的原因：前面几版（灯塔、绳环、毛线 N）都栽在同一件事上——细节太密。
缩到 32px 时渐变、纹理、细笔画会被平均掉；能活下来的只有
**大面积实心块 + 一根粗笔画**，任何尺寸都有足够的像素预算。

这里不加渐变、不加阴影、不加描边。全部差异只落在品牌色和角度上。

用法：
    python gen.py                      # 产出 icon.svg
    node render.mjs                    # 光栅化成 icon-1024.png（需要本机 Chrome）
    npx tauri icon icon-1024.png -o out
"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))

S = 1024
R = 232        # 底板圆角（≈ 22.5%，Win11 观感）
BW = 576       # 斜块宽
BH = 502       # 斜块高
SKEW = 112     # 上下边错位量 → 斜边倾角 ≈ 12.6°
SW = 68        # 符号笔画宽
REACH = 166    # `>` 的水平跨度
SPAN = 230     # `>` 的垂直跨度
BAR = 92       # `_` 长度（约为 `>` 宽度的 55%）
GAP = 16       # `>` 下臂末端与 `_` 之间的缝
MX, MY = 26, 8  # 符号组整体偏移：斜块重心偏右上，几何居中会读成"偏左上"

# 品牌色。底板用饱和薄荷而不是近白——近白底在浅色任务栏上会整块融进背景，
# 视觉上只剩里面那块深色，等于图标凭空小一圈。
MINT = "#5DCAA5"
INK = "#0F2A23"


def svg() -> str:
    cx, cy = S / 2, S / 2

    # 平行四边形：上边右移，左边界成 "/"，与 PowerShell 同向
    x0, x1 = cx - BW / 2, cx + BW / 2
    y0, y1 = cy - BH / 2, cy + BH / 2
    pts = (f"{x0 + SKEW / 2:.1f},{y0:.1f} {x1 + SKEW / 2:.1f},{y0:.1f} "
           f"{x1 - SKEW / 2:.1f},{y1:.1f} {x0 - SKEW / 2:.1f},{y1:.1f}")

    # 让 `>` 与 `_` 的视觉包围盒（含圆头）在块内严格居中，两者底边齐平
    tip_x = cx + (REACH - BAR - GAP) / 2 + MX
    left_x = tip_x - REACH
    top_y, bot_y = cy - SPAN / 2 + MY, cy + SPAN / 2 + MY

    body = [f'  <rect width="{S}" height="{S}" rx="{R}" fill="{MINT}"/>',
            f'  <polygon points="{pts}" fill="{INK}"/>',
            f'  <path d="M {left_x:.1f} {top_y:.1f} L {tip_x:.1f} {(top_y + bot_y) / 2:.1f} '
            f'L {left_x:.1f} {bot_y:.1f}" fill="none" stroke="#FFFFFF" '
            f'stroke-width="{SW}" stroke-linecap="round" stroke-linejoin="round"/>',
            f'  <path d="M {tip_x + GAP:.1f} {bot_y:.1f} '
            f'L {tip_x + GAP + BAR:.1f} {bot_y:.1f}" fill="none" stroke="#FFFFFF" '
            f'stroke-width="{SW}" stroke-linecap="round"/>']

    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{S}" height="{S}" '
            f'viewBox="0 0 {S} {S}">\n'
            f'  <title>NexusTerm</title>\n' + "\n".join(body) + "\n</svg>\n")


if __name__ == "__main__":
    out = os.path.join(HERE, "icon.svg")
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write(svg())
    print("wrote", os.path.basename(out))
