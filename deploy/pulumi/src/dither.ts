import * as k8s from "@pulumi/kubernetes";
import * as pulumi from "@pulumi/pulumi";
import { VERSIONS } from "./versions.ts";

/**
 * A hostname the deployment should publish, and the workload answering it.
 * `service` is the name prefix: the Service is `<prefix>-service`.
 */
export type Route = { service: string; hostname: string };

/**
 * The OAuth client this needs registered for it. Returned rather than
 * configured, because the callback path is dither's own and the host comes
 * from the hostname it was told to publish at, so the redirect URI cannot name
 * somewhere it is not.
 */
export type OidcClient = { id: string; name: string; redirectUri: string };

export type Deployed = { routes: Route[]; oidc?: OidcClient };

const NAME = "dither";
const PORT = 8080;
const METRICS_PORT = 9464;
/** The uid the image runs as. */
const UID = 65532;
const SECRETS = "dither-secrets";

/** Private and link-local space: nothing the pod has business reaching. */
const PRIVATE = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "100.64.0.0/10"];

export type DitherArgs = {
  /** Where the app is used and signed in to. */
  hostname: string;
  /**
   * Where share links point, when the images have a domain of their own. That
   * domain serves the public pages and images only. Defaults to `hostname`.
   */
  publicHostname?: string;
  /** The node whose disk holds the state, by hostname. */
  node: string;
  /** Where on that node the database, uploads in progress and (without a bucket) the images live. */
  statePath?: string;
  oidc: {
    issuer: string;
    clientId: string;
    clientSecret: pulumi.Input<string>;
    /**
     * The provider's service inside the cluster, for exchanging codes when
     * the pod shares a node with the ingress: Cilium drops a pod's traffic to
     * its own node's public address.
     */
    backchannel?: { url: string; podLabels: Record<string, string>; port: number };
  };
  /**
   * An S3-compatible bucket (Cloudflare R2, Hetzner, MinIO) for the images
   * and the database's copies, in place of the node's disk. `url` is
   * `s3://bucket` or `s3://bucket/prefix`.
   */
  bucket?: {
    url: string;
    endpoint: string;
    region?: string;
    accessKeyId: pulumi.Input<string>;
    secretAccessKey: pulumi.Input<string>;
  };
  /**
   * Uploads are refused while a disk they write to (the state directory, and
   * the images without a bucket) has less than this free, so a full image
   * store never fills a disk the node depends on. Unset, each disk keeps 15%
   * of its size free, above the kubelet's 10% eviction threshold.
   */
  minFreeBytes?: number;
  maxUploadBytes?: number;
  limits?: { cpu: string; memory: string };
  requests?: { cpu: string; memory: string };
};

/** The container's environment, apart from the secrets. Exported for its test. */
export function environment(args: DitherArgs) {
  const publicHost = args.publicHostname ?? args.hostname;
  return [
    { name: "BIND_ADDR", value: `0.0.0.0:${PORT}` },
    { name: "METRICS_ADDR", value: `0.0.0.0:${METRICS_PORT}` },
    { name: "APP_URL", value: `https://${args.hostname}` },
    { name: "PUBLIC_URL", value: `https://${publicHost}` },
    { name: "STATE_DIR", value: "/data/state" },
    { name: "STORAGE", value: args.bucket ? args.bucket.url : "file:///data/objects" },
    ...(args.bucket
      ? [
          { name: "AWS_ENDPOINT", value: args.bucket.endpoint },
          { name: "AWS_REGION", value: args.bucket.region ?? "auto" },
        ]
      : []),
    ...(args.minFreeBytes === undefined ? [] : [{ name: "MIN_FREE_BYTES", value: String(args.minFreeBytes) }]),
    ...(args.maxUploadBytes ? [{ name: "MAX_UPLOAD_BYTES", value: String(args.maxUploadBytes) }] : []),
    { name: "OIDC_ISSUER", value: args.oidc.issuer },
    { name: "OIDC_CLIENT_ID", value: args.oidc.clientId },
    ...(args.oidc.backchannel ? [{ name: "OIDC_TOKEN_URL", value: `${args.oidc.backchannel.url}/oauth2/token` }] : []),
    { name: "RUST_LOG", value: "info" },
  ];
}

/**
 * dither: one replica on one node, its SQLite database and uploads in
 * progress on that node's disk, and its images there too or in a bucket.
 */
export function createDither(provider: k8s.Provider, namespace: pulumi.Input<string>, args: DitherArgs) {
  const options = { provider };
  const labels = { app: NAME };
  const state = args.statePath ?? "/var/lib/dither";
  const secretRef = (key: string) => ({ valueFrom: { secretKeyRef: { name: SECRETS, key } } });

  const secret = new k8s.core.v1.Secret(
    SECRETS,
    {
      metadata: { name: SECRETS, namespace },
      stringData: {
        "oidc-client-secret": pulumi.output(args.oidc.clientSecret),
        ...(args.bucket && {
          "s3-access-key-id": pulumi.output(args.bucket.accessKeyId),
          "s3-secret-access-key": pulumi.output(args.bucket.secretAccessKey),
        }),
      },
    },
    options,
  );

  const deployment = new k8s.apps.v1.Deployment(
    NAME,
    {
      metadata: { name: NAME, namespace, labels },
      spec: {
        replicas: 1,
        // One SQLite file on one node: a surge would put two writers on it.
        strategy: { type: "Recreate" },
        selector: { matchLabels: labels },
        template: {
          metadata: {
            labels,
            annotations: {
              "prometheus.io/scrape": "true",
              "prometheus.io/port": String(METRICS_PORT),
              "prometheus.io/path": "/metrics",
            },
          },
          spec: {
            nodeSelector: { "kubernetes.io/hostname": args.node },
            automountServiceAccountToken: false,
            enableServiceLinks: false,
            // Stopping lets an upload being processed finish and copies the
            // database a last time.
            terminationGracePeriodSeconds: 60,
            securityContext: {
              runAsNonRoot: true,
              runAsUser: UID,
              runAsGroup: UID,
              seccompProfile: { type: "RuntimeDefault" },
            },
            // kubelet creates a missing hostPath as root, which the service
            // cannot write; this hands it over, with only the ownership
            // capabilities.
            initContainers: [
              {
                name: "state",
                image: VERSIONS.alpine,
                command: ["sh", "-c", `chown ${UID}:${UID} /data && chmod 700 /data`],
                securityContext: {
                  runAsUser: 0,
                  runAsNonRoot: false,
                  allowPrivilegeEscalation: false,
                  readOnlyRootFilesystem: true,
                  capabilities: { drop: ["ALL"], add: ["CHOWN", "FOWNER"] },
                },
                resources: { requests: { cpu: "5m", memory: "8Mi" }, limits: { memory: "32Mi" } },
                volumeMounts: [{ name: "state", mountPath: "/data" }],
              },
            ],
            containers: [
              {
                name: NAME,
                image: VERSIONS.dither,
                imagePullPolicy: "IfNotPresent",
                ports: [
                  { name: "http", containerPort: PORT },
                  { name: "metrics", containerPort: METRICS_PORT },
                ],
                env: [
                  ...environment(args),
                  { name: "OIDC_CLIENT_SECRET", ...secretRef("oidc-client-secret") },
                  ...(args.bucket
                    ? [
                        { name: "AWS_ACCESS_KEY_ID", ...secretRef("s3-access-key-id") },
                        { name: "AWS_SECRET_ACCESS_KEY", ...secretRef("s3-secret-access-key") },
                      ]
                    : []),
                ],
                readinessProbe: { httpGet: { path: "/healthz", port: "http" }, periodSeconds: 10 },
                livenessProbe: {
                  httpGet: { path: "/healthz", port: "http" },
                  initialDelaySeconds: 10,
                  periodSeconds: 30,
                },
                // Decoding is what takes memory: the service holds the
                // megapixels being decoded at once to a budget, which a limit
                // a little over a hundred-megapixel photo's worth allows.
                resources: {
                  limits: args.limits ?? { cpu: "2", memory: "1536Mi" },
                  requests: args.requests ?? { cpu: "10m", memory: "64Mi" },
                },
                securityContext: {
                  allowPrivilegeEscalation: false,
                  readOnlyRootFilesystem: true,
                  capabilities: { drop: ["ALL"] },
                },
                volumeMounts: [{ name: "state", mountPath: "/data" }],
              },
            ],
            volumes: [{ name: "state", hostPath: { path: state, type: "DirectoryOrCreate" } }],
          },
        },
      },
    },
    { ...options, dependsOn: [secret], deleteBeforeReplace: true },
  );

  // Named for what the ingress derives from the route prefix.
  new k8s.core.v1.Service(
    `${NAME}-service`,
    {
      metadata: { name: `${NAME}-service`, namespace },
      spec: { selector: labels, ports: [{ port: 80, targetPort: PORT }] },
    },
    options,
  );

  // What it calls: the cluster's resolver, the identity provider's token
  // endpoint, and a bucket when it has one. No ingress rule: Traefik reaches
  // it, and an ingress policy that also drops the kubelet's probes gets the
  // pod killed for failing readiness.
  const backchannel = args.oidc.backchannel;
  new k8s.networking.v1.NetworkPolicy(
    `${NAME}-netpol`,
    {
      metadata: { name: NAME, namespace },
      spec: {
        podSelector: { matchLabels: labels },
        policyTypes: ["Egress"],
        egress: [
          {
            to: [{ namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "kube-system" } } }],
            ports: [
              { protocol: "UDP", port: 53 },
              { protocol: "TCP", port: 53 },
            ],
          },
          ...(backchannel
            ? [
                {
                  to: [{ podSelector: { matchLabels: backchannel.podLabels } }],
                  ports: [{ protocol: "TCP", port: backchannel.port }],
                },
              ]
            : []),
          ...(!backchannel || args.bucket
            ? [
                {
                  to: [{ ipBlock: { cidr: "0.0.0.0/0", except: PRIVATE } }],
                  ports: [{ protocol: "TCP", port: 443 }],
                },
              ]
            : []),
        ],
      },
    },
    options,
  );

  const hosts = [...new Set([args.hostname, args.publicHostname ?? args.hostname])];
  return {
    routes: hosts.map((hostname) => ({ service: NAME, hostname })),
    oidc: {
      id: args.oidc.clientId,
      name: NAME,
      redirectUri: `https://${args.hostname}/auth/callback`,
    },
    deployment,
  } satisfies Deployed & Record<string, unknown>;
}
