import { beforeAll, describe, expect, test } from "bun:test";
import { authed, base, readOk, requireLive, sha256 } from "./client.ts";
import { run, type CommandResult } from "./proc.ts";

const image = "library/redis";
const tag = "7.4-alpine";
const platform = "linux/amd64";
/** why: 默认 context 是远端主机，不能作为本地 Picapica 客户端。 */
const dockerContext = "desktop-linux";
const accept = [
  "application/vnd.oci.image.index.v1+json",
  "application/vnd.docker.distribution.manifest.list.v2+json",
  "application/vnd.docker.distribution.manifest.v2+json",
  "application/vnd.oci.image.manifest.v1+json",
].join(", ");

/** why: Docker inspect 是客户端公共事实，可与官方 config 的内容身份独立对照。 */
interface ImageInfo {
  Id: string;
  Architecture: string;
  Os: string;
  RootFS: { Type: string; Layers: string[] };
}

/** why: 测试只依赖仓库路由与出站声明，不复制完整配置模型。 */
interface RepoConfig {
  name: string;
  upstreams: { url: string; proxy: string }[];
}

interface IndexManifest {
  digest: string;
  platform: { architecture: string; os: string };
}

interface ImageManifest {
  config: { digest: string };
  layers: { digest: string }[];
}

interface ImageConfig {
  architecture: string;
  os: string;
  rootfs: { type: string; diff_ids: string[] };
}

interface PortlessApp {
  name: string;
  url: string;
  stop: () => Promise<void>;
}

const proxyProgram = `
  const net = require("node:net");
  const targetPort = Number(process.argv.at(-1));
  const server = net.createServer((client) => {
    const upstream = net.connect(targetPort, "127.0.0.1");
    client.pipe(upstream);
    upstream.pipe(client);
    client.on("error", () => upstream.destroy());
    upstream.on("error", () => client.destroy());
  });
  server.listen(Number(process.env.PORT), "127.0.0.1", () => {
    console.log("PICAPICA_TAILSCALE=" + process.env.PORTLESS_TAILSCALE_URL);
  });
`;

function docker(args: string[]): Promise<CommandResult> {
  return run("docker", ["--context", dockerContext, ...args]);
}

function pullDigest(result: CommandResult): string {
  const output = `${result.stdout}\n${result.stderr}`;
  const digest = output.match(/Digest:\s*(sha256:[0-9a-f]{64})/i)?.[1];
  if (!digest) throw new Error(`docker pull 没有返回 digest\n${output}`);
  return digest;
}

async function inspect(ref: string): Promise<ImageInfo> {
  const args = ["image", "inspect", "--format", "{{json .}}", ref];
  const result = await docker(args);
  return JSON.parse(result.stdout.trim()) as ImageInfo;
}

/** why: 本地已有同名镜像时 docker pull 会跳过 blob，命名空间对象数就盖不住完整制品。 */
async function removeLocal(ref: string): Promise<void> {
  const listed = await docker(["images", "-q", ref]);
  if (!listed.stdout.trim()) return;
  await docker(["rmi", "--force", ref]);
}

/** why: Portless Tailscale Serve 让 Docker 通过 tailnet 访问，不把仓库暴露到公网。 */
async function startPortless(targetPort: string): Promise<PortlessApp> {
  const name = `picapica-it-${process.pid}`;
  const args = [name, "--tailscale", "node", "-e", proxyProgram, targetPort];
  const proc = Bun.spawn(["portless", ...args], { stdout: "pipe", stderr: "pipe" });
  const reader = proc.stdout.getReader();
  const stderr = new Response(proc.stderr).text();
  const decoder = new TextDecoder();
  let output = "";
  let url = "";
  const deadline = Date.now() + 30_000;

  while (!url) {
    const left = deadline - Date.now();
    if (left <= 0) {
      proc.kill();
      throw new Error("Portless Tailscale URL 启动超时");
    }
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error("Portless 输出超时")), left);
    });
    const chunk = await Promise.race([reader.read(), timeout]);
    if (timer !== undefined) clearTimeout(timer);
    if (chunk.done) {
      const detail = await stderr;
      throw new Error(`Portless 提前退出\n${output}\n${detail}`);
    }
    output += decoder.decode(chunk.value, { stream: true });
    url = output.match(/PICAPICA_TAILSCALE=(https:\/\/\S+)/)?.[1] ?? "";
  }

  const drain = (async () => {
    while (!(await reader.read()).done) {
      // why: 持续消费输出，避免长时间 pull 时管道背压阻塞 Portless。
    }
  })();
  return {
    name,
    url,
    stop: async () => {
      proc.kill();
      await proc.exited;
      await drain;
      await stderr;
    },
  };
}

function digestOf(bytes: ArrayBuffer): string {
  return `sha256:${sha256(bytes)}`;
}

/** why: Serve 发布和 DNS/TLS 生效有短暂窗口，真实客户端只能在 tailnet 健康后进入。 */
async function waitServe(url: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  let reason = "尚未请求";
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${url}/api/health`);
      if (resp.ok) return;
      reason = `HTTP ${resp.status}`;
    } catch (err) {
      reason = err instanceof Error ? err.message : String(err);
    }
    await Bun.sleep(250);
  }
  throw new Error(`Portless Tailscale Serve 健康检查超时：${reason}`);
}

/** why: 状态清理完成后还要从公共接缝确认旧地址不能继续访问服务。 */
async function waitClosed(url: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${url}/api/health`, {
        signal: AbortSignal.timeout(2_000),
      });
      if (!resp.ok) return;
    } catch {
      return;
    }
    await Bun.sleep(250);
  }
  throw new Error(`Tailscale Serve 地址仍可访问：${url}`);
}

async function stopPortless(app?: PortlessApp): Promise<void> {
  const errors: unknown[] = [];
  if (app) {
    try {
      await app.stop();
    } catch (err) {
      errors.push(err);
    }
  }
  try {
    await run("portless", ["proxy", "stop"], 30_000);
  } catch (err) {
    errors.push(err);
  }
  if (app) {
    try {
      const serve = await run("tailscale", ["serve", "status"], 30_000);
      expect(serve.stdout.trim()).toBe("No serve config");
      const routes = await run("portless", ["list"], 30_000);
      expect(routes.stdout).not.toContain(app.name);
      await waitClosed(app.url);
      console.info(`[it-docker] Tailscale Serve closed: ${app.url}`);
    } catch (err) {
      errors.push(err);
    }
  }
  if (errors.length > 0) {
    throw new AggregateError(errors, "Portless/Tailscale Serve 清理失败");
  }
}

/** why: 对照 Hub 必须用配置声明的出站；未配置时 default 等于直连，不能另猜代理。 */
function via(proxy?: string): RequestInit {
  return proxy ? { proxy } : {};
}

async function dockerToken(proxy?: string): Promise<string> {
  const url = new URL("https://auth.docker.io/token");
  url.searchParams.set("service", "registry.docker.io");
  url.searchParams.set("scope", `repository:${image}:pull`);
  const resp = await fetch(url, via(proxy));
  if (!resp.ok) throw new Error(`Docker Hub token HTTP ${resp.status}`);
  const body = (await resp.json()) as { token?: string };
  if (!body.token) throw new Error("Docker Hub token 响应缺少 token");
  return body.token;
}

async function official(
  path: string,
  proxy: string | undefined,
  token: string,
  manifest = false,
): Promise<ArrayBuffer> {
  const headers = new Headers({ Authorization: `Bearer ${token}` });
  if (manifest) headers.set("Accept", accept);
  const resp = await fetch(`https://registry-1.docker.io/v2/${image}/${path}`, {
    headers,
    ...via(proxy),
  });
  return readOk(resp, `Docker Hub ${path}`);
}

describe("docker pull", () => {
  beforeAll(requireLive);

  test(
    "redis from picapica matches Docker Hub hashes",
    async () => {
      const cfgResp = await authed("/api/config");
      expect(cfgResp.status).toBe(200);
      const cfg = (await cfgResp.json()) as {
        proxy_url?: string;
        repos: RepoConfig[];
      };
      const repo = cfg.repos.find((row) =>
        row.upstreams.some(
          (upstream) => new URL(upstream.url).hostname === "registry-1.docker.io",
        ),
      );
      expect(repo).toBeDefined();
      const upstream = repo?.upstreams.find(
        (row) => new URL(row.url).hostname === "registry-1.docker.io",
      );
      expect(upstream?.proxy).toBe("default");
      const proxy = cfg.proxy_url || undefined;

      const repoName = repo?.name ?? "";
      const clearPath = `/api/namespaces?repo=${encodeURIComponent(repoName)}&ns=${encodeURIComponent(image)}`;
      const clear = await authed(clearPath, { method: "DELETE" });
      expect(clear.status).toBe(200);

      const targetPort = new URL(base).port || "80";
      await run("portless", ["proxy", "start", "-p", "8443"], 30_000);
      let app: PortlessApp | undefined;
      try {
        app = await startPortless(targetPort);
        await waitServe(app.url);
        console.info(`[it-docker] Tailscale Serve: ${app.url}`);
        const registry = new URL(app.url).host;
        const localRef = `${registry}/${repoName}/${image}:${tag}`;
        const manifestPath = `/v2/${repoName}/${image}/manifests/${tag}`;
        const head = await fetch(`${app.url}${manifestPath}`, {
          method: "HEAD",
          headers: { Accept: accept },
        });
        expect(head.status).toBe(200);
        expect(head.headers.get("docker-content-digest")).toMatch(
          /^sha256:[0-9a-f]{64}$/,
        );
        await removeLocal(localRef);
        const localPull = await docker(["pull", "--platform", platform, localRef]);
        const digest = pullDigest(localPull);
        const local = await inspect(localRef);

        const token = await dockerToken(proxy);
        const indexBytes = await official(`manifests/${digest}`, proxy, token, true);
        expect(digestOf(indexBytes)).toBe(digest);
        const index = JSON.parse(new TextDecoder().decode(indexBytes)) as {
          manifests: IndexManifest[];
        };
        const selected = index.manifests.find(
          (row) => row.platform.os === "linux" && row.platform.architecture === "amd64",
        );
        expect(selected).toBeDefined();

        const manifestDigest = selected?.digest ?? "";
        const manifestBytes = await official(
          `manifests/${manifestDigest}`,
          proxy,
          token,
          true,
        );
        expect(digestOf(manifestBytes)).toBe(manifestDigest);
        const manifest = JSON.parse(new TextDecoder().decode(manifestBytes)) as ImageManifest;
        const configBytes = await official(
          `blobs/${manifest.config.digest}`,
          proxy,
          token,
        );
        expect(digestOf(configBytes)).toBe(manifest.config.digest);
        const config = JSON.parse(new TextDecoder().decode(configBytes)) as ImageConfig;

        expect(local.Id).toBe(manifest.config.digest);
        expect(local.Os).toBe(config.os);
        expect(local.Architecture).toBe(config.architecture);
        expect(local.RootFS.Type).toBe(config.rootfs.type);
        expect(local.RootFS.Layers).toEqual(config.rootfs.diff_ids);
        expect(manifest.layers.length).toBeGreaterThan(0);

        const configPath = `/v2/${repoName}/${image}/blobs/${manifest.config.digest}`;
        const localConfig = await readOk(await fetch(`${app.url}${configPath}`), configPath);
        expect(digestOf(localConfig)).toBe(manifest.config.digest);

        const blobHashes = await Promise.all(
          manifest.layers.map(async (layer) => {
            const path = `/v2/${repoName}/${image}/blobs/${layer.digest}`;
            const bytes = await readOk(await fetch(`${app.url}${path}`), path);
            return digestOf(bytes);
          }),
        );
        expect(blobHashes).toEqual(manifest.layers.map((layer) => layer.digest));

        const nsPath = `/api/namespaces?q=${encodeURIComponent(image)}&repo=${encodeURIComponent(repoName)}`;
        const nsResp = await authed(nsPath);
        expect(nsResp.status).toBe(200);
        const rows = (await nsResp.json()) as {
          namespace: string;
          objects: number;
          bytes: number;
        }[];
        const cached = rows.find((row) => row.namespace === image);
        const unique = new Set([
          manifest.config.digest,
          ...manifest.layers.map((layer) => layer.digest),
        ]);
        expect(cached?.objects).toBeGreaterThanOrEqual(unique.size);
        expect(cached?.bytes).toBeGreaterThan(0);
      } finally {
        await stopPortless(app);
      }
    },
    360_000,
  );
});
