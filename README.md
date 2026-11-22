# dither

An image host: albums of images with captions, uploaded behind a sign-in and
shared by unlisted links that preview anywhere a link can. It has no feed, no
accounts of its own, no ads and no tracking.

It ships as a container image, `ghcr.io/radiosilence/dither`, a static Linux
binary on each release, and a Pulumi component in `deploy/pulumi`
(`@radiosilence/dither-pulumi`) at the same version.

## What a visitor gets

An album is a page of its images stacked in order, each with its caption, at
`/a/<id>`; each image has a page of its own at `/<id>` and is the file itself
at `/<id>.jpg` (or `.png`, `.gif`, `.webp`). The pages are plain HTML and CSS
with no script. They carry Open Graph and Twitter card tags, so a link posted
to Reddit, Discord, iMessage, Slack or X previews as the picture, and the
direct file address works anywhere an image link does.

Albums are unlisted: an address is ten characters of base62 from a CSPRNG
(about 59.5 bits), long enough that enumerating them is hopeless where
imgur's seven are not. Pages say `noindex`, so search engines leave them out,
while `robots.txt` allows every crawler, since some link-preview fetchers obey
it and a disallow would leave a link without its picture. An album can be
made private, when it is its owner's alone.

## What is stored

Each upload is cleaned before anything is kept: the container is taken apart
and rebuilt from the parts that draw the picture (image data, colour profile,
animation control), and the orientation is written back as an EXIF block
holding that one tag. Everything else goes, including GPS coordinates, camera
and lens details, XMP, IPTC, comments, embedded thumbnails and the second
image a phone appends after the first (a depth map or HDR gain map). Nothing
is re-encoded, so the original keeps its quality and animations survive. A
parser drops whatever it does not recognise, so a new kind of metadata is
removed by default.

Beside the original are a thumbnail (WebP, short side 480 pixels) and, for a
picture longer than 2048 pixels on a side, a display copy of that size, which
pages offer through `srcset` so a phone is not sent a 48-megapixel original.
Both carry the original's colour profile when it is an RGB one, so a
wide-gamut photo looks the same in every copy.

JPEG, PNG (including APNG), GIF and WebP are accepted, up to 64 MB and 100
megapixels by default. HEIC, AVIF, SVG (which can carry script), BMP and TIFF
are refused with the reason. An iPhone converts HEIC to JPEG when uploading
from Photos through the browser.

## Storage

Images, and copies of the database, go to one object store chosen by
`STORAGE`: a directory (`file:///data/objects`) or an S3-compatible bucket
(`s3://bucket`, with Cloudflare R2, Hetzner Object Storage or MinIO). The
database is SQLite in `STATE_DIR`. Two seconds after each burst of changes it
is compacted (`VACUUM INTO`), gzipped and written to the store as
`state/dither.db.gz`, with one copy an hour kept for three days. A node that
starts with no database restores the latest copy, and so does one whose
database is behind it: each copy carries a generation, so a disk an older run
left behind is told apart from the live one, and is kept beside the restored
database rather than deleted. With a bucket, the store is the only thing to
back up and the pod's disk is a cache. One replica is assumed, as with any
SQLite service.

The database records an image's copies before they are written, and recording
the image claims them. An hourly sweep deletes copies whose record is an hour
old and unclaimed: a crash between storing and recording, or a delete the
store did not confirm. It never deletes a copy because no image names it, so
a database restored from an older copy leaves newer images in the store.

Uploads are refused while a disk they write to (the state directory, and a
disk store) has less than `MIN_FREE_BYTES` free, or by default 15% of its
size, so a full image store never fills a disk the rest of the machine
depends on: the kubelet starts evicting pods at 10% free.

### Moving from a disk to a bucket

```sh
# Copy everything, the database's copies included. It resumes if interrupted.
AWS_ENDPOINT=https://<account>.r2.cloudflarestorage.com AWS_REGION=auto \
AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… \
  dither copy-store file:///data/objects s3://<bucket>
```

Then point `STORAGE` at the bucket (in Kubernetes, give the component its
`bucket`). Uploads made between the copy and the switch are not in the
bucket, so run the copy again just before switching, or stop uploading for
the few minutes it takes.

## Uploading

The web UI takes images from the file picker, dropped anywhere on the page,
or pasted. Each file goes in chunks of 4 MiB, three files at a time; a chunk
that fails on the network is sent again from wherever the server says it got
to, so a phone that loses signal halfway through a large photo loses seconds,
not the file. Chunks are small enough to arrive within Traefik's default
60-second read timeout on a slow connection. A batch keeps the order its
files were picked in, whatever order they finish in.

Processing (cleaning, decoding, resizing, encoding) runs under a budget of
memory (`PROCESS_MEMORY_BYTES`), which is what bounds what a batch of hundreds
of photos can take. Each image's share is worked out from its header before
anything is decoded, in bytes rather than pixels: a 16-bit PNG decodes to
twice an 8-bit one's size, and a photo turning upright needs a second copy.
An image that needs more than the whole budget is refused with the reason.
Processing runs in a task the request only waits on, so a client that
disconnects does not stop it, and asking again reads how it ended.

The same JSON API serves anything else that wants to upload:

```text
POST   /api/albums                 {title?}                 → {id}
POST   /api/albums/{id}/uploads    {files: [{name, size}]}  → {uploads: [{id}], chunk}
PUT    /api/uploads/{id}           Content-Range: bytes a-b/size, the bytes → {received}
GET    /api/uploads/{id}                                    → {received, size, image?, error?}
POST   /api/uploads/{id}/finish                             → {image: {id, page, file, thumb, …}} | {error}
DELETE /api/uploads/{id}
```

Every request needs a session and, if it writes, an `X-Dither` or
`Datastar-Request` header. A chunk answered 408, 429 or a 5xx other than 507
is worth sending again, from the `received` that `GET` reports; a 409 carries
it. Other answers are final.

## The web UI

Pages are drawn on the server and patched in place by
[Datastar](https://data-star.dev): a live stream redraws an album's grid as
uploads land or another device reorders it, sending a part only when it
changed. Titles, descriptions and captions save as they are typed. Images are
reordered by dragging (on a touch screen, hold a tile to lift it), selected
for moving or deleting, and opened in a viewer with their caption and the
links to share them by (page, file, Markdown, BBCode, HTML). The look is
kōan's, from [@radiosilence/theme](https://github.com/radiosilence/theme).

## Signing in

dither is an OAuth client of an OpenID Connect provider (`OIDC_ISSUER`) and
has no allowlist of its own: who may sign in is the provider's decision.
Every person who signs in gets albums of their own and sees nobody else's.
Sessions are opaque ids in `__Host-` cookies, stored hashed.

## A domain of its own

`PUBLIC_URL` is what share links name. Set it to a different host from
`APP_URL` and requests for that host reach the public pages and images only:
no sign-in, no session, no app routes, and a front page that says nothing.
The session cookie is scoped to the app's host, so the public domain never
receives it.

The domain is still linkable to whoever runs it by its address (it resolves
to the same server as everything else there) and by any public configuration
naming it. Serving it through a CDN that hides the origin, and keeping its
name out of public repositories, closes both.

## Security notes

- Every route behind sign-in sits under one session layer; every write needs
  a header a cross-site form cannot set, beside `SameSite=Lax` cookies.
- Each query that changes something takes the owner and checks it in the same
  statement: a request naming someone else's album changes nothing and reads
  as not found.
- Image files are served with their sniffed type, `nosniff` and
  `Content-Security-Policy: default-src 'none'; sandbox`, so no browser treats
  one as a document. Pages forbid inline script and style; public pages allow
  no script at all.
- A file's type is decided by its signature, never by its name or the type
  the browser claims. Decoding is limited in pixels and allocation.

## Configuration

Read from the environment at start; a missing or malformed value fails
startup.

| Variable | Default | |
|---|---|---|
| `APP_URL` | `http://<BIND_ADDR>` | Where the app is used and signed in to. |
| `PUBLIC_URL` | `APP_URL` | What share links name. |
| `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` | — | Required. `INSECURE_NO_AUTH=1` disables sign-in and is only accepted with a loopback `BIND_ADDR`. |
| `OIDC_TOKEN_URL` | the issuer's | Where codes are exchanged, when the issuer's public address is unreachable from the pod. |
| `STORAGE` | `file://<cwd>/data/objects` | The object store. |
| `AWS_ENDPOINT`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | — | For an `s3://` store. R2's region is `auto`. |
| `STATE_DIR` | `data/state` | The database and uploads in progress, on a local disk. |
| `MIN_FREE_BYTES` | 15% of each disk | Uploads are refused below this much free space on a disk they write to. |
| `MAX_UPLOAD_BYTES`, `MAX_PIXELS` | 64 000 000, 100 000 000 | Per image. |
| `PROCESS_MEMORY_BYTES` | 1 GiB | Memory processing takes at once, across all uploads. An image that needs more alone is refused. |
| `CACHE_BYTES` | 256 MiB | Recently served images kept in memory. |
| `BIND_ADDR`, `METRICS_ADDR` | `127.0.0.1:8080`, none | `/metrics` (Prometheus) is on a port of its own when set. |

## Development

```sh
just dev     # http://127.0.0.1:8080, no sign-in, everything under ./data
just check   # fmt, clippy, tests, the Pulumi package, the stylesheet, changelog, versions
just css     # after changing styles/, a template's classes or class strings in src/
```

The stylesheet is compiled by Tailwind and committed, as is the theme's
typeface, because the binary embeds them; building dither needs no Node.
`just css` installs `@radiosilence/theme` from GitHub Packages, which needs a
token even to read (`just auth` writes it from the gh login).
