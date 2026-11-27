# Architecture

dither is one binary that runs as a web process (`serve`) and as workers
(`worker`). The README describes what it does; CLAUDE.md the rules that hold
the design together. This document describes how the web layer is put
together. It follows slsk-mcp's (radiosilence/slsk-mcp, `ARCHITECTURE.md`),
the model for radiosilence's server-rendered apps.

## The web layer

Server-rendered HTML, patched over server-sent events by
[Datastar](https://data-star.dev). Markup comes from askama templates and the
components of [koan-kit-web](https://github.com/radiosilence/koan/tree/main/koan-kit-web);
the glue between them is [askama_datastar](https://github.com/radiosilence/askama-datastar).

```
src/web/mod.rs              the route table, and nothing else
src/web/paths.rs            every address dither serves, as functions of ids
src/web/state.rs            the state every handler shares
src/web/layers.rs           counting, preview logging, marked writes, hardening, app-host only
src/web/client.rs           the host asked for, the client's address, link preview fetchers
src/web/layout.rs           the document, title, header and main, and the templates' helpers
src/web/page.rs             the Page shape, Body, and how a page is drawn
src/web/page/<page>.rs      one module per signed-in page
src/web/page/album/         the album page's viewer and move dialog
src/web/stream.rs           the Live shape and how a stream is run
src/web/stream/<page>.rs    one module per page with live regions
src/web/sign.rs             signing in, setting up and joining
src/web/public/             the public pages, the files, and a video as the player draws it
src/web/api.rs              the uploader's JSON
src/web/work.rs             the worker API, on a port of its own
templates/layout.html and layout/   the document and its regions
templates/<page>/page.html          each page's body
templates/<page>/<region>.html      the regions it sends again
```

A page's module, its stream and its templates carry the name the header and
the title give it: Albums is `page/albums.rs`, `stream/albums.rs` and
`templates/albums/`. What belongs to a page lives with it: the album page's
viewer is `page/album/viewer.rs` and `templates/album/viewer/`.

### The route table

`web::router` declares every route in one place:

- **Pages**: Albums (`/`) and an album (`/albums/{id}`) read the session
  themselves, since `/` is the public domain's front page too and an album's
  address is what its owner copies; Account and Accounts are drawn by
  `page::serve` behind the session layer.
- **Streams**: `/stream/albums`, `/stream/album`, and `/stream/quiet` for a
  page with nothing live.
- **Actions**: everything a page posts to or reads in place, each a handler
  in its page's module; the uploader's JSON under `/api`.
- **Public**: the album and image pages, the files, `robots.txt`.

Every route's pattern is a constant in `paths`, beside the function that
fills it, and a test checks the two agree. Nothing else formats an address:
the script reads the ones it posts to from the page (`data-albums-api`, an
album's `data-uploads`, the selection's buttons), and the uploader's JSON
names each upload's own (`url`, `finish`).

### Pages

Every signed-in page implements `page::Page`:

```rust
impl Page for Albums {
    const NAME: Name = Name::Albums;                 // what the title and header call it
    const STREAM: &'static str = paths::STREAM_ALBUMS; // the stream it holds open
    type Args = ();                                  // what its handler hands it
    async fn body(s: &AppState, session: &Session, (): ()) -> AppResult<Body> { … }
}
```

`page::respond` draws any page two ways. A browser asking for the address
gets the layout around the body. The page itself asking, through a link that
loads in place (`kit::Link::in_place`, `kit::Button::in_place`, or a link
inside an element marked `data-kit-in-place`, such as the albums' cards), gets
the body morphed into `main`, the title and the header, and the address as
`$url` (`askama_datastar::address`). The layout's `kit::Navigation` owns the
history: it pushes `$url` once a load has settled, so a load that fails moves
nothing, and Back and Forward load their entry in place rather than reloading.
A reload or a shared link draws what the address names.

### Streams

Every live stream implements `stream::Live`:

```rust
impl Live for Album {
    const PROGRESS: bool = true;                                   // follows videos being made
    async fn open(s: &AppState, session: &Session, shown: &Shown) -> Self { … }
    async fn draw(&mut self, s: &AppState, session: &Session, c: &Changed) -> Vec<Event> { … }
}
```

`stream::serve::<L>` runs it. It waits on a change to its owner's albums or
images (`Db::changes`) and, for a stream that asks, on videos' progress
(`jobs::Hub::ticks`), and `askama_datastar::paced` spaces the redraws at most
one every 250 ms and ends the stream after a minute. It also ends when the
server begins to stop or the account changes. Each region is sent only when
it differs from what was last sent (`askama_datastar::Last`).

A page opens its stream from `main`, through `askama_datastar::reopen_on`
with `url`: it is opened again when it ends or goes quiet, and afresh when
`$url` changes, as it does when a page loads in place, so the stream always
draws the page the address names. `url` and `seen` are declared on every
page; a signal `reopen_on` names that a page does not declare stops the
stream reopening without a console message.

An opening is cheap. `Db::seen` says how far each owner's changes have gone:
a random id drawn as the process starts, and a count of changes since. A page
records it before its body is drawn (the `seen` signal), and its stream's
first pass draws only if it has moved since; one recorded before a restart
never matches, since the counts restart with the process. After a pass that
drew the owner's regions, the stream sets `seen`, so the stream that replaces
it carries on from there. A pass that drew none of them (a video's progress
alone) leaves `seen` where it was. One that failed to draw them (the database
did not answer) ends the stream with `seen` where it was, and the stream the
page opens next draws from it. A stream never starts work: everything it draws
is read from the database and the job hub.

### Actions

An action changes something and answers with as little as it can:

- When the page's stream draws what changed (deleting or moving images,
  setting the cover's mark), the action answers with nothing, a flash, or the
  page's question closed (`kit::Confirm::close`).
- When nothing streams the region (the accounts page, a password change, the
  viewer), the action answers with the region it changed.
- When the action removes the page (deleting the album), it sends the browser
  elsewhere (`askama_datastar::go`).
- Fields saved as they are typed (a title, a caption) answer with nothing.

A button posts the page's signals rather than a form, so the images it acts
on travel in its address; the script writes the images chosen into the
selection's buttons as the choice changes.

### Templates and markup

Every view is a struct deriving `Template` with a file under `templates/`. A
view embeds another as `askama_datastar::Html` (rendered with `Draw::draw`),
which askama writes as markup, and escapes every other value. Nothing builds
markup with `format!`, the one `Html::trusted` is `markdown::html`, a
sanitiser with its tests, and no template uses `|safe`. A share snippet is a
template too (`album/viewer/share.html`, `.md`, `.bbcode`), so a caption in
an `alt` is escaped as an attribute and in Markdown as link text.

Datastar expressions are constants. A value that differs per element goes in
a `data-*` attribute, escaped, and the expression reads `el.dataset`. Signals
are named by the program: `url` and `seen`, which the server reads, and the
page's own, which it never does and which start with `_` (`_newuser`,
`_current`, `_password`, `_confirm`), named once as constants beside the
handler that empties them.

Vocabularies are typed: a page's name (`layout::Name`), an account's role
(`koan_auth::Role`), why a video failed (`jobs::Failure`), what an admin does
to an account (`paths::AccountAction`), and why an account or a password was
not changed. Templates word them in `match`es and macros.

### Patch targets

Datastar morphs a region by the id of its top-level element, so ids exist only
where the server sends a region again, declared once in its template:

| Where | Ids |
|---|---|
| Layout | `title`, `header`, `main` |
| Albums | `albums`, `usage` |
| Album | `grid`, `summary`, `foot`, each video's bar `vp-<id>`, `viewer-body`, `mover-body` |
| Account, Accounts | `outcome`; `accounts` |

Dialogs carry ids the buttons that open and close them name (`viewer`,
`mover`). Two ids are the morph's identity rather than targets: a caption's
`caption-<id>`, so text typed into one image's caption is not carried into
the next, and a player's `player-<id>`, so a redrawn viewer keeps playing the
same video. A tile is found by its `data-id`.

### The kit

Every control, status, meter, notice, flash, empty state, question and card
is a koan-kit-web component; templates add layout utilities only. The upload
tray's parts are the layout's `<template>`s drawn with the kit, which the
script clones and fills. The layout's flash region (`layout/flash.html`) says
over the page when a request's answer is an error or the server cannot be
reached, and sends a 401 to sign in. What the kit cannot express yet (a
full-screen media viewer, among others) is tracked on radiosilence/koan#2365.

### The script

`browser/app.ts` holds what lives in the browser: files on their way, what
is selected, a drag in progress, the viewer's keys and swipes. Pages load in
place, so it holds no element of a page: it listens on the document, observes
`main`, and reads the addresses it posts to from the page and the API's
answers as it acts. Every
word it shows is the layout's `<template id="copy">`.

### The guard

`web::escaping` builds the real router against the test fixture
(`web::fixture`), fills every field a person controls with markup-shaped
text (a title, a description, a caption, an upload's name, a username), and
requests every page whole and in place, every stream, every region a page
reads, the answers to the actions that draw what a person wrote (making an
account, setting the cover, moving images), the public pages, joining, and a
refused sign-in. The set-up page, which a server with accounts never shows,
is checked drawn from its template (`sign::tests`). `askama_datastar::escaping::Guard`,
told the hostile texts and the templates directory, finds markup of the
templates' own escaped (a fragment spliced in as text), text escaped twice,
or text left as markup, and the test checks each page shows the text as
`escaping::escaped` writes it. A new page, region, stream or field a person
controls belongs in it.
