import { useEffect, useState } from "react";

/**
 * 视频播放：Blob URL + 原生 &lt;video&gt;，与 ImageView 同一套「Blob URL 接到标签上」的模式。
 * 编解码器支持取决于 WebView 自身（比如 Windows WebView2 对部分编码的支持有限）——
 * 这是平台限制，这里只负责把标签接上，播不出来时交给浏览器的 onerror 兜底提示。
 */
export default function VideoView({ bytes, mime }: { bytes: Uint8Array; mime: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState(false);

  useEffect(() => {
    const u = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [bytes, mime]);

  return (
    <div className="rd-video-root">
      {err ? (
        <div className="rd-error">视频无法播放（文件损坏，或编解码器不受支持）</div>
      ) : (
        url && (
          <video className="rd-video" src={url} controls preload="metadata" onError={() => setErr(true)} />
        )
      )}
    </div>
  );
}
