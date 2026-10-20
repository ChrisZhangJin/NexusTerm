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
    /** 发送端：非末尾分片 */
    send(bytes: Uint8Array | number[]): void;
    /** 发送端：末尾分片，远端确认后 resolve */
    end(bytes?: Uint8Array | number[]): Promise<void>;
    /** 发送端：远端要求的起始偏移（断点续传时非 0） */
    get_offset(): number;
  }

  export interface ZOfferParams {
    name: string;
    size: number;
    mtime?: Date;
    files_remaining?: number;
    bytes_remaining?: number;
  }

  export interface ZSession {
    type: "send" | "receive";
    start(): void;
    /** 发 ZFIN 结束会话，远端回 ZFIN 后 resolve */
    close(): Promise<void>;
    abort(): void;
    /** 发送端：提交文件；远端 ZSKIP（拒收）时 resolve 为 undefined */
    send_offer(params: ZOfferParams): Promise<ZTransfer | undefined>;
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
    /** 注意：files 必须是数组（实现里按 length 倒序遍历），传对象会一个都不发 */
    send_files(
      session: ZSession,
      files: File[],
      opts?: {
        /** xfer 为 undefined 表示远端 ZSKIP（拒收） */
        on_offer_response?: (obj: File, xfer: ZTransfer | undefined) => void;
        on_progress?: (obj: File, xfer: ZTransfer, piece: Uint8Array) => void;
        on_file_complete?: (obj: File, xfer: ZTransfer) => void;
      }
    ): Promise<void>;
    save_to_disk(payloads: Uint8Array[], name: string): void;
  };

  const Zmodem: {
    Sentry: typeof Sentry;
    Browser: typeof Browser;
  };
  export default Zmodem;
}
