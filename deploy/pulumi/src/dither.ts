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
/** The worker API's port: inside the cluster only, never through the ingress. */
const WORK_PORT = 8081;
const WORKER = `${NAME}-worker`;
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
    backchannel?: {
      url: string;
      podLabels: Record<string, string>;
      port: number;
      /** The provider's namespace, when it is not this one; `url` then names it too. */
      namespace?: string;
    };
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
  /** The largest video taken (1 GB by default); `video: false` takes none. */
  maxVideoBytes?: number;
  /** The longest video taken, in seconds (30 minutes by default). */
  maxVideoSeconds?: number;
  video?: boolean;
  /** The web container's resources. It decodes nothing, so needs little beyond its cache. */
  limits?: { cpu: string; memory: string };
  requests?: { cpu: string; memory: string };
  /** The worker container's: the processing budget (1 GiB by default) and one ffmpeg beside it. */
  workerLimits?: { cpu: string; memory: string };
  workerRequests?: { cpu: string; memory: string };
  /** Threads the pod's own worker gives ffmpeg (2 by default): the CPUs it may use. */
  videoThreads?: number;
  /**
   * Workers on other nodes, pulling from the same queue as the pod's own
   * over the worker API: each a Deployment pinned to its node, with a
   * scratch disk for what it makes. They need `workerToken`.
   */
  pool?: PoolWorker[];
  /** What workers present to the worker API. Required with `pool`. */
  workerToken?: pulumi.Input<string>;
};

export type PoolWorker = {
  /** The node it runs on, by hostname. */
  node: string;
  limits?: { cpu: string; memory: string };
  requests?: { cpu: string; memory: string };
  /** Threads ffmpeg uses (2 by default): the CPUs it may use. */
  videoThreads?: number;
  /** Pictures made at once (4 by default); 0 leaves them to the rest. */
  pictures?: number;
  /** Videos made at once (1 by default). */
  videos?: number;
  /** Room for uploads and what is made of them (16Gi by default). */
  scratch?: string;
};

/**
 * A worker's environment, apart from its token: where the queue is, what it
 * makes and with how much. It holds neither the store's keys nor the
 * sign-in; the web process stores what it makes. Exported for its test.
 */
export function workerEnvironment(
  args: Pick<DitherArgs, "video">,
  worker: { queue: pulumi.Input<string>; videoThreads?: number; pictures?: number; videos?: number },
) {
  return [
    { name: "STATE_DIR", value: "/data/state" },
    { name: "QUEUE_URL", value: worker.queue },
    ...(args.video === false ? [{ name: "VIDEO", value: "off" }] : []),
    ...(worker.videoThreads ? [{ name: "VIDEO_THREADS", value: String(worker.videoThreads) }] : []),
    ...(worker.pictures !== undefined ? [{ name: "PICTURE_WORKERS", value: String(worker.pictures) }] : []),
    ...(worker.videos !== undefined ? [{ name: "VIDEO_WORKERS", value: String(worker.videos) }] : []),
    { name: "RUST_LOG", value: "info" },
  ];
}

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
    ...(args.video === false ? [{ name: "VIDEO", value: "off" }] : []),
    ...(args.maxVideoBytes ? [{ name: "MAX_VIDEO_BYTES", value: String(args.maxVideoBytes) }] : []),
    ...(args.maxVideoSeconds ? [{ name: "MAX_VIDEO_SECONDS", value: String(args.maxVideoSeconds) }] : []),
    // Reached from other nodes only when there is a pool, and then with a token.
    { name: "WORKER_ADDR", value: `${args.pool?.length ? "0.0.0.0" : "127.0.0.1"}:${WORK_PORT}` },
    { name: "OIDC_ISSUER", value: args.oidc.issuer },
    { name: "OIDC_CLIENT_ID", value: args.oidc.clientId },
    ...(args.oidc.backchannel ? [{ name: "OIDC_TOKEN_URL", value: `${args.oidc.backchannel.url}/oauth2/token` }] : []),
    { name: "RUST_LOG", value: "info" },
  ];
}

/**
 * What the pod may call: the cluster's resolver, the identity provider's
 * token endpoint, and HTTPS beyond the cluster when a bucket or a provider
 * reached at its public address needs it. Exported for its test.
 */
export function egress(args: Pick<DitherArgs, "oidc" | "bucket">) {
  const backchannel = args.oidc.backchannel;
  // A pod selector alone matches pods in this namespace only.
  const provider = backchannel && {
    podSelector: { matchLabels: backchannel.podLabels },
    ...(backchannel.namespace
      ? { namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": backchannel.namespace } } }
      : {}),
  };
  return [
    {
      to: [{ namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "kube-system" } } }],
      ports: [
        { protocol: "UDP", port: 53 },
        { protocol: "TCP", port: 53 },
      ],
    },
    ...(backchannel && provider ? [{ to: [provider], ports: [{ protocol: "TCP", port: backchannel.port }] }] : []),
    ...(!backchannel || args.bucket
      ? [{ to: [{ ipBlock: { cidr: "0.0.0.0/0", except: PRIVATE } }], ports: [{ protocol: "TCP", port: 443 }] }]
      : []),
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
  const pool = args.pool ?? [];
  if (pool.length && !args.workerToken) {
    throw new Error("dither: a worker pool needs workerToken");
  }
  const tokenEnv = args.workerToken ? [{ name: "WORKER_TOKEN", ...secretRef("worker-token") }] : [];

  const secret = new k8s.core.v1.Secret(
    SECRETS,
    {
      metadata: { name: SECRETS, namespace },
      stringData: {
        "oidc-client-secret": pulumi.output(args.oidc.clientSecret),
        ...(args.workerToken && { "worker-token": pulumi.output(args.workerToken) }),
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
                  { name: "work", containerPort: WORK_PORT },
                ],
                env: [
                  ...environment(args),
                  ...tokenEnv,
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
                // The web process decodes nothing: its memory is the image
                // cache (256 MiB by default) and requests.
                resources: {
                  limits: args.limits ?? { cpu: "1", memory: "512Mi" },
                  requests: args.requests ?? { cpu: "10m", memory: "64Mi" },
                },
                securityContext: {
                  allowPrivilegeEscalation: false,
                  readOnlyRootFilesystem: true,
                  capabilities: { drop: ["ALL"] },
                },
                volumeMounts: [{ name: "state", mountPath: "/data" }],
              },
              // Makes uploads into images and videos, from the queue over
              // the worker API on the pod's loopback, reading uploads where
              // they wait in the shared state directory. It holds neither
              // the store's keys nor the sign-in secret, and its own limits
              // bound what processing can take: the budget pictures are made
              // under (1 GiB by default) and one ffmpeg.
              {
                name: "worker",
                image: VERSIONS.dither,
                imagePullPolicy: "IfNotPresent",
                args: ["worker"],
                env: [
                  ...workerEnvironment(args, {
                    queue: `http://127.0.0.1:${WORK_PORT}`,
                    videoThreads: args.videoThreads,
                  }),
                  ...tokenEnv,
                ],
                resources: {
                  // A conversion takes about 1.5 GB at 4K, 2 GB from an 8K
                  // source, beside the pictures' PROCESS_MEMORY_BYTES.
                  limits: args.workerLimits ?? { cpu: "2", memory: "4Gi" },
                  requests: args.workerRequests ?? { cpu: "10m", memory: "64Mi" },
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

  // No ingress rule: Traefik reaches it, and an ingress policy that also
  // drops the kubelet's probes gets the pod killed for failing readiness.
  new k8s.networking.v1.NetworkPolicy(
    `${NAME}-netpol`,
    {
      metadata: { name: NAME, namespace },
      spec: { podSelector: { matchLabels: labels }, policyTypes: ["Egress"], egress: egress(args) },
    },
    options,
  );

  if (pool.length) {
    createPool(provider, namespace, args, pool, secret, secretRef);
  }

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

/**
 * Workers on other nodes: the worker API published inside the cluster, and a
 * worker on each node pulling from it. A worker reaches only the cluster's
 * resolver and the worker API, and holds only the token.
 */
function createPool(
  provider: k8s.Provider,
  namespace: pulumi.Input<string>,
  args: DitherArgs,
  pool: PoolWorker[],
  secret: k8s.core.v1.Secret,
  secretRef: (key: string) => object,
) {
  const options = { provider };
  const work = new k8s.core.v1.Service(
    `${NAME}-work`,
    {
      metadata: { name: `${NAME}-work`, namespace },
      spec: { selector: { app: NAME }, ports: [{ name: "work", port: WORK_PORT, targetPort: WORK_PORT }] },
    },
    options,
  );
  const queue = pulumi.interpolate`http://${work.metadata.name}.${namespace}.svc.cluster.local:${WORK_PORT}`;

  for (const worker of pool) {
    const name = `${WORKER}-${worker.node}`;
    const labels = { app: WORKER, node: worker.node };
    new k8s.apps.v1.Deployment(
      name,
      {
        metadata: { name, namespace, labels },
        spec: {
          replicas: 1,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              nodeSelector: { "kubernetes.io/hostname": worker.node },
              automountServiceAccountToken: false,
              enableServiceLinks: false,
              // Stopping gives its claims back for another worker to take up.
              terminationGracePeriodSeconds: 30,
              securityContext: {
                runAsNonRoot: true,
                runAsUser: UID,
                runAsGroup: UID,
                fsGroup: UID,
                seccompProfile: { type: "RuntimeDefault" },
              },
              containers: [
                {
                  name: "worker",
                  image: VERSIONS.dither,
                  imagePullPolicy: "IfNotPresent",
                  args: ["worker"],
                  env: [
                    ...workerEnvironment(args, { ...worker, queue }),
                    { name: "WORKER_TOKEN", ...secretRef("worker-token") },
                  ],
                  resources: {
                    limits: worker.limits ?? { cpu: "2", memory: "4Gi" },
                    requests: worker.requests ?? { cpu: "10m", memory: "64Mi" },
                  },
                  securityContext: {
                    allowPrivilegeEscalation: false,
                    readOnlyRootFilesystem: true,
                    capabilities: { drop: ["ALL"] },
                  },
                  volumeMounts: [{ name: "scratch", mountPath: "/data" }],
                },
              ],
              volumes: [{ name: "scratch", emptyDir: { sizeLimit: worker.scratch ?? "16Gi" } }],
            },
          },
        },
      },
      { ...options, dependsOn: [secret] },
    );
  }

  new k8s.networking.v1.NetworkPolicy(
    `${WORKER}-netpol`,
    {
      metadata: { name: WORKER, namespace },
      spec: {
        podSelector: { matchLabels: { app: WORKER } },
        policyTypes: ["Ingress", "Egress"],
        ingress: [],
        egress: [
          {
            to: [{ namespaceSelector: { matchLabels: { "kubernetes.io/metadata.name": "kube-system" } } }],
            ports: [
              { protocol: "UDP", port: 53 },
              { protocol: "TCP", port: 53 },
            ],
          },
          { to: [{ podSelector: { matchLabels: { app: NAME } } }], ports: [{ protocol: "TCP", port: WORK_PORT }] },
        ],
      },
    },
    options,
  );
}
