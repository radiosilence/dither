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

Captions and album descriptions are Markdown: CommonMark with GitHub's
tables, strikethrough, task lists and bare links, and a line break in the box
is one on the page, as people expect of a caption. Raw HTML shows as the text
it is, a link that could run script loses its address, and an image becomes a
link to it, since the pages load no picture from another site. Link previews,
alt text and a video's title in the media controls take the words alone.

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
by default.

One copy of each video is kept, and it is what plays and what downloads. By
default it is converted to what every browser plays, at the video's own size:
H.264 within 4K and level 5.2's frame (4096×2304; beyond it hardware
decoders refuse a video), x264 at CRF 18, which is visually lossless, 8-bit
SDR tone-mapped from HDR (PQ or HLG), at most 60 frames a second, with stereo
AAC. That also makes dither a way to turn a phone's HEVC into something
anything plays. A video that is already all of that is not re-encoded,
whatever its bitrate. With *keep videos as recorded* ticked when it is uploaded, it is
kept exactly as recorded instead, and plays where the browser can.

Either way it goes into MP4 with its first video and sound streams and
nothing else: phones write where a video was taken into the container's
metadata and into timed metadata tracks, as they do into a photo's EXIF, and
none of it is kept. Nor are the SEI messages inside an H.264 or HEVC stream,
where an encoder writes its settings and some cameras the time or where they
were, whether in the frames or in the stream's configuration. The rotation a
phone records is part of the video track and survives. A frame of it becomes the thumbnail and the link preview,
tone-mapped when the video is HDR, and a page's `og:video` names it, for the
sites that play one inline.

The worker (see *Processing*) makes a video's copies, showing its progress
on the album page, and marks it ready, or failed with the reason: unreadable,
too long or too large. Until it is ready a video is its owner's alone.

ffmpeg parses untrusted files, so it runs as a process of its own, with none
of the environment: told the container it reads rather than left to probe,
limited to the file protocol and to the decoders phones and cameras need, on
`VIDEO_THREADS` threads (filters included) at a lower priority, and with a
time limit. Those are ffmpeg's own checks, which a file exploiting a bug in
it would ignore, so the kernel confines each run as well. Landlock lets it
read only the programs' directories and the file it was given, and write
only in its own working directory: not the database, the store or `/proc`,
and no TCP. seccomp refuses it sockets, other processes (`ptrace`,
`process_vm_*`), new namespaces and the kernel interfaces exploits reach for
(io_uring, BPF, perf events, userfaultfd). It writes no file larger than the
disk's room above `MIN_FREE_BYTES` and dumps no core, and the worker that
starts it keeps its own memory and environment out of `/proc`. Landlock
applies as far as the kernel has it, which the worker logs at startup; the
rest applies on any Linux. A video whose codec MP4 cannot hold (VP8, Theora,
ProRes) is converted even when it is to be kept as recorded.

Every file is served with byte ranges, which seeking needs and Safari requires
before it plays a video at all; video is never compressed again on the way
out.

## View counts

An album's owner sees, in the viewer and on the album's page, how often its
public pages and files were served over the last 30 days and in all, to how
many visitors, how many bytes that took, which link previews fetched it and
which sites linked to it. The counts exist so an owner can see where
something they shared went; they are kept per image and per album, per UTC
day, and nothing in them identifies a visitor.

For each GET of an album's page, an image's page or any file of an image,
the web process counts a hit by kind (page, original, display copy,
thumbnail, preview) and the bytes sent: for a range request the range, for
a download abandoned part way what went out before it stopped, and for a
page its compressed size when it was sent compressed. A revalidation
answered `304 Not Modified` sends nothing and is not counted.

A view is a page, or an original or display copy opened on its own: navigated
to (`Sec-Fetch-Dest: document`), embedded in another site, or fetched by a
client that sends no `Sec-Fetch-Dest` and is not a media framework. A file
fetched for one of dither's pages is part of that page's view: public pages
send `Referrer-Policy: same-origin`, so their requests name dither's host and
no other site learns anything, and a media framework playing a page's video
(iOS's AppleCoreMedia, which sends no referrer) is not counted as a view
either. Only a range from the start of a file can be a view, so a video
played in pieces counts once, and not the first few bytes, which Safari asks
for before it plays.

An album's totals are its own page's counts and those of the images in it
now, so a moved image takes its counts with it. Its visitors are counted
across the album as they arrive: someone who opens the album and three of
its images is one visitor of the album.

A visitor is the SHA-256 of a salt, the client's address and its user
agent. The salt is random, held only in memory and replaced at UTC midnight;
the day's hashes stay in memory only to tell a new visitor from one already
counted, and the database is given the number of distinct ones. A visitor on
two days counts once on each. A restart draws a new salt, so someone seen
before and after a restart counts twice that day. The client's address is the
last `X-Forwarded-For` entry outside `TRUSTED_PROXIES`, or else the
connection's. The proxy in front (Traefik, as deployed) appends the address it
saw, which behind a CDN is the CDN edge's, so list the CDN's ranges in
`TRUSTED_PROXIES`. If the proxy keeps the `X-Forwarded-For` the CDN sends, the
client's address is the entry the CDN appended, and anything a client sends
sits to the left of it and is never chosen. If it does not (keeping it for
every site on the CDN would let any of them claim any address), set
`ADDRESS_HEADER` to the header the CDN puts the client's address in
(`CF-Connecting-IP`); it is read only when the last entry is a trusted
proxy's, so a request that skipped the CDN cannot choose its address. The
address is used only inside the hash. A day remembers at most 500,000 visitors; past
that, new ones are not counted until midnight.

A CDN in front serves what it has cached without asking dither, so those
requests are not counted: during a burst on one image, its counts show a few
hits for each file every ten minutes, and, where the CDN caches pages, a few
for its page every minute.

A referring site is the host of the `Referer` header, without its path or
query, and dither's own hosts are ignored; past the first 50 hosts in a day
for an image or album, the rest are counted together as other sites. A
link-preview fetcher (WhatsApp, Facebook, X, Slack, Discord, Telegram,
LinkedIn, Reddit, Iframely, Embedly) asking for a page counts once for that
fetcher, as a place the link was shared; the files it fetches next count
only their bytes, and a fetcher is never a view or a visitor. Requests carrying the owner's own session are not
counted at all; with the public pages on [a domain of their
own](#a-domain-of-its-own), which never receives the session cookie, the
owner's visits there are counted like anyone's. A private album is only ever
its owner's and is never counted.

No address, user agent, hash or cookie is written anywhere. The counts gather
in memory and are added to the database every minute, so the newest minute
may not show yet. The database's copy in the store is brought up to date for
them at most every 15 minutes, since a copy is of the whole database; any
other change copies it at once, counts included, and so does stopping, after
the last counts are written. Deleting an image or album deletes its counts. Prometheus's `/metrics` keeps its totals by kind
regardless.

## Storage

Images, uploads waiting for a worker, and copies of the database go to one
S3-compatible bucket, `STORAGE` (`s3://bucket`): Garage or MinIO in the
cluster, reached over plain HTTP with `AWS_ALLOW_HTTP=true`, or Cloudflare R2
or Hetzner Object Storage outside it. `memory:///` keeps them in memory
instead, for development, and forgets them when the process stops. The
database is SQLite in `STATE_DIR`. Two seconds after each burst of changes it
is compacted (`VACUUM INTO`), gzipped and written to the store as
`state/dither.db.gz`, with one copy an hour kept for three days. A node that
starts with no database restores the latest copy, and so does one whose
database is behind it: each copy carries a generation, so a disk an older run
left behind is told apart from the live one, and is kept beside the restored
database rather than deleted. The bucket is the only thing to back up: the
web process's disk holds uploads in progress, the database and a cache. One
replica is assumed, as with any SQLite service.

The database records an image's copies before they are written, and recording
the image claims them. An hourly sweep deletes copies whose record is an hour
old and unclaimed: a crash between storing and recording, or a delete the
store did not confirm. It never deletes a copy because no image names it, so
a database restored from an older copy leaves newer images in the store.

Uploads in progress are refused while the state directory's disk has less
than `MIN_FREE_BYTES` free, or by default 15% of its size, so they never fill
a disk the rest of the machine depends on: the kubelet starts evicting pods
at 10% free. A finished upload goes to the bucket, under `upload/<id>`, until
its job ends.

The bucket can have a cache on this machine's disk (`DISK_CACHE_BYTES`, in
`STATE_DIR/cache`): what was read or written recently,
kept whole, each object at most a quarter of the budget, the least recently
used going first. A read from it costs no round trip, and it keeps serving
what it holds while the bucket is unreachable, which matters when the bucket
is at home and the server is not. A miss is served from the bucket and
fetched to disk behind it; an upload is written to disk as it is stored,
since its owner looks at it next. It fills only while the state disk keeps
`MIN_FREE_BYTES` free, so that setting leaves room for it, and an upload short
of room takes its room back from the cache.

Requests for an image in neither cache wait for the first to fetch it,
rather than each fetching it, so a link shared widely costs the bucket one
read. A video is read in ranges and not held in memory: it is served from
the disk cache, which takes a new one as it is made, or else from the bucket.

### Moving to another bucket

```sh
# Copy everything, the database's copies included. It resumes if interrupted.
# Both buckets are read from the AWS_* variables, so they share an endpoint
# and keys; copy through a machine that reaches both.
dither copy-store s3://<from> s3://<to>
```

Then point `STORAGE` at the new bucket (in Kubernetes, the component's
`bucket`). Uploads made between the copy and the switch are not in it, so run
the copy again just after switching; it skips what is already there.

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
album, and its file stored in the bucket for a worker. Finishing runs in a task the
request only waits on, so a client that disconnects does not stop it, and
asking again reads how it ended.

### Processing

A worker, `dither worker`, makes uploads' copies: the web process decodes
nothing. Workers form a pool. Any number may run, on any machine that
reaches the web process's worker API, and all pull from one queue, so the
work goes to whichever has room. A worker holds neither the database nor
the store, nor any secret but the API's token: it asks for a job, fetches
the upload, makes it, sends back what it made and says how the job ended,
and the web process records it. In Kubernetes each worker is a Deployment of
its own (`createDitherWorker` in the Pulumi component), placed on whichever
node should do the work; without one, uploads queue and wait. `just dev` runs
one beside the server.

A worker fetches an upload from the bucket, and puts what it makes into it,
directly, at URLs the web process signs for that job alone and for an hour:
it holds none of the bucket's keys, and a worker beside the bucket moves
nothing across the network to the web process. What it puts goes aside first
(`work/<id>/<worker>/<variant>`); the web process checks its length and moves
it into place only while the job is still that worker's, and records the
original's length and SHA-256, which the worker computes. Without a bucket
that signs (memory), or when a worker cannot reach the bucket or it refuses
the URL, the worker API carries the bytes both ways instead.

### Moving off a directory store

Versions before 0.10 could keep images in a directory. `dither copy-store
file:///data/objects s3://<bucket>` copies them into a bucket, with the
bucket's `AWS_*` variables set; then set `STORAGE` to the bucket. Uploads an
earlier version left waiting for a worker are moved into the bucket when the
web process starts.

The worker API (`web::work`) is served on a port of its own
(`WORKER_ADDR`), never through the ingress. Anywhere but loopback it
requires `WORKER_TOKEN`, which workers present. A worker asking for a job is held until one is waiting, so a
queued job starts at once without polling.

The queue is the database. A worker claims a job with a lease it renews while
it works, so a worker that stops leaves its jobs for the next once their
leases lapse (a minute), and one stopped by a signal gives them back at
once. A worker reports a job's progress every second as it moves, which the
album page shows as it happens, and each report renews the lease; a renewal
refused (the image was deleted, or the lease lapsed and another worker took
the job) stops it. A job that fails on this side is tried again later, three times; one
whose file is at fault fails at once. Pictures are made several at a time
(`PICTURE_WORKERS`) under a budget of memory (`PROCESS_MEMORY_BYTES`), each
one's share worked out from its header in bytes rather than pixels: a 16-bit
PNG decodes to twice an 8-bit one's size, and a photo turning upright needs a
second copy. Videos are made one at a time (`VIDEO_WORKERS`).

The database counts the bytes that arrived and the file on disk holds them.
Where the two disagree (a disk restored without an upload's file, or a crash
before its last bytes reached the disk), the file wins: the count drops to
what it holds and the client sends the rest.

The same JSON API serves anything else that wants to upload:

```text
POST   /api/albums                 {title?}                 → {id}
POST   /api/albums/{id}/uploads    {files: [{name, size, type?}], as_recorded?} → {uploads: [{id}], chunk}
PUT    /api/uploads/{id}           Content-Range: bytes a-b/size, the bytes → {received}
GET    /api/uploads/{id}                                    → {received, size, image?, error?}
POST   /api/uploads/{id}/finish                             → {image: {id, page, file, thumb, processing, …}} | {error}
DELETE /api/uploads/{id}
```

A file's `type`, the browser's MIME type, holds a video to the video limit
rather than the image one; the bytes decide what a file is when it finishes.
A video finishes `processing`, and what is kept of it follows.

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
kōan's: [@radiosilence/theme](https://github.com/radiosilence/theme) supplies
the tokens, the element defaults and the controls (button weights, the meter,
notices, sheets), and `styles/app.css` holds only dither's accent, layout and
its own components.

## Signing in

dither is an OAuth client of an OpenID Connect provider (`OIDC_ISSUER`) and
has no allowlist of its own: who may sign in is the provider's decision.
Every person who signs in gets albums of their own and sees nobody else's.
Sessions are opaque ids in `__Host-` cookies, stored hashed. A sign-in lasts
two days, and a page load a day after it goes back through the provider,
which remembers the person and asks nothing, but decides again whether they
may sign in: someone the provider stops letting in is out of dither within a
day. An upload or a live page already going carries on for the second day.

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
  at a lower priority, with a time limit, and confined by Landlock, seccomp
  and resource limits to the files it was given (see *Videos*).
- The web process decodes nothing: every picture is made by the worker.
- Sign-ins are checked with the provider again a day after they are made
  (see *Signing in*).
- A file's type is decided by its signature, never by its name or the type
  the browser claims. Decoding is limited in pixels and allocation.
- A 404 is never cached, since a private album's files are missing to
  everyone but their owner, who would otherwise be shown a 404 a shared cache
  kept from someone else's request.
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
| `STORAGE` | required | The bucket (`s3://bucket[/prefix]`, with the `AWS_*` variables), or `memory:///` for development. A worker does not use it. |
| `AWS_ENDPOINT`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | — | For an `s3://` store. R2's region is `auto`. |
| `STATE_DIR` | `data/state` | The database and uploads in progress, on a local disk. |
| `MIN_FREE_BYTES` | 15% of the disk | Uploads in progress are refused below this much free space on the state directory's disk, and no file ffmpeg writes may take a worker's disk below it. |
| `MAX_UPLOAD_BYTES`, `MAX_PIXELS` | 64 000 000, 100 000 000 | Per image. |
| `PROCESS_MEMORY_BYTES` | 1 GiB | Memory a worker's pictures take at once. A picture that needs more alone is refused when its upload finishes. |
| `CACHE_BYTES` | 256 MiB | Recently served images kept in memory. |
| `DISK_CACHE_BYTES` | 0 | With a bucket, images kept on this machine's disk as well; 0 for none. |
| `TRUSTED_PROXIES` | unset | Comma-separated addresses or CIDR ranges of a CDN in front, passed over in `X-Forwarded-For` when counting visitors. |
| `ADDRESS_HEADER` | unset | The header a trusted proxy puts the client's address in (`CF-Connecting-IP`), read only on requests from `TRUSTED_PROXIES`. |
| `VIDEO` | `auto` | `auto` takes videos when ffmpeg and ffprobe run; `on` fails startup without them; `off` takes none. |
| `FFMPEG`, `FFPROBE` | `ffmpeg`, `ffprobe` | The tools, by path or on `PATH`. The image sets both. |
| `MAX_VIDEO_BYTES`, `MAX_VIDEO_SECONDS` | 1 000 000 000, 1800 | Per video. |
| `VIDEO_THREADS` | 2 | Threads an ffmpeg run uses, filters included. A conversion also takes about 1.5 GB of memory at 4K, 2 GB from an 8K source, beside `PROCESS_MEMORY_BYTES`. |
| `PICTURE_WORKERS`, `VIDEO_WORKERS` | 4, 1 | Jobs a worker makes at once. `VIDEO_WORKERS=0` leaves videos to other workers. |
| `WORKER_ADDR` | `127.0.0.1:8081` | Where the web process serves the worker API. Anything but loopback requires `WORKER_TOKEN`. |
| `WORKER_TOKEN` | none | What workers present to the worker API. |
| `QUEUE_URL` | `http://127.0.0.1:8081` | Where a worker reaches the worker API. |
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
token even to read (`just auth` writes it from the gh login). It runs the
theme's kit-check first (`just kit`), which fails on classes outside the look:
Tailwind's palette, radii, shadows and type sizes are removed by the theme, so
a leftover would compile to nothing and lose its style without an error.

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
