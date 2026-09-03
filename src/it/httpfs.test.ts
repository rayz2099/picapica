import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, test } from "bun:test";
import { authed, get, readOk, requireLive, sha256 } from "./client.ts";

/** why: 只声明断言所需字段，避免测试复制完整控制面模型。 */
interface NamespaceRow {
  repo: string;
  namespace: string;
  objects: number;
  bytes: number;
}

/** why: 哈希必须来自上游签名索引，不能用本地再算一遍当期望值。 */
function sha256Section(text: string): Map<string, string> {
  const start = text.indexOf("\nSHA256:\n");
  if (start < 0) throw new Error("InRelease 没有 SHA256 段");
  const body = text.slice(start + "\nSHA256:\n".length);
  const end = body.search(/\n[A-Z][A-Za-z0-9]+:/);
  const block = end >= 0 ? body.slice(0, end) : body;
  const out = new Map<string, string>();
  for (const line of block.split("\n")) {
    const m = line.trim().match(/^([0-9a-f]{64})\s+\d+\s+(\S+)$/);
    if (m) out.set(m[2], m[1]);
  }
  if (out.size === 0) throw new Error("SHA256 段为空");
  return out;
}

function checksumAttr(xml: string, type: string): { href: string; hash: string } {
  const data = xml.match(new RegExp(`<data type="${type}"[\\s\\S]*?<\\/data>`));
  if (!data) throw new Error(`repomd 没有 ${type}`);
  const href = data[0].match(/<location[^>]*href="([^"]+)"/)?.[1];
  const hash = data[0].match(/<checksum[^>]*>([0-9a-f]{64})<\/checksum>/)?.[1];
  if (!href || !hash) throw new Error(`repomd ${type} 缺 location 或 checksum`);
  return { href, hash };
}

function firstDeb(packages: string): { file: string; hash: string } {
  const blocks = packages.split("\n\n");
  for (const block of blocks) {
    const size = Number(block.match(/^Size:\s+(\d+)$/m)?.[1] ?? Number.MAX_SAFE_INTEGER);
    const file = block.match(/^Filename:\s+(\S+)$/m)?.[1];
    const hash = block.match(/^SHA256:\s+([0-9a-f]{64})$/m)?.[1];
    if (file && hash && size > 0 && size < 400_000) return { file, hash };
  }
  throw new Error("Packages 里没有小于 400KB 的 .deb");
}

describe("httpfs", () => {
  beforeAll(requireLive);

  test("ubuntu jammy index and a .deb match InRelease hashes", async () => {
    const irResp = await get("/ubuntu/dists/jammy/InRelease");
    expect(irResp.status).toBe(200);
    const ir = await irResp.text();
    expect(ir.includes("BEGIN PGP SIGNED MESSAGE")).toBe(true);
    const sums = sha256Section(ir);
    const pkgPath = "main/binary-amd64/Packages.gz";
    const wantPkg = sums.get(pkgPath);
    expect(wantPkg).toBeDefined();

    const gzPath = `/ubuntu/dists/jammy/${pkgPath}`;
    const gz = new Uint8Array(await readOk(await get(gzPath), gzPath));
    expect(sha256(gz)).toBe(wantPkg);

    const listing = gunzipSync(gz).toString("utf8");
    const deb = firstDeb(listing);
    const debPath = `/ubuntu/${deb.file}`;
    const bytes = await readOk(await get(debPath), debPath);
    expect(sha256(bytes)).toBe(deb.hash);

    const ns = deb.file.split("/").slice(0, 3).join("/");
    const nsResp = await authed(
      `/api/namespaces?q=${encodeURIComponent(ns)}&repo=ubuntu`,
    );
    expect(nsResp.status).toBe(200);
    const rows = (await nsResp.json()) as NamespaceRow[];
    expect(rows.some((row) => row.repo === "ubuntu" && row.namespace === ns)).toBe(
      true,
    );

    const parent = ns.split("/").slice(0, 2).join("/");
    const leaf = ns.split("/")[2];
    const treePath = `/api/tree?repo=ubuntu&prefix=${encodeURIComponent(parent)}`;
    const treeResp = await authed(treePath);
    expect(treeResp.status).toBe(200);
    const tree = (await treeResp.json()) as { entries: { name: string }[] };
    expect(tree.entries.some((row) => row.name === leaf)).toBe(true);
  }, 120_000);

  test("fedora repomd primary checksum matches", async () => {
    const rel = "releases/42/Everything/x86_64/os";
    const mdPath = `/fedora/${rel}/repodata/repomd.xml`;
    const xml = new TextDecoder().decode(await readOk(await get(mdPath), mdPath));
    expect(xml.includes("<repomd")).toBe(true);
    const primary = checksumAttr(xml, "primary");
    const href = primary.href.replace(/^\.\//, "");
    const filePath = `/fedora/${rel}/${href}`;
    const bytes = await readOk(await get(filePath), filePath);
    expect(sha256(bytes)).toBe(primary.hash);
  }, 120_000);
});
