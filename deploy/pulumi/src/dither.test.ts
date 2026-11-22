import { describe, expect, it } from "vitest";
import { egress, environment } from "./dither.ts";

const base = {
  hostname: "dither.example",
  node: "box",
  oidc: { issuer: "https://auth.example", clientId: "dither", clientSecret: "s" },
};

const env = (args: Parameters<typeof environment>[0]) =>
  Object.fromEntries(environment(args).map((e) => [e.name, e.value]));

describe("environment", () => {
  it("keeps images on the node's disk with a floor of free space", () => {
    const e = env({ ...base, minFreeBytes: 1024 });
    expect(e.STORAGE).toBe("file:///data/objects");
    expect(e.MIN_FREE_BYTES).toBe("1024");
    expect(e.AWS_ENDPOINT).toBeUndefined();
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
    expect(JSON.stringify(e)).not.toMatch(/KEY|SECRET|"s"/);
  });
});

describe("egress", () => {
  const backchannel = { url: "http://hydra", podLabels: { app: "hydra" }, port: 4444 };
  const peers = (args: Parameters<typeof egress>[0]) => egress(args).flatMap((rule) => rule.to);

  it("reaches the identity provider in its own namespace when it has one", () => {
    const oidc = { ...base.oidc, backchannel: { ...backchannel, namespace: "auth" } };
    expect(peers({ oidc })).toContainEqual({
      podSelector: { matchLabels: { app: "hydra" } },
      namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "auth" } },
    });
    expect(peers({ oidc: { ...base.oidc, backchannel } })).toContainEqual({
      podSelector: { matchLabels: { app: "hydra" } },
    });
  });

  it("leaves the cluster only when something out there is needed", () => {
    const internet = (args: Parameters<typeof egress>[0]) => peers(args).some((p) => "ipBlock" in p);
    const bucket = { url: "s3://pics", endpoint: "https://r2.example", accessKeyId: "a", secretAccessKey: "b" };
    expect(internet({ oidc: { ...base.oidc, backchannel } })).toBe(false);
    expect(internet({ oidc: { ...base.oidc, backchannel }, bucket })).toBe(true);
    expect(internet({ oidc: base.oidc })).toBe(true);
  });
});
