// 把 icon-design/*.svg 渲染成 1024×1024 透明 PNG，并输出 32px 小尺寸预览
// 用法: node render.mjs
// sharp 不在本仓库依赖里，通过 SHARP_DIR 指向已安装 sharp 的目录（默认本机托管目录）
import { createRequire } from "module";
import { readdirSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const sharpDir = process.env.SHARP_DIR || "/Users/chris/.workbuddy/binaries/node/workspace";
const sharp = createRequire(join(sharpDir, "noop.js"))("sharp");

const dir = dirname(fileURLToPath(import.meta.url));
const files = readdirSync(dir).filter((f) => f.endsWith(".svg") && f.startsWith("icon-"));

for (const f of files) {
  const base = f.replace(/\.svg$/, "");
  // density 提升以让 VectorEffect/滤镜在栅格化时有足够采样
  await sharp(join(dir, f), { density: 384 })
    .resize(1024, 1024, { fit: "contain", background: { r: 0, g: 0, b: 0, alpha: 0 } })
    .png({ compressionLevel: 9 })
    .toFile(join(dir, `${base}.png`));

  await sharp(join(dir, f), { density: 384 })
    .resize(32, 32, { fit: "contain", background: { r: 0, g: 0, b: 0, alpha: 0 } })
    .png()
    .toFile(join(dir, `${base}-32.png`));

  console.log(`rendered ${base}.png (1024) + ${base}-32.png`);
}
