import * as pulumi from "@pulumi/pulumi";
import { describe, expect, it } from "vitest";
import { egress, environment, workerEnvironment } from "./dither.ts";

const base = {
  hostname: "dither.example",
  node: "box",
  oidc: { issuer: "https://auth.example", clientId: "dither", clientSecret: "s" },
  workerToken: "t",
  bucket: { url: "s3://pics", endpoint: "https://r2.example", accessKeyId: "a", secretAccessKey: "b" },
};

const env = (args: Parameters<typeof environment>[0]) =>
  Object.fromEntries(environment(args).map((e) => [e.name, e.value]));

describe("environment", () => {
  it("keeps a floor of free space on the node's disk", () => {
    expect(env({ ...base, minFreeBytes: 1024 }).MIN_FREE_BYTES).toBe("1024");
  });

  it("refuses a bucket outside the cluster over plain HTTP", () => {
    expect(() =>
      environment({ ...base, bucket: { url: "s3://p", endpoint: "http://s3.example", accessKeyId: "a", secretAccessKey: "b" } }),
    ).toThrow("https");
  });

  it("leaves the disk floor to the service's default unless given one", () => {
    expect(env(base).MIN_FREE_BYTES).toBeUndefined();
  });

  it("uses a bucket when given one", () => {
    const e = env({
      ...base,
      bucket: { url: "s3://pics", endpoint: "https://r2.example", accessKeyId: "a", secretAccessKey: "b" },
    });
    expect(e.STORAGE).toBe("s3://pics");
    expect(e.AWS_ENDPOINT).toBe("https://r2.example");
    expect(e.AWS_REGION).toBe("auto");
  });

  it("takes videos by default, and can be told not to or given limits", () => {
    expect(env(base).VIDEO).toBeUndefined();
    expect(env({ ...base, video: false }).VIDEO).toBe("off");
    const e = env({ ...base, maxVideoBytes: 2e9, maxVideoSeconds: 600 });
    expect(e.MAX_VIDEO_BYTES).toBe("2000000000");
    expect(e.MAX_VIDEO_SECONDS).toBe("600");
  });

  it("gives a worker the queue and none of the store or the sign-in", () => {
    const w = Object.fromEntries(
      workerEnvironment({ name: "lady", queue: "http://q:8081", token: "t", videoThreads: 4, pictures: 0 }).map(
        (e) => [e.name, e.value],
      ),
    );
    expect(w.QUEUE_URL).toBe("http://q:8081");
    expect(w.VIDEO_THREADS).toBe("4");
    expect(w.PICTURE_WORKERS).toBe("0");
    expect(w.VIDEO_WORKERS).toBeUndefined();
    expect(Object.keys(w).some((k) => k.startsWith("OIDC") || k.startsWith("AWS") || k === "STORAGE")).toBe(false);
  });

  it("serves the worker API to workers on other nodes", () => {
    expect(env(base).WORKER_ADDR).toBe("0.0.0.0:8081");
  });

  it("gives the worker the disk's floor, which ffmpeg's files may not breach", () => {
    const w = Object.fromEntries(
      workerEnvironment({ name: "lady", queue: "http://q:8081", token: "t", minFreeBytes: 1024 }).map((e) => [
        e.name,
        e.value,
      ]),
    );
    expect(w.MIN_FREE_BYTES).toBe("1024");
  });

  it("reaches a bucket in the cluster over plain HTTP, with a cache on disk", () => {
    const e = env({
      ...base,
      bucket: {
        url: "s3://dither",
        endpoint: "http://garage:3900",
        region: "garage",
        accessKeyId: "a",
        secretAccessKey: "b",
        peer: { podLabels: { app: "garage" }, port: 3900 },
      },
      diskCacheBytes: 4096,
    });
    expect(e.AWS_ALLOW_HTTP).toBe("true");
    expect(e.AWS_REGION).toBe("garage");
    expect(e.DISK_CACHE_BYTES).toBe("4096");
    expect(env({ ...base, bucket: { url: "s3://p", endpoint: "https://r2", accessKeyId: "a", secretAccessKey: "b" } }).AWS_ALLOW_HTTP).toBeUndefined();
  });

  it("passes over a CDN's edges when there is one", async () => {
    const ranges = env({ ...base, trustedProxies: ["173.245.48.0/20", "2400:cb00::/32"] }).TRUSTED_PROXIES;
    expect(await new Promise((r) => pulumi.output(ranges).apply(r))).toBe("173.245.48.0/20,2400:cb00::/32");
    expect(env(base).TRUSTED_PROXIES).toBeUndefined();
    expect(env({ ...base, addressHeader: "CF-Connecting-IP" }).ADDRESS_HEADER).toBe("CF-Connecting-IP");
  });

  it("links to the public domain when there is one", () => {
    expect(env(base).PUBLIC_URL).toBe("https://dither.example");
    expect(env({ ...base, publicHostname: "pics.example" }).PUBLIC_URL).toBe("https://pics.example");
  });

  it("exchanges codes in the cluster when told how", () => {
    const backchannel = { url: "http://hydra", podLabels: { app: "hydra" }, port: 4444 };
    expect(env({ ...base, oidc: { ...base.oidc, backchannel } }).OIDC_TOKEN_URL).toBe("http://hydra/oauth2/token");
    expect(env(base).OIDC_TOKEN_URL).toBeUndefined();
  });

  it("never puts a secret in the plain environment", () => {
    const e = environment({
      ...base,
      bucket: { url: "s3://pics", endpoint: "https://r2.example", accessKeyId: "KEY", secretAccessKey: "SECRET" },
    });
    expect(JSON.stringify(e)).not.toMatch(/KEY|SECRET|"s"|"t"/);
  });
});

describe("egress", () => {
  const backchannel = { url: "http://hydra", podLabels: { app: "hydra" }, port: 4444 };
  const peers = (args: Parameters<typeof egress>[0]) =>
    egress(args).flatMap((rule): Record<string, unknown>[] => [...rule.to]);

  it("reaches the identity provider in its own namespace when it has one", () => {
    const oidc = { ...base.oidc, backchannel: { ...backchannel, namespace: "auth" } };
    expect(peers({ oidc, bucket: base.bucket })).toContainEqual({
      podSelector: { matchLabels: { app: "hydra" } },
      namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "auth" } },
    });
    expect(peers({ oidc: { ...base.oidc, backchannel }, bucket: base.bucket })).toContainEqual({
      podSelector: { matchLabels: { app: "hydra" } },
    });
  });

  it("leaves the cluster only when something out there is needed", () => {
    const internet = (args: Parameters<typeof egress>[0]) => peers(args).some((p) => "ipBlock" in p);
    const bucket = { url: "s3://pics", endpoint: "https://r2.example", accessKeyId: "a", secretAccessKey: "b" };
    const inCluster = { ...bucket, endpoint: "http://garage:3900", peer: { podLabels: { app: "garage" }, port: 3900 } };
    expect(internet({ oidc: { ...base.oidc, backchannel }, bucket: inCluster })).toBe(false);
    expect(internet({ oidc: { ...base.oidc, backchannel }, bucket })).toBe(true);
    expect(internet({ oidc: base.oidc, bucket: inCluster })).toBe(true);
  });

  it("reaches a bucket in the cluster, and nothing beyond it", () => {
    const bucket = {
      url: "s3://dither",
      endpoint: "http://garage:3900",
      accessKeyId: "a",
      secretAccessKey: "b",
      peer: { podLabels: { app: "garage" }, port: 3900 },
    };
    const rules = egress({ oidc: { ...base.oidc, backchannel }, bucket });
    expect(rules.some((r) => r.to.some((p) => "ipBlock" in p))).toBe(false);
    expect(
      rules.some(
        (r) =>
          r.ports.some((p) => p.port === 3900) &&
          r.to.some((p) => "podSelector" in p && JSON.stringify(p.podSelector) === JSON.stringify({ matchLabels: { app: "garage" } })),
      ),
    ).toBe(true);
  });
});
