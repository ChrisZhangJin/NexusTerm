/**
 * 远端终端上下文感知：工作目录 / tmux 会话 / docker 容器。
 *
 * 数据来自后端 `ssh_probe`（旁路 exec 通道读 /proc，不往用户终端写任何东西），
 * 这里负责把原始 KEY=VALUE 解释成结构化上下文，并生成「还原命令」。
 */

export interface TermContext {
  /** 交互 shell（或 tmux 当前 pane）的工作目录 */
  cwd?: string;
  /** 所在 tmux 会话名 */
  tmux?: string;
  /** 正在前台运行的 docker exec / attach / run（原样 argv；只用于提示和标签名，不重放） */
  docker?: { container: string; argv: string[] };
  /** 正在前台运行的 kubectl exec（进到哪个 pod） */
  k8s?: { pod: string; namespace?: string; container?: string };
  /** 探测时间 */
  at: number;
}

const US = "\x1f"; // 后端用单元分隔符替换 argv 中的 \0

/** docker exec 的带参选项（其后紧跟一个值） */
const DOCKER_OPTS_WITH_VALUE = new Set([
  "-u", "--user", "-w", "--workdir", "-e", "--env", "--env-file", "--detach-keys",
]);

/** docker run 的带参选项（其后紧跟一个值；--name 单独处理） */
const DOCKER_RUN_OPTS_WITH_VALUE = new Set([
  "-u", "--user", "-w", "--workdir", "-e", "--env", "--env-file", "-v", "--volume", "-p", "--publish",
  "--network", "--net", "--entrypoint", "-h", "--hostname", "--mount", "--platform", "-l", "--label",
  "--add-host", "--device", "--cpus", "-m", "--memory", "--restart", "--user", "--group-add", "--ulimit",
  "--log-driver", "--log-opt", "--shm-size", "--cap-add", "--cap-drop", "--security-opt", "--pull",
]);

function parseDocker(argv: string[]): TermContext["docker"] | undefined {
  const bin = (argv[0] ?? "").split("/").pop();
  let i = 1;
  if (bin === "docker" && argv[1] === "compose") i = 2;
  else if (bin !== "docker" && bin !== "docker-compose" && bin !== "podman") return;
  // 跳过全局选项（如 -H host / --context x）
  while (i < argv.length && argv[i].startsWith("-")) {
    i += argv[i].includes("=") ? 1 : 2;
  }
  const sub = argv[i];
  if (sub === "exec" || sub === "attach") {
    i++;
    while (i < argv.length && argv[i].startsWith("-")) {
      const opt = argv[i];
      i += DOCKER_OPTS_WITH_VALUE.has(opt) ? 2 : 1;
    }
    const container = argv[i];
    if (!container) return;
    return { container, argv };
  }
  if (sub === "run") {
    // docker run -it --name web nginx bash：有 --name 用它，否则用镜像名
    let name: string | undefined;
    i++;
    while (i < argv.length && argv[i].startsWith("-")) {
      const opt = argv[i];
      if (opt === "--name") {
        name = argv[i + 1];
        i += 2;
      } else if (opt.startsWith("--name=")) {
        name = opt.slice("--name=".length);
        i++;
      } else {
        i += opt.includes("=") || !DOCKER_RUN_OPTS_WITH_VALUE.has(opt) ? 1 : 2;
      }
    }
    const container = name || argv[i];
    if (!container) return;
    return { container, argv };
  }
  return;
}

/** kubectl exec 的带参选项（其后紧跟一个值） */
const KUBECTL_OPTS_WITH_VALUE = new Set([
  "-n", "--namespace", "-c", "--container", "--context", "--kubeconfig", "-s", "--server", "--cluster",
  "--user", "--token", "--as", "--request-timeout", "-f", "--filename", "--pod-running-timeout",
]);

/** `kubectl exec -it web-7d9f -n prod -c app -- bash`（也认 oc、k3s kubectl、`pod/web-7d9f`） */
export function parseKubectl(argv: string[]): TermContext["k8s"] | undefined {
  const bin = (argv[0] ?? "").split("/").pop();
  let i = 1;
  if (bin === "k3s" || bin === "microk8s") {
    if (argv[1] !== "kubectl") return;
    i = 2;
  } else if (bin !== "kubectl" && bin !== "oc") return;
  let namespace: string | undefined;
  let container: string | undefined;
  let pod: string | undefined;
  let seenExec = false;
  for (; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--") break;
    if (a.startsWith("-")) {
      const eq = a.indexOf("=");
      const key = eq > 0 ? a.slice(0, eq) : a;
      const val = eq > 0 ? a.slice(eq + 1) : KUBECTL_OPTS_WITH_VALUE.has(key) ? argv[++i] : undefined;
      if (key === "-n" || key === "--namespace") namespace = val;
      if (key === "-c" || key === "--container") container = val;
      continue;
    }
    if (!seenExec) {
      if (a !== "exec" && a !== "attach") return; // 只认进容器的子命令
      seenExec = true;
      continue;
    }
    if (!pod) pod = a.replace(/^pods?\//, "");
  }
  if (!seenExec || !pod) return;
  return { pod, namespace, container };
}

export function parseProbe(raw: Record<string, string>): TermContext | null {
  if (!raw || Object.keys(raw).length === 0) return null;
  const ctx: TermContext = { at: Date.now() };
  const fgComm = raw.FG_COMM ?? "";
  const fgArgs = raw.FG_ARGS ? raw.FG_ARGS.split(US).filter((s) => s !== "") : [];

  if (raw.TMUX_SESSION) {
    ctx.tmux = raw.TMUX_SESSION;
    ctx.cwd = raw.TMUX_PATH || raw.SHELL_CWD;
    return ctx;
  }
  if (/^(docker|docker-compose|podman)$/.test(fgComm) && fgArgs.length) {
    ctx.docker = parseDocker(fgArgs);
  }
  if (/^(kubectl|oc|k3s|microk8s)$/.test(fgComm) && fgArgs.length) {
    ctx.k8s = parseKubectl(fgArgs);
  }
  // 前台是 vim / less 之类时，其 cwd 与 shell 一致；docker 客户端 cwd 也就是 shell cwd
  ctx.cwd = raw.SHELL_CWD || raw.FG_CWD;
  return ctx.cwd || ctx.tmux || ctx.docker || ctx.k8s ? ctx : null;
}

/** POSIX 单引号转义 */
export function shq(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/**
 * 生成还原命令主体（不含前置空格与回车，由 stealthInput 包装）。
 * - reconnect：断线恢复 —— tmux 会话还活着，直接 attach 回去
 * - duplicate：复制会话 —— 切到 pane 目录，并以分组会话（new-session -t）进入同一 tmux
 *
 * docker 只做提示、不重放：前台进程的命令行无法区分「进容器」与「一次性命令」，
 * 重放可能重复执行用户不想要的操作。
 */
export function restoreCommand(
  ctx: TermContext,
  mode: "reconnect" | "duplicate"
): string | null {
  const parts: string[] = [];
  if (ctx.cwd) parts.push(`cd -- ${shq(ctx.cwd)} 2>/dev/null`);
  if (ctx.tmux && mode === "reconnect") {
    parts.push(`tmux attach -t ${shq(ctx.tmux)}`);
  } else if (ctx.tmux) {
    // 分组会话：共享窗口但各自独立切换（不像 attach 那样两边同屏），detach 后自动销毁
    parts.push(
      `tmux new-session -t ${shq(ctx.tmux)} \\; set-option destroy-unattached on`
    );
  }
  if (parts.length === 0) return null;
  return parts.join("; ");
}

/**
 * 把还原命令包装成「不留痕」的终端输入：前置空格让 HISTCONTROL=ignorespace 不记 history；
 * printf 上移 N 行清到屏尾，抹掉「提示符 + 回显的命令」，改由界面卡片说明。
 * N = 提示符与命令按列数折行的行数（命令里的 N 本身也占宽度，故迭代到稳定）。
 */
export function stealthInput(body: string, promptCells: number, cols: number,
  widthOf: (s: string) => number): string {
  let n = 1;
  for (let i = 0; i < 4; i++) {
    const line = ` printf '\\033[${n}A\\r\\033[J'; ${body}`;
    const need = Math.max(1, Math.ceil((promptCells + widthOf(line)) / Math.max(cols, 1)));
    if (need === n) return `${line}\r`;
    n = need;
  }
  return ` printf '\\033[${n}A\\r\\033[J'; ${body}\r`;
}

/** 终端里展示的「上下文恢复」说明卡片内容：[标签, 说明] */
export function restoreNotice(
  ctx: TermContext,
  mode: "reconnect" | "duplicate"
): [string, string][] {
  const rows: [string, string][] = [];
  if (ctx.cwd) rows.push(["目录", ctx.cwd]);
  if (ctx.tmux) {
    rows.push([
      "tmux",
      mode === "reconnect"
        ? `重新附着会话 ${ctx.tmux}`
        : `进入会话 ${ctx.tmux}（分组会话：窗口共享、视图独立，关闭标签自动清理）`,
    ]);
  }
  if (ctx.docker) {
    rows.push(["docker", `原会话在容器 ${ctx.docker.container} 中，未自动进入`]);
  }
  if (ctx.k8s) {
    rows.push(["k8s", `原会话在 pod ${ctx.k8s.pod} 中，未自动进入`]);
  }
  return rows;
}

/** 状态提示用的简短描述 */
export function describe(ctx: TermContext): string {
  const bits: string[] = [];
  if (ctx.tmux) bits.push(`tmux:${ctx.tmux}`);
  if (ctx.docker) bits.push(`docker:${ctx.docker.container}`);
  if (ctx.k8s) bits.push(`pod:${ctx.k8s.pod}`);
  if (ctx.cwd) bits.push(ctx.cwd);
  return bits.join(" · ");
}

/**
 * 标签页自动名：进了 k8s pod 就叫 pod 名，进了容器就叫容器名，在 tmux 里就叫 tmux 会话名；
 * 都不是（回到普通 shell）返回 undefined，标签恢复原名。用户手动改过的名字始终优先（由调用方处理）。
 */
export function autoTitleOf(ctx: TermContext | null | undefined): string | undefined {
  if (!ctx) return undefined;
  return ctx.k8s?.pod || ctx.docker?.container || ctx.tmux || undefined;
}

/** 这条刚提交的命令会不会换掉「前台在哪」（进 / 出 tmux、容器、pod）：是就提前探测一次，不干等 15 秒的轮询 */
export function changesForeground(line: string): boolean {
  return /^\s*(sudo\s+(-\S+\s+)*)?(tmux|screen|docker|docker-compose|podman|kubectl|oc|k3s|microk8s|exit|logout)\b/.test(line);
}
