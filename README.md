# dither

An image host: albums of images and videos with captions, uploaded behind a sign-in and
shared by unlisted links that preview anywhere a link can. It has no feed, no
accounts of its own, no ads and no tracking.

It ships as a container image, `ghcr.io/radiosilence/dither`, a static Linux
binary on each release, and a Pulumi component in `deploy/pulumi`
(`@radiosilence/dither-pulumi`) at the same version.

## What a visitor gets

An album is a page of its images stacked in order, each with its caption, at
`/a/<id>`; each image has a page of its own at `/<id>` and is the file itself
at `/<id>.jpg` (or `.png`, `.gif`, `.webp`, `.mp4`). The pages are plain HTML
and CSS; a page with a video also loads the player's script, which enhances a
`<video>` that plays without it. They carry Open Graph and Twitter card tags, so a link posted
to Reddit, WhatsApp, Discord, iMessage, Slack or X previews as the picture, and
the direct file address works anywhere an image link does. The preview's
picture is a JPEG at most 1200 pixels wide and 300 KB, made at upload:
WhatsApp shows nothing for a larger one, and not every fetcher reads WebP. An
animated picture under 5 MB is its own preview, so it plays where previews do.

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

### Videos

MP4, MOV (as phones record), M4V and WebM or Matroska are accepted when the
server has ffmpeg (the image ships a static build), up to 1 GB and 30 minutes
by default. The original is remuxed into MP4 without re-encoding, keeping its
first video and audio streams and nothing else: phones write where a video
was taken into the container's metadata and into timed metadata tracks, as
they do into a photo's EXIF, and none of it is kept. The rotation a phone
records is part of the video track and survives.

Browsers do not all play what phones record, so a video that is not already
H.264, 8-bit, SDR, at most 1920 pixels long, at most 60 frames a second and
under 12 Mbit/s gets a playback copy that is (x264 at CRF 23, stereo AAC),
tone-mapped from HDR (PQ or HLG) to SDR. The original stays downloadable as
recorded. A frame of the playback copy becomes the thumbnail and the link
preview, and a page's `og:video` names the playback copy, for the sites that
play one inline.

The worker (see *Processing*) makes a video's copies, showing its progress
on the album page, and marks it ready, or failed with the reason: unreadable,
too long or too large. Until it is ready a video is its owner's alone.

ffmpeg parses untrusted files, so it runs as a process of its own, with none
of the environment: told the container it reads rather than left to probe,
limited to the file protocol and to the decoders phones and cameras need, on
`VIDEO_THREADS` threads (filters included) at a lower priority, and with a
time limit. A video whose codec MP4 cannot hold (VP8, Theora, ProRes) keeps
only its playback copy.

Every file is served with byte ranges, which seeking needs and Safari requires
before it plays a video at all; video is never compressed again on the way
out.

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

Finishing an upload checks what its first few megabytes can tell (what it
is, how large, and for a picture its pixels and the memory it needs), so a
file that will be refused is refused while its sender waits. Then it is
queued: its image's row is added as processing, at once visible in the
album, and its file moved to `STATE_DIR/jobs`. Finishing runs in a task the
request only waits on, so a client that disconnects does not stop it, and
asking again reads how it ended.

### Processing

A worker, `dither worker`, makes uploads' copies: the web process decodes
nothing. In Kubernetes it is a second container in the pod, sharing the
state directory and the store, holding the store's keys but not the sign-in
secret, with its own resource limits. `just dev` runs one beside the server.

The queue is the database. A worker claims a job with a lease it renews while
it works, so a worker that stops leaves its jobs for the next once their
leases lapse (a minute), and one stopped by a signal gives them back at
once. A job that fails on this side is tried again later, three times; one
whose file is at fault fails at once. Pictures are made several at a time
(`PICTURE_WORKERS`) under a budget of memory (`PROCESS_MEMORY_BYTES`), each
one's share worked out from its header in bytes rather than pixels: a 16-bit
PNG decodes to twice an 8-bit one's size, and a photo turning upright needs a
second copy. Videos are made one at a time (`VIDEO_WORKERS`).

The two processes talk over a Unix socket in the state directory, a line of
JSON each way: the web process says a job is waiting, so the worker need not
poll, and that images were deleted, so a worker making one stops; the worker
says how far along a job is, which the album page shows as it happens, and
that a job ended, which redraws its owner's pages and prompts a copy of the
database. A worker opens the database only once the web process listens,
since the web process may first replace a database that is behind the
store's copy, and stops when the web process goes away, to be started again
with it.

The database counts the bytes that arrived and the file on disk holds them.
Where the two disagree (a disk restored without an upload's file, or a crash
before its last bytes reached the disk), the file wins: the count drops to
what it holds and the client sends the rest.

The same JSON API serves anything else that wants to upload:

```text
POST   /api/albums                 {title?}                 → {id}
POST   /api/albums/{id}/uploads    {files: [{name, size, type?}]} → {uploads: [{id}], chunk}
PUT    /api/uploads/{id}           Content-Range: bytes a-b/size, the bytes → {received}
GET    /api/uploads/{id}                                    → {received, size, image?, error?}
POST   /api/uploads/{id}/finish                             → {image: {id, page, file, thumb, processing, …}} | {error}
DELETE /api/uploads/{id}
```

A file's `type`, the browser's MIME type, holds a video to the video limit
rather than the image one; the bytes decide what a file is when it finishes.
A video finishes `processing`, and its copies follow.

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

The id_token is taken from the token endpoint in answer to a request dither
makes, never from the browser, so its signature is not checked; its issuer,
audience and expiry are. That trusts whatever answers at the token endpoint:
the issuer itself over TLS, or, with `OIDC_TOKEN_URL` pointing at the
provider's service inside a cluster, the cluster's network.

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
  only the player's own script, and load it only where there is a video.
- ffmpeg, which parses uploaded videos, runs as a separate process told the
  container it reads, limited to the file protocol and a list of decoders,
  at a lower priority, with a time limit (see *Videos*).
- A file's type is decided by its signature, never by its name or the type
  the browser claims. Decoding is limited in pixels and allocation.
- Files in an album that is not private are cached by browsers for a year and
  by shared caches (a CDN in front) for ten minutes, so making an album
  private or deleting an image stops it being served within ten minutes. A
  private album's files are never cached.
- Signing out is a link, which another site cannot follow on someone's
  behalf: a navigation the browser says came from elsewhere is ignored.

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
| `PROCESS_MEMORY_BYTES` | 1 GiB | Memory a worker's pictures take at once. A picture that needs more alone is refused when its upload finishes. |
| `CACHE_BYTES` | 256 MiB | Recently served images kept in memory. |
| `VIDEO` | `auto` | `auto` takes videos when ffmpeg and ffprobe run; `on` fails startup without them; `off` takes none. |
| `FFMPEG`, `FFPROBE` | `ffmpeg`, `ffprobe` | The tools, by path or on `PATH`. The image sets both. |
| `MAX_VIDEO_BYTES`, `MAX_VIDEO_SECONDS` | 1 000 000 000, 1800 | Per video. |
| `VIDEO_THREADS` | 2 | Threads an ffmpeg run uses, filters included. |
| `PICTURE_WORKERS`, `VIDEO_WORKERS` | 4, 1 | Jobs a worker makes at once. |
| `BIND_ADDR`, `METRICS_ADDR` | `127.0.0.1:8080`, none | `/metrics` (Prometheus) is on a port of its own when set. |

## Development

```sh
just dev     # http://127.0.0.1:8080, no sign-in, everything under ./data
just check   # fmt, clippy, tests, the Pulumi package, the stylesheet, the script's types, changelog, versions
just css     # after changing styles/, a template's classes or class strings in src/
just i18n    # after changing a template's words
```

The stylesheet is compiled by Tailwind and committed, as is the theme's
typeface, because the binary embeds them; building dither needs no Node.
`just css` installs `@radiosilence/theme` from GitHub Packages, which needs a
token even to read (`just auth` writes it from the gh login).

The browser's script is TypeScript (`browser/app.ts`). Building strips its
types with oxc (`build.rs`) and embeds the result, so a build needs no Node
either; `just js` typechecks it with typescript-go, as CI does.

### Words

Every word a page shows is a gettext message in a template: `{{ __("…") }}`,
`{{ __p("context", "…") }}` where the same English means different things, and
`{{ __n("%{count} image", "%{count} images", n) }}` for plurals, through
[askama_gettext](https://github.com/radiosilence/blit/tree/main/crates/askama_gettext).
The English is the message id, so en-GB, the only locale for now, needs no
catalogue at run time. The script's words come from the layout's
`<template id="copy">`, keyed by name and rendered the same way, so they are
in the catalogue too. A line with a count comes once for each plural form of
the language, rendered at a count in that form, and the browser files each
under the category `Intl.PluralRules` gives its count.

Two things are not translated yet: the API's error text, which the uploader
shows as the server words it (an API's answers serve any client, so wording
them per language means answering with a code a client words), and the
formatting of numbers, sizes and times, which the server and the script do
the same way for every language.

`locales/en-GB/messages.po` collects every message with where it is used; a
test fails while it is out of date, and `just i18n` rewrites it. Adding a
language is a catalogue beside it, its code in that test's list, and loading
and choosing it per request.
