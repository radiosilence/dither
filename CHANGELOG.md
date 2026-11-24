# Changelog

## 0.10.3

### Changed

- **Datastar from askama_datastar.** The script (Datastar 1.0.2 to 1.0.5), patches and the live stream come from the shared crate; the stream is no longer stored by any cache.

### Fixed

- **Live pages after a deploy.** The library and album pages reopen their live stream every minute and when the tab is shown again, rather than going still until reloaded.

## 0.10.2

### Fixed

- **A widely shared image costs the bucket one read.** Requests for an image in neither cache wait for the first to fetch it and share what it found, where each fetched it before: 200 at once for a new thumbnail made 199 reads of the bucket. The disk cache's floor above `MIN_FREE_BYTES` is one largest upload rather than two, which on a small disk left it next to nothing. Range requests that miss now count in `dither_cache_misses_total`, other than a worker's reads of uploads.

## 0.10.1

### Added

- **The app shows its version.** The signed-in pages end with the running version, linking to its release notes; the public pages do not say what they run.

## 0.10.0

### Changed

- **Workers read and write the bucket directly, and the bucket is the only store.** A finished upload goes to the bucket; a worker fetches it, and puts what it makes, at URLs the web process signs for that job alone, so a worker beside the bucket sends nothing through the web process and still holds none of its keys. The local-directory store is gone: `STORAGE` is an `s3://` bucket, or `memory:///` for development (`just dev` runs on one, with a fresh state directory each time). The Pulumi component requires `bucket`, and `createDitherWorker` takes the bucket's pods as `store` for its egress policy.

## 0.9.0

### Added

- **Anonymous view and bandwidth counts.** An owner sees, in the viewer and on an album's page, the views, visitors and bytes its public pages and files were served over the last 30 days and in all, the link previews that fetched it and the sites that linked to it. Visitors are counted by a hash with a salt held only in memory and replaced daily; no address, user agent or hash is stored.

## 0.8.0

### Added

- **A disk cache for a bucket elsewhere.** With `DISK_CACHE_BYTES`, images read or written recently are also kept whole on the serving machine's disk, so they cost no round trip and are served while the bucket is unreachable, as when it is at home and the server is not. The Pulumi component takes `diskCacheBytes`, and a `bucket` in the cluster (`peer`), which its egress policy allows and reaches over plain HTTP. A worker says when the queue is reachable again after losing it.

## 0.7.0

### Security

- **ffmpeg runs confined by the kernel.** Each ffmpeg and ffprobe run reads only the programs' directories and the file it was given and writes only in its own working directory (Landlock: not the database, the store or `/proc`, and no TCP); seccomp refuses it sockets, other processes, new namespaces, io_uring, BPF, perf events, userfaultfd and keyrings; no file it writes may take the disk below `MIN_FREE_BYTES`, and the worker keeps its own memory and environment out of `/proc`.
- **The Pulumi package pins the image by digest.** CI writes the released image's digest into the package it publishes, so a tag moved later changes nothing that is deployed; the worker is given `MIN_FREE_BYTES` too.
- **SEI messages are stripped from videos kept as they are.** H.264 and HEVC streams lose the SEI where encoders write their settings and some cameras the time or where they were, both in the frames and in an HEVC stream's configuration.
- **Sign-ins are checked with the provider daily.** A session lasts two days instead of thirty, and a page load a day after signing in goes back through the identity provider, which asks the person nothing but decides again whether they may sign in.

### Removed

- **The web process no longer makes link previews.** An image from before 0.3.0 without one gets none: every picture is made by the worker, and the web process decodes nothing.

### Fixed

- **A video's file name no longer reaches its public pages.** A video with no caption was titled for the media controls with the name it was uploaded under, which is its owner's alone; it is now "a video".

## 0.6.0

### Added

- **Workers form a pool.** Any number of workers, on any machine that reaches the web process's worker API, pull from one queue, so processing runs wherever there is room: a worker asks for a job and is held until one waits, fetches the upload, reports progress, sends back what it made and says how the job ended. Only the web process opens the database and the store, so a worker holds no store keys, only the API's token (`WORKER_TOKEN`, required when `WORKER_ADDR` is not loopback). In the Pulumi component, workers are Deployments of their own: `createDither` requires `workerToken` and returns `queue`, and `createDitherWorker` runs a worker on whichever node it names. Without one, uploads queue and nothing is made of them.

## 0.5.0

### Added

- **An album's app address works as a link.** Anyone but its owner who opens `/albums/<id>`, link previews included, is sent to its public page, unless it is private. Link previews' fetchers are logged with what they asked for and were answered.

### Changed

- **One copy of each video.** A video is converted to H.264 at its own size (up to 4K, CRF 18, SDR, AAC), which plays everywhere and is what downloads, or with *keep videos as recorded* ticked, kept exactly as recorded; nothing else is stored. Videos already H.264 are not re-encoded, and a frame of an HDR video is tone-mapped for its thumbnail.
- **The worker's default memory limit is 4 GiB.** Converting at full size takes about 1.5 GB at 4K and 2 GB from an 8K source, beside the pictures' `PROCESS_MEMORY_BYTES`. A deployment that sets `workerLimits` should allow as much.

### Fixed

- **A caption stays with its image.** After a caption was typed, moving to the next image in the viewer showed the same text in its caption field; each image's field is now its own, and leaving one saves it at once.
- **A video file opens in the browser's own player.** Opened on its own, a video file showed iOS Safari's player tiny and never loading: it was sandboxed, and a sandboxed document's origin does not match `media-src 'self'`. Video files are no longer sandboxed; images still are.

## 0.4.0

### Added

- **Videos, and a worker that processes uploads.** MP4, MOV and WebM uploads are kept as MP4 originals without their metadata, with an H.264 playback copy when browsers would not play the original, a poster frame for thumbnails and link previews, byte-range serving, `og:video` tags and a player. Every upload, picture or video, is now made by `dither worker`, a second container beside the web process: finishing an upload checks its header and queues it in the database, and the worker claims jobs with a lease, reports progress to the album page over a socket, and runs ffmpeg (shipped in the image) without the service's secrets.

### Internal

- **The security audit runs cargo-audit directly.** The action it replaces failed now and then updating its own check run after a clean audit, failing releases.

## 0.3.0

### Fixed

- **Link previews show their picture on WhatsApp.** A page's `og:image` is now a JPEG of at most 1200 pixels and 300 KB at `/p/<id>.jpg`, made at upload, or the first time a preview asks for an image uploaded earlier. It was the original, or the WebP display copy above 5 MB, and WhatsApp shows nothing for an image that large. An animated picture under 5 MB keeps its original, so it still plays where a preview can.

## 0.2.0

### Internal

- **Words through gettext, and a typed script.** Every word the pages and the uploader show is a gettext message in a template, collected in `locales/en-GB/messages.po`, so a translation is a catalogue rather than a code change. The uploader's script is TypeScript, typechecked in CI and stripped of its types as the binary builds.

## 0.1.0

### Added

- **An image host.** Albums of images with captions, uploaded behind the estate's sign-in in resumable chunks and shared by unlisted links whose pages preview on Reddit, Discord and the rest. Uploads are stripped of location and camera metadata before anything is stored; images and copies of the database go to a directory or an S3 bucket.
