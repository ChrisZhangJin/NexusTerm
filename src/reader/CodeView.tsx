import { useEffect, useMemo, useState } from "react";
import hljs from "highlight.js/lib/core";
import json from "highlight.js/lib/languages/json";
import yaml from "highlight.js/lib/languages/yaml";
import ini from "highlight.js/lib/languages/ini";
import properties from "highlight.js/lib/languages/properties";
import xml from "highlight.js/lib/languages/xml";
import bash from "highlight.js/lib/languages/bash";
import nginx from "highlight.js/lib/languages/nginx";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import makefile from "highlight.js/lib/languages/makefile";
import plaintext from "highlight.js/lib/languages/plaintext";
import "highlight.js/styles/github.css";
import { api } from "../api";

hljs.registerLanguage("json", json);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("properties", properties);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("nginx", nginx);
hljs.registerLanguage("dockerfile", dockerfile);
hljs.registerLanguage("makefile", makefile);
hljs.registerLanguage("plaintext", plaintext);

/** 超过这个大小不做高亮（hljs 是同步的，几 MB 的日志会卡住窗口） */
const HL_LIMIT = 1024 * 1024;

type JsonState = { ok: true; pretty: string } | { ok: false; msg: string; line?: number };

/** 校验 JSON；出错时尽量给出行号（V8 的报错里有 position 或 line/column） */
function checkJson(text: string): JsonState {
  try {
    return { ok: true, pretty: JSON.stringify(JSON.parse(text), null, 2) };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    let line: number | undefined;
    const lc = /line (\d+)/i.exec(msg);
    const pos = /position (\d+)/i.exec(msg);
    if (lc) line = Number(lc[1]);
    else if (pos) line = text.slice(0, Number(pos[1])).split("\n").length;
    return { ok: false, msg, line };
  }
}

export default function CodeView({
  text,
  lang,
  isJson,
  zoom,
  onNotify,
}: {
  text: string;
  lang: string;
  isJson: boolean;
  zoom: number;
  onNotify: (msg: string) => void;
}) {
  const [wrap, setWrap] = useState(() => localStorage.getItem("nx.reader.wrap") === "1");
  const [pretty, setPretty] = useState(false);

  useEffect(() => localStorage.setItem("nx.reader.wrap", wrap ? "1" : "0"), [wrap]);

  const js = useMemo(() => (isJson ? checkJson(text) : null), [isJson, text]);
  const shown = pretty && js?.ok ? js.pretty : text;

  const html = useMemo(() => {
    if (shown.length > HL_LIMIT || !hljs.getLanguage(lang)) return null;
    try {
      return hljs.highlight(shown, { language: lang, ignoreIllegals: true }).value;
    } catch {
      return null;
    }
  }, [shown, lang]);

  // 行数：去掉末尾换行，避免多出一个空行号
  const lines = useMemo(() => {
    const n = shown.split("\n").length;
    return shown.endsWith("\n") ? n - 1 : n;
  }, [shown]);
  const errLine = js && !js.ok && !pretty ? js.line : undefined;

  const copy = async () => {
    try {
      await api.clipboardWrite(shown);
      onNotify("已复制全部内容");
    } catch (e) {
      onNotify(`复制失败：${e}`);
    }
  };

  const fs = (13 * zoom) / 100;

  return (
    <div className="rd-code-root">
      <div className="rd-img-tools">
        {js &&
          (js.ok ? (
            <span className="rd-ok">✓ JSON 语法正确</span>
          ) : (
            <span className="rd-bad" title={js.msg}>
              ✗ JSON 语法错误{js.line ? `（第 ${js.line} 行）` : ""}：{js.msg}
            </span>
          ))}
        {js?.ok && (
          <label className="rd-js">
            <input type="checkbox" checked={pretty} onChange={(e) => setPretty(e.target.checked)} />
            格式化
          </label>
        )}
        <label className="rd-js">
          <input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} />
          自动换行
        </label>
        <span className="rd-spacer" />
        <span className="rd-img-hint">
          {lang === "plaintext" ? "纯文本" : lang} · {lines} 行
          {html === null && shown.length > HL_LIMIT ? " · 文件较大，已关闭高亮" : ""}
        </span>
        <button className="rd-btn" onClick={copy}>
          复制全部
        </button>
      </div>
      <div className="rd-code-scroll">
        <div className={`rd-code ${wrap ? "wrap" : ""}`} style={{ fontSize: `${fs}px` }}>
          {!wrap && (
            <div className="rd-gutter" aria-hidden>
              {Array.from({ length: lines }, (_, i) => (
                <div key={i} className={errLine === i + 1 ? "err" : undefined}>
                  {i + 1}
                </div>
              ))}
            </div>
          )}
          {html !== null ? (
            <pre className="hljs" dangerouslySetInnerHTML={{ __html: html }} />
          ) : (
            <pre className="hljs">{shown}</pre>
          )}
        </div>
      </div>
    </div>
  );
}
