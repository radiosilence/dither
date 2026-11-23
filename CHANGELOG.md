# Changelog

## 0.3.0

### Fixed

- **Link previews show their picture on WhatsApp.** A page's `og:image` is now a JPEG of at most 1200 pixels and 300 KB at `/p/<id>.jpg`, made at upload, or the first time a preview asks for an image uploaded earlier. It was the original, or the WebP display copy above 5 MB, and WhatsApp shows nothing for an image that large. An animated picture under 5 MB keeps its original, so it still plays where a preview can.

## 0.2.0

### Internal

- **Words through gettext, and a typed script.** Every word the pages and the uploader show is a gettext message in a template, collected in `locales/en-GB/messages.po`, so a translation is a catalogue rather than a code change. The uploader's script is TypeScript, typechecked in CI and stripped of its types as the binary builds.

## 0.1.0

### Added

- **An image host.** Albums of images with captions, uploaded behind the estate's sign-in in resumable chunks and shared by unlisted links whose pages preview on Reddit, Discord and the rest. Uploads are stripped of location and camera metadata before anything is stored; images and copies of the database go to a directory or an S3 bucket.
