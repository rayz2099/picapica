/** 打本地 just dev 起来的 serve，不自己拉进程。 */
export const base = process.env.PICAPICA_URL ?? "http://127.0.0.1:8080";
export const token = process.env.PICAPICA_TOKEN ?? "picapica";

const acceptOci =
  "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json";

/** why: 集成测试假定调试进程已在，挂了就直接说，避免再起一份抢 8080。 */
export async function requireLive(): Promise<void> {
  let resp: Response;
  try {
    resp = await fetch(`${base}/api/health`);
  } catch (err) {
    const why = err instanceof Error ? err.message : String(err);
    throw new Error(`picapica 没起来，先 just dev（${why}）`);
  }
  if (!resp.ok) {
    throw new Error(`picapica health ${resp.status}，先 just dev`);
  }
}

export function get(path: string, init?: RequestInit): Promise<Response> {
  return fetch(`${base}${path}`, init);
}

export function authed(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers);
  headers.set("Authorization", `Bearer ${token}`);
  const next: RequestInit = { ...init, headers };
  return fetch(`${base}${path}`, next);
}

/** why: OCI 的 GET/HEAD 必须共用 Accept，避免边缘用例偏离真实客户端协商。 */
export function oci(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers);
  headers.set("Accept", acceptOci);
  const next: RequestInit = { ...init, headers };
  return get(path, next);
}

export function ociGet(path: string): Promise<Response> {
  return oci(path);
}

export function sha256(bytes: ArrayBuffer | Uint8Array): string {
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
  return hasher.digest("hex");
}

export async function readOk(resp: Response, label: string): Promise<ArrayBuffer> {
  if (!resp.ok) throw new Error(`${label} HTTP ${resp.status}: ${await resp.text()}`);
  return resp.arrayBuffer();
}
