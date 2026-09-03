import { beforeAll, describe, expect, test } from "bun:test";
import { authed, get, oci, ociGet, requireLive } from "./client.ts";

/** why: 只声明断言所需字段，避免测试复制完整控制面模型。 */
interface NamespaceRow {
  repo: string;
  namespace: string;
  objects: number;
  bytes: number;
}

describe("core", () => {
  beforeAll(requireLive);

  test("health", async () => {
    const resp = await get("/api/health");
    expect(resp.status).toBe(200);
    const body = (await resp.json()) as { ok: boolean };
    expect(body.ok).toBe(true);
  });

  test("favicon is not a repo", async () => {
    const resp = await get("/favicon.ico");
    expect(resp.status).toBe(200);
    const ct = resp.headers.get("content-type") ?? "";
    expect(ct.includes("svg")).toBe(true);
  });

  test("docker path ping", async () => {
    const resp = await get("/docker/v2/");
    expect(resp.status).toBe(200);
  });

  test("docker host alias ping", async () => {
    const resp = await get("/v2/");
    expect(resp.status).toBe(200);
  });

  test("http file repository roots", async () => {
    for (const repo of ["ubuntu", "fedora"]) {
      const resp = await get(`/${repo}/`);
      expect(resp.status).toBe(200);
      expect(await resp.text()).toBe("picapica httpfs\n");
    }
  });

  test("unknown repository is not found", async () => {
    const resp = await get("/not-configured/anything");
    expect(resp.status).toBe(404);
    expect(await resp.text()).toContain("仓库不存在");
  });

  test("busybox tag, digest and HEAD", async () => {
    const path = "/docker/v2/library/busybox/manifests/latest";
    const first = await ociGet(path);
    expect(first.status).toBe(200);
    const text = await first.text();
    expect(text.includes("schemaVersion") || text.includes("manifests")).toBe(true);

    const second = await ociGet(path);
    expect(second.status).toBe(200);
    const digest = second.headers.get("docker-content-digest") ?? "";
    expect(digest).toMatch(/^sha256:[0-9a-f]{64}$/);
    const current = await second.text();

    const immutable = await ociGet(`/docker/v2/library/busybox/manifests/${digest}`);
    expect(immutable.status).toBe(200);
    expect(immutable.headers.get("docker-content-digest")).toBe(digest);
    expect(await immutable.text()).toBe(current);

    const head = await oci(path, { method: "HEAD" });
    expect(head.status).toBe(200);
    expect(head.headers.get("docker-content-digest")).toMatch(/^sha256:[0-9a-f]{64}$/);
    expect(Number(head.headers.get("content-length"))).toBeGreaterThan(0);
    expect(await head.text()).toBe("");
  }, 30000);

  test("control plane rejects missing and wrong tokens", async () => {
    const paths = ["/api/config", "/api/stats", "/api/probe", "/api/namespaces"];
    for (const path of paths) {
      const missing = await get(path);
      expect(missing.status).toBe(401);

      const wrong = await get(path, {
        headers: { Authorization: "Bearer definitely-wrong" },
      });
      expect(wrong.status).toBe(401);
    }
  });

  test("control plane reports cached namespace", async () => {
    const cached = await ociGet("/docker/v2/library/busybox/manifests/latest");
    expect(cached.status).toBe(200);

    const statsResp = await authed("/api/stats");
    expect(statsResp.status).toBe(200);
    const stats = (await statsResp.json()) as {
      artifacts: number;
      bytes: number;
      refs: number;
      repos: number;
    };
    expect(stats.artifacts).toBeGreaterThan(0);
    expect(stats.bytes).toBeGreaterThan(0);
    expect(stats.refs).toBeGreaterThan(0);
    expect(stats.repos).toBeGreaterThan(0);

    const nsResp = await authed(
      "/api/namespaces?q=library%2Fbusybox&repo=docker",
    );
    expect(nsResp.status).toBe(200);
    const rows = (await nsResp.json()) as NamespaceRow[];
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.every((row) => row.repo === "docker")).toBe(true);
    expect(rows.some((row) => row.namespace === "library/busybox")).toBe(true);
    expect(rows.every((row) => row.objects > 0 && row.bytes > 0)).toBe(true);
  });

  test("probe ranks docker", async () => {
    const resp = await authed("/api/probe", { method: "POST" });
    expect(resp.status).toBe(200);
    const ranks = (await resp.json()) as Record<string, { ok: boolean; url: string }[]>;
    expect(Array.isArray(ranks.docker)).toBe(true);
    expect(ranks.docker.some((row) => row.ok)).toBe(true);
  }, 30000);
});
