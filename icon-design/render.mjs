// 把 gen.py 产出的 icon.svg 光栅化成 icon-1024.png。
// 用宿主的 playwright-core 驱动本机 Chrome（sharp 在这台机器上装不上）。
import { createRequire } from "module";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const HOSTED = "C:/Users/dell/.workbuddy/binaries/node/workspace";
const requireHosted = createRequire(path.join(HOSTED, "noop.js"));
const { chromium } = requireHosted("playwright-core");
const CHROME = "C:/Program Files/Google/Chrome/Application/chrome.exe";

const dir = path.dirname(fileURLToPath(import.meta.url));
const src = path.join(dir, "icon.svg");
if (!fs.existsSync(src)) {
  console.error("missing icon.svg — run: python gen.py");
  process.exit(1);
}

const inline = fs.readFileSync(src, "utf8")
  .replace('width="1024" height="1024"', 'width="100%" height="100%"');

const html = `<!doctype html><meta charset="utf-8">
<style>html,body{margin:0;padding:0;background:transparent;overflow:hidden}svg{display:block;width:100vw;height:100vh}</style>${inline}`;

const browser = await chromium.launch({ executablePath: CHROME });
const page = await browser.newPage({ viewport: { width: 1024, height: 1024 } });
await page.setContent(html);
await page.screenshot({ path: path.join(dir, "icon-1024.png"), omitBackground: true });
console.log("rendered icon-1024.png");
await browser.close();
