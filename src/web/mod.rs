//! HTTP: the app behind sign-in, the public pages and image files, and the
//! JSON the uploader speaks.
//!
//! [`router`] declares every route, in one place. The app's pages are modules
//! of [`page`], named as the header and the title name them, each drawn by
//! `page::serve` or `page::respond`; each page with live regions has a stream
//! of the same name under [`stream`]; an action is a handler in its page's
//! module. Every path is formatted by [`paths`].
//!
//! One router serves both hosts. While the public pages share the app's
//! hostname that is the whole story; once they have a domain of their own,
//! requests for that domain reach the public routes only, and it never sets
//! or receives a cookie. Every route behind sign-in sits under one session
//! layer, so a route added there later is protected by where it is added.

pub mod api;
pub mod assets;
pub mod client;
mod layers;
pub mod layout;
mod page;
pub mod paths;
pub mod public;
mod sign;
mod state;
mod stream;
pub mod work;

#[cfg(test)]
mod escaping;
#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;

use askama_datastar::{Csp, SecurityHeaders};
use axum::Router;
use axum::middleware;
use axum::response::Redirect;
use axum::routing::{get, post, put};

pub use layout::human;
pub use state::{AppState, Inner};

/// Pages and fragments of the app: scripts from here only (Datastar needs
/// `unsafe-eval` for its expressions), no inline script or style, images from
/// here and from previews of files about to be uploaded. A route with a
/// stricter policy, as the public pages and the files have, sets its own.
fn app_policy() -> Csp {
    Csp::new().datastar().allow("img-src", "blob:")
}

pub fn router(state: AppState) -> Router {
    use page::{account, accounts, album, albums};

    // Behind sign-in: the pages, their streams and their actions. Albums and
    // an album are drawn outside the session layer, below, since each reads
    // the session itself.
    let streams = Router::new()
        .route(
            paths::STREAM_ALBUMS,
            get(stream::serve::<stream::albums::Albums>),
        )
        .route(
            paths::STREAM_ALBUM,
            get(stream::serve::<stream::album::Album>),
        )
        .route(paths::STREAM_QUIET, get(stream::serve::<stream::Quiet>));
    let actions = Router::new()
        // An album
        .route(paths::ALBUM_META, post(album::meta))
        .route(paths::ALBUM_ORDER, post(album::order))
        .route(paths::ALBUM_COVER, post(album::cover))
        .route(paths::ALBUM_DELETE, post(album::delete))
        .route(paths::ALBUM_MOVER, get(album::mover::mover))
        .route(paths::VIEWER, get(album::viewer::view))
        .route(paths::CAPTION, post(album::caption))
        .route(paths::MOVE, post(album::mover::move_images))
        .route(paths::DELETE_IMAGES, post(album::delete_images))
        // The uploader's JSON
        .route("/api/albums", post(api::create_album))
        .route("/api/albums/{id}/uploads", post(api::begin))
        .route(
            "/api/uploads/{id}",
            put(api::chunk).get(api::status).delete(api::cancel),
        )
        .route("/api/uploads/{id}/finish", post(api::finish));
    // Accounts: the admins' page and its actions.
    let admin = Router::new()
        .route(
            paths::ACCOUNTS,
            get(page::serve::<accounts::Accounts>).post(accounts::create),
        )
        .route(paths::ACCOUNT_ACTION, post(accounts::act))
        .route(
            "/users",
            get(|| async { Redirect::permanent(paths::ACCOUNTS) }),
        )
        .layer(middleware::from_fn(crate::auth::extract::require_admin));
    // Account: its own password is a readonly account's one write.
    let own = Router::new()
        .route(paths::ACCOUNT, get(page::serve::<account::Account>))
        .route(paths::ACCOUNT_PASSWORD, post(account::change_password));
    let protected = Router::new()
        .merge(streams)
        .merge(actions)
        .merge(admin)
        .layer(middleware::from_fn(crate::auth::extract::require_writer))
        .merge(own)
        .layer(middleware::from_fn(layers::marked_writes))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::extract::require_session,
        ));
    let app_only = Router::new()
        .route(paths::ALBUM, get(album::page))
        .merge(protected)
        .route(paths::LOGIN, get(sign::login_form).post(sign::login))
        .route(paths::SETUP, get(sign::setup_form).post(sign::setup))
        .route(paths::JOIN, get(sign::join_form).post(sign::join))
        .route(paths::LOGOUT, get(sign::logout))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            layers::app_host_only,
        ));
    Router::new()
        .route(paths::ALBUMS, get(albums::home))
        .merge(app_only)
        .route(paths::PUBLIC_ALBUM, get(public::pages::album))
        .route(paths::THUMB, get(public::files::thumb))
        .route(paths::DISPLAY, get(public::files::display))
        .route(paths::PREVIEW, get(public::files::preview))
        .route(paths::IMAGE, get(public::pages::image))
        .route("/healthz", get(|| async { "ok" }))
        .route("/robots.txt", get(public::files::robots))
        .route("/favicon.ico", get(assets::favicon))
        .route("/assets/datastar.js", get(askama_datastar::script::serve))
        .route("/assets/geist-mono.woff2", get(koan_kit_web::font::serve))
        .route("/assets/{name}", get(assets::serve))
        .fallback(public::files::not_found)
        .layer(SecurityHeaders::new(&app_policy()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            layers::hardening,
        ))
        .layer(middleware::from_fn(layers::preview_log))
        // Video is compressed already, and a compressed range would not be
        // the range asked for.
        .layer(
            tower_http::compression::CompressionLayer::new().compress_when(
                tower_http::compression::Predicate::and(
                    tower_http::compression::DefaultPredicate::new(),
                    tower_http::compression::predicate::NotForContentType::const_new("video/"),
                ),
            ),
        )
        .layer(middleware::from_fn_with_state(state.clone(), layers::count))
        .with_state(state)
}
