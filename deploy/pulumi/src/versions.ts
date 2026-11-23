/**
 * The version this chart deploys: the crate's version, and the package's.
 * dither and the chart that deploys it ship together, so one number says
 * exactly which build a pin gets. CI refuses a release where `Cargo.toml`,
 * `package.json` and this disagree.
 */
export const APP_VERSION = "0.4.0";

/**
 * The image's digest, which CI writes into the package it publishes once the
 * image is pushed. A tag can be moved by anyone who can push to the
 * registry, and a node pulls whatever it names when the image is not cached;
 * a digest names one image for good. Empty here, where no image of this
 * version exists yet when the release is made.
 */
export const IMAGE_DIGEST: string = "";

export const VERSIONS = {
  dither: `ghcr.io/radiosilence/dither:v${APP_VERSION}${IMAGE_DIGEST ? `@${IMAGE_DIGEST}` : ""}`,
  alpine: "alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf",
} as const;
