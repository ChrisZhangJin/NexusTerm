import type { IImageAddonOptions } from "@xterm/addon-image";
import type { ITerminalAddon, Terminal } from "@xterm/xterm";

/** Enable only IIP, without opting the terminal into window-size reports. */
const INLINE_IMAGE_OPTIONS = {
  iipSupport: true,
  sixelSupport: false,
  enableSizeReports: false,
  iipSizeLimit: 4_000_000, // raw file bytes per OSC
  pixelLimit: 4_000_000, // checked before and after resizing
  storageLimit: 32, // MB of cached pixels per terminal
  showPlaceholder: true,
} satisfies IImageAddonOptions;

type Host = Pick<Terminal, "loadAddon">;

let canDecode: boolean | undefined;

function wasmAvailable(): boolean {
  if (canDecode !== undefined) return canDecode;
  try {
    // The IIP decoder compiles Wasm synchronously on the first image, not at
    // activation. Probe up front so a restricted WebView doesn't fail inside write().
    new WebAssembly.Module(Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0));
    canDecode = true;
  } catch (error) {
    canDecode = false;
    console.warn("Inline images unavailable: WebAssembly compilation failed", error);
  }
  return canDecode;
}

/**
 * Load after term.open(), once per visual terminal. xterm then owns disposal.
 * Any failure leaves the terminal running without image support.
 */
export function loadInlineImages(
  term: Host,
  create: (options: IImageAddonOptions) => ITerminalAddon,
  canUseWasm: () => boolean = wasmAvailable
): boolean {
  if (!canUseWasm()) return false;
  let addon: ITerminalAddon | undefined;
  try {
    addon = create(INLINE_IMAGE_OPTIONS);
    term.loadAddon(addon);
    return true;
  } catch (error) {
    try {
      // xterm registers an addon before activate(), so dispose after failed activation too.
      addon?.dispose();
    } catch {
      /* partially activated */
    }
    console.warn("Inline images unavailable: xterm addon activation failed", error);
    return false;
  }
}
