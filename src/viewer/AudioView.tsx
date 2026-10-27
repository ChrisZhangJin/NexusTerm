import { useEffect, useState } from "react";
import { mediaErrorDetail } from "./mediaError";

/**
 * 音频播放：Blob URL + 原生 &lt;audio&gt;，与 ImageView 同一套「Blob URL 接到标签上」的模式。
 * 不做自定义播放器皮肤——浏览器自带的控件条已经够用，没必要重新造。
 */
export default function AudioView({ bytes, mime }: { bytes: Uint8Array; mime: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    const u = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [bytes, mime]);

  return (
    <div className="rd-audio-root">
      {err ? (
        <div className="rd-error">音频无法播放：{err}</div>
      ) : (
        url && (
          <audio
            className="rd-audio"
            src={url}
            controls
            preload="metadata"
            onError={(e) => setErr(mediaErrorDetail(e.currentTarget.error))}
          />
        )
      )}
    </div>
  );
}
