/**
 * `lib/context` 的回归测试：探测结果解析（tmux / docker / kubectl）、标签自动名、提前探测的判定。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { autoTitleOf, changesForeground, parseKubectl, parseProbe, restoreCommand } from "../../src/lib/context";

const US = "\x1f";
const args = (...a: string[]) => a.join(US);

test("parseProbe：tmux 里取会话名与 pane 目录", () => {
  const c = parseProbe({ TMUX_SESSION: "ops", TMUX_PATH: "/srv", SHELL_CWD: "/home/a" })!;
  assert.equal(c.tmux, "ops");
  assert.equal(c.cwd, "/srv");
  assert.equal(autoTitleOf(c), "ops");
});

test("parseProbe：docker exec / attach / run 都认出容器名", () => {
  const exec = parseProbe({ FG_COMM: "docker", FG_ARGS: args("docker", "exec", "-it", "-u", "root", "web", "bash"), SHELL_CWD: "/x" })!;
  assert.equal(exec.docker?.container, "web");
  assert.equal(autoTitleOf(exec), "web");
  const attach = parseProbe({ FG_COMM: "docker", FG_ARGS: args("docker", "attach", "db"), SHELL_CWD: "/x" })!;
  assert.equal(attach.docker?.container, "db");
  const runNamed = parseProbe({
    FG_COMM: "docker",
    FG_ARGS: args("docker", "run", "-it", "--rm", "-v", "/a:/b", "--name", "tmp1", "ubuntu", "bash"),
    SHELL_CWD: "/x",
  })!;
  assert.equal(runNamed.docker?.container, "tmp1");
  const runImage = parseProbe({ FG_COMM: "docker", FG_ARGS: args("docker", "run", "-it", "-p", "80:80", "nginx:1.25"), SHELL_CWD: "/x" })!;
  assert.equal(runImage.docker?.container, "nginx:1.25");
  // docker ps 这类一次性命令：不算进了容器
  const ps = parseProbe({ FG_COMM: "docker", FG_ARGS: args("docker", "ps"), SHELL_CWD: "/x" })!;
  assert.equal(ps.docker, undefined);
  assert.equal(autoTitleOf(ps), undefined);
});

test("parseKubectl：pod 名、命名空间、容器；oc / k3s kubectl / pod/ 前缀", () => {
  assert.deepEqual(parseKubectl(["kubectl", "exec", "-it", "web-7d9f", "-n", "prod", "-c", "app", "--", "bash"]), {
    pod: "web-7d9f",
    namespace: "prod",
    container: "app",
  });
  assert.equal(parseKubectl(["kubectl", "-n", "prod", "exec", "-it", "pod/api-0", "--", "sh"])?.pod, "api-0");
  assert.equal(parseKubectl(["kubectl", "exec", "--namespace=kube-system", "-it", "coredns-1", "--", "sh"])?.namespace, "kube-system");
  assert.equal(parseKubectl(["/usr/local/bin/oc", "exec", "-it", "db-0", "--", "psql"])?.pod, "db-0");
  assert.equal(parseKubectl(["k3s", "kubectl", "exec", "-it", "nginx-1", "--", "sh"])?.pod, "nginx-1");
  // 不是进容器的子命令
  assert.equal(parseKubectl(["kubectl", "get", "pods"]), undefined);
  assert.equal(parseKubectl(["kubectl", "logs", "-f", "web-1"]), undefined);
  // 「--」之后是容器里的命令，不当 pod 名
  assert.equal(parseKubectl(["kubectl", "exec", "-it", "--", "bash"]), undefined);
  const c = parseProbe({ FG_COMM: "kubectl", FG_ARGS: args("kubectl", "exec", "-it", "web-7d9f", "--", "bash"), SHELL_CWD: "/x" })!;
  assert.equal(autoTitleOf(c), "web-7d9f");
});

test("autoTitleOf：普通 shell 没有自动名（标签恢复原名）", () => {
  assert.equal(autoTitleOf(parseProbe({ SHELL_CWD: "/home/a" })), undefined);
  assert.equal(autoTitleOf(null), undefined);
});

test("changesForeground：进 / 出 tmux、容器、pod 的命令才提前探测", () => {
  for (const l of ["tmux a -t ops", "sudo docker exec -it web bash", "kubectl exec -it p -- sh", "exit", "  logout", "sudo -E podman run -it x"]) {
    assert.ok(changesForeground(l), l);
  }
  for (const l of ["ls -la", "cat tmux.conf", "echo docker", "vim x"]) {
    assert.ok(!changesForeground(l), l);
  }
});

test("restoreCommand：只带目录的上下文只切目录", () => {
  assert.equal(restoreCommand({ cwd: "/srv/app", at: 0 }, "duplicate"), "cd -- '/srv/app' 2>/dev/null");
});
