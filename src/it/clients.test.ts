import { beforeAll, describe, expect, test } from "bun:test";
import { base, requireLive } from "./client.ts";
import { run } from "./proc.ts";

/** why: 默认 context 是远端主机，不能作为打本机 serve 的客户端。 */
const dockerContext = "desktop-linux";

function docker(args: string[], timeoutMs = 180_000) {
  return run("docker", ["--context", dockerContext, ...args], timeoutMs);
}

function hostPort(): string {
  return new URL(base).port || "80";
}

describe("package clients", () => {
  beforeAll(requireLive);

  test("apt-get update through ubuntu repository", async () => {
    const port = hostPort();
    const script = [
      "set -euo pipefail",
      `printf 'deb http://host.docker.internal:${port}/ubuntu jammy main\\n' > /etc/apt/sources.list`,
      "rm -rf /etc/apt/sources.list.d",
      "apt-get update -o Acquire::Retries=2",
    ].join("\n");
    const result = await docker(
      [
        "run",
        "--rm",
        "--platform",
        "linux/amd64",
        "--add-host=host.docker.internal:host-gateway",
        "ubuntu:22.04",
        "bash",
        "-lc",
        script,
      ],
      180_000,
    );
    expect(result.stdout).toContain("Get:");
    expect(result.stdout.toLowerCase()).not.toContain("err:");
  }, 180_000);

  test("dnf makecache through fedora repository", async () => {
    const port = hostPort();
    const script = [
      "set -euo pipefail",
      "rm -f /etc/yum.repos.d/*.repo",
      "cat > /etc/yum.repos.d/picapica.repo <<'EOF'",
      "[picapica]",
      "name=picapica",
      `baseurl=http://host.docker.internal:${port}/fedora/releases/$releasever/Everything/$basearch/os/`,
      "enabled=1",
      "gpgcheck=0",
      "EOF",
      "dnf makecache --refresh",
    ].join("\n");
    const result = await docker(
      [
        "run",
        "--rm",
        "--add-host=host.docker.internal:host-gateway",
        "fedora:42",
        "bash",
        "-lc",
        script,
      ],
      240_000,
    );
    const out = `${result.stdout}\n${result.stderr}`;
    expect(out.toLowerCase()).toContain("metadata");
  }, 240_000);
});
