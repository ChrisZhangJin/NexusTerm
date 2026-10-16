/** zmodem.js 0.1.x 没有官方类型，按我们用到的面声明。 */
declare module "zmodem.js" {
  export interface ZDetection {
    confirm(): ZSession;
    deny(): void;
    is_valid(): boolean;
  }

  export interface ZTransferDetails {
    name: string;
    size: number;
    mtime: number;
    files_remaining?: number;
    bytes_remaining?: number;
  }

  export interface ZTransfer {
    get_details(): ZTransferDetails;
    get_payloads(): Uint8Array[];
    on(event: "input", fn: (octets: number[]) => void): void;
    accept(opts?: { on_progress?: (t: ZTransfer, p: number) => void }): Promise<void>;
    skip(): void;
  }

  export interface ZSession {
    type: "send" | "receive";
    start(): void;
    close(): void;
    abort(): void;
    on(event: "offer", fn: (xfer: ZTransfer) => void): void;
    on(event: "session_end", fn: () => void): void;
    on(event: "garbage", fn: (octets: number[]) => void): void;
  }

  export interface SentryOptions {
    to_terminal(octets: Iterable<number>): void;
    sender(octets: number[]): void;
    on_detect(d: ZDetection): void;
    on_retract(): void;
  }

  export class Sentry {
    constructor(options: SentryOptions);
    consume(input: Uint8Array | number[]): void;
    send_abort(): void;
  }

  export const Browser: {
    send_files(
      session: ZSession,
      files_obj: Record<string, File | Blob>,
      opts?: {
        on_offer_response?: (obj: unknown, xfer: ZTransfer) => void;
        on_file_response?: (obj: unknown, xfer: ZTransfer) => void;
        on_progress?: (obj: unknown, xfer: ZTransfer) => void;
      }
    ): Promise<unknown>;
    save_to_disk(payloads: Uint8Array[], name: string): void;
  };

  const Zmodem: {
    Sentry: typeof Sentry;
    Browser: typeof Browser;
  };
  export default Zmodem;
}
