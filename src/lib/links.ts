/**
 * 终端里的链接什么时候打开：按住 Ctrl（macOS 为 ⌘）再点。单纯点击不打开 ——
 * 点一下选文字、点一下聚焦终端都很常见，跳出浏览器会很烦。
 */
export function opensLink(e: { ctrlKey: boolean; metaKey: boolean }, mac: boolean): boolean {
  return mac ? e.metaKey : e.ctrlKey;
}
