# Changelog

## 0.5.0

### Added

- **An album's app address works as a link.** Anyone but its owner who opens `/albums/<id>`, link previews included, is sent to its public page, unless it is private. Link previews' fetchers are logged with what they asked for and were answered.

### Changed

- **One copy of each video.** A video is converted to H.264 at its own size (up to 4K, CRF 18, SDR, AAC), which plays everywhere and is what downloads, or with *keep videos as recorded* ticked, kept exactly as recorded; nothing else is stored. Videos already H.264 are not re-encoded, and a frame of an HDR video is tone-mapped for its thumbnail.

### Fixed

- **A caption stays with its image.** After a caption was typed, moving to the next image in the viewer showed the same text in its caption field; each image's field is now its own, and leaving one saves it at once.

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
