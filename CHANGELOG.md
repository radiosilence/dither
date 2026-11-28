# Changelog

## 0.18.0

### Changed

- **koan-kit-web 0.8.1.** The signed-in pages load in place through the kit's `Navigation`, so Back and Forward no longer reload a page, and a load that fails leaves the address as it was; the albums' cards load their album in place. A request that fails says so over the page, and one refused for want of a session goes to the sign-in page.
- **koan-kit-web 0.9.** The top bar, footer, rendered Markdown and move sheet are the kit's: the move sheet reads its albums as it opens and closes itself once a move is made or when the albums cannot be read, the sites linking to an image are listed one to a line, and the invite link's words describe its field. A flash no longer stops later failed requests from being reported.
- **koan-kit-web 0.9.1.** The move sheet hides its buttons while it reads the albums and closes with a flash if it cannot, and a live page reconnecting its stream no longer reports a failed request.

### Fixed

- **Uploading from the albums page.** Images dropped or chosen there made an empty album and then failed, since the answer making the album no longer said where its uploads go.

## 0.17.0

### Changed

- **The web layer is a routed application: one module and template directory per page, a stream per page.** Pages load in place and keep their address, each live page's stream is opened afresh as the address changes and draws only what changed since the page was drawn, every address is formatted in one place, and a guard test requests every page, region and stream with markup in every field a person controls. The accounts page moves to `/accounts` (`/users` redirects there), and the viewer's share snippets escape a caption properly, where they used to delete brackets, quotes and angle brackets from it.

### Fixed

- **A page whose live update failed is drawn again at once.** A stream that could not read what it draws now ends, and the page reopens one, rather than staying stale until the next change or a minute passed.

### Internal

- **askama-analyzer formats the templates.** `just fmt` runs its formatter, and CI runs `askama-analyzer fmt --check` as an advisory step until a later change formats the templates and makes it blocking. mise installs the latest release with its release-age wait off for this tool alone, since it is maintained alongside these projects and CI should run the release the editor runs.

## 0.16.0

### Changed

- **askama_datastar 0.3 with koan-kit-web 0.7.** The kit's slots take askama_datastar 0.3's `Html`, so the two move together. The live stream keeps each part it has sent in the crate's `Last` rather than a map keyed by name, and a test draws every page and live stream with markup-shaped titles, descriptions and captions and checks with the crate's escaping guard that each is escaped exactly once.

## 0.15.2

### Changed

- **The web kit is `@radiosilence/koan-kit-web`.** The same kit as `@radiosilence/theme`, renamed and now published from the koan repository; `primary` buttons are filled with the accent.
- **The pages are drawn with koan-kit-web's components.** Buttons, forms and fields, notices, empty states, meters, copy fields, the head and the footer are the kōan kit's Rust components, as on kōan's other servers, and the stylesheet and typeface come from the same release of the kit. Deleting images, an album or an account, and resetting a password, ask in the kit's one question sheet in place of the browser's `confirm()`, and a failure shows inside it. The security headers are askama_datastar 0.2's, so the app's pages send `Referrer-Policy: same-origin` rather than `no-referrer`.
- **koan-kit-web 0.6.** The video player's buttons, glyphs and loader, the move dialog's choices, the view counts and the copy fields are the kit's components, and every wait shows the kit's ensō. Rows of controls take the kit's compact size rather than restyled controls, deleting an album sends the browser to the albums through the kit's question, and the script's messages go through the kit's flash region. The player's toggles keep their names and say whether they are pressed, rather than renaming themselves. askama_datastar is 0.2.1, and the live stream no longer needs async-stream.

### Internal

- **The kit from crates.io.** koan-kit-web 0.5 is taken from its published release rather than a deleted branch of koan.

## 0.15.1

### Security

- **Accounts take kōan-auth 0.75.2 from crates.io.** It carries the fixes from a security review of the crate: removing an admin and resetting a password each happen in one transaction, so two admins cannot remove each other at once and a failed reset leaves no old credential alive.

## 0.15.0

### Added

- **Accounts of its own, by invite.** dither no longer signs in through an OpenID Connect provider: an admin makes accounts and sends invite links, the first admin is made on `/setup` with a code the server logs, and accounts are admins, users or read-only. `SIGNING_KEY` replaces `OIDC_*` and `INSECURE_NO_AUTH`, and the Pulumi component's `signingKey` replaces `oidc`. Albums made before are not kept.
- **Change your own password.** `/account`, from the username in the top bar, takes the current password and a new one; the browser it is changed from stays signed in and every other session ends. Read-only accounts change theirs too.

## 0.14.0

### Changed

- **Filled primary actions.** Add images, choose images and move are filled with the accent.

## 0.13.0

### Added

- **Visitors counted behind a CDN.** `TRUSTED_PROXIES` (the Pulumi component's `trustedProxies`) lists a CDN's edge ranges, which are passed over in `X-Forwarded-For`, so visitors are counted by their own address rather than the edge's.
- **Client address from a trusted proxy's header.** `ADDRESS_HEADER` (`addressHeader`) names the header a trusted proxy puts the client's address in, such as `CF-Connecting-IP`, for an ingress that does not keep the CDN's `X-Forwarded-For`; it is read only on requests from `TRUSTED_PROXIES`.

### Fixed

- **404s are never cached.** A private album's files are missing to everyone but their owner, so a shared cache keeping a stranger's 404 would show the owner the same.

## 0.12.0

### Changed

- **Lighter panels.** Panels are the kit's lighter ground rather than hairline boxes.

## 0.11.0

### Added

- **Markdown in captions and album descriptions.** CommonMark with GitHub's tables, strikethrough, task lists and bare links; raw HTML is shown as text, and link previews and alt text take the words without the markup.

### Changed

- **The icon has a transparent background.** The favicon and touch icon no longer sit on a dark square, so they match the browser tab or home screen behind them.
- **The controls come from the kōan web kit.** @radiosilence/theme 0.2.5 now draws the element defaults, button weights, meter, notice and sheet that dither's stylesheet used to carry, so they stay in step with the other apps. A confirmation or the move picker now rises from the bottom of a phone's screen.

## 0.10.5

### Changed

- **askama_datastar 0.1.2.** Fragment answers sent as `Html` vary on `Datastar-Request`, so a browser's cache keeps them apart from the page at the same address.

## 0.10.4

### Changed

- **The accent is magenta.** Controls, links and the icon use magenta instead of the shared theme's green, in both colour schemes.

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
