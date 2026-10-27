import { useEffect, useState } from "react";
import { mediaErrorDetail } from "./mediaError";

/**
 * 视频播放：Blob URL + 原生 &lt;video&gt;，与 ImageView 同一套「Blob URL 接到标签上」的模式。
 * 编解码器支持取决于 WebView 自身——这是平台限制，这里只负责把标签接上，
 * 播不出来时把浏览器给出的原始错误（Linux 上常是 GStreamer 缺插件的提示）显示出来。
 */
export default function VideoView({ bytes, mime }: { bytes: Uint8Array; mime: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    const u = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [bytes, mime]);

  return (
    <div className="rd-video-root">
      {err ? (
        <div className="rd-error">视频无法播放：{err}</div>
      ) : (
        url && (
          <video
            className="rd-video"
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
