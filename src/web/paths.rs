//! Every address dither serves, as functions of ids: the files, the public
//! pages, the app's pages, its streams and its actions. The router's patterns
//! sit beside the functions that fill them, so a path is changed in one place.
//! Ids reach here checked by `id::valid` or made by dither, so none needs
//! escaping.

// Public pages and files.

pub const PUBLIC_ALBUM: &str = "/a/{id}";
/// An image's page (`/<id>`) or the image itself (`/<id>.<ext>`).
pub const IMAGE: &str = "/{slug}";
pub const THUMB: &str = "/t/{file}";
pub const DISPLAY: &str = "/d/{file}";
pub const PREVIEW: &str = "/p/{file}";

/// An album's public page.
pub fn public_album(id: &str) -> String {
    format!("/a/{id}")
}

/// An album's public page, at one of its images.
pub fn public_album_at(album: &str, image: &str) -> String {
    format!("/a/{album}#{image}")
}

/// An image's public page.
pub fn image_page(id: &str) -> String {
    format!("/{id}")
}

/// An image or video as it was uploaded (cleaned), by its extension.
pub fn original(id: &str, ext: &str) -> String {
    format!("/{id}.{ext}")
}

pub fn thumb(id: &str) -> String {
    format!("/t/{id}.webp")
}

/// A picture's display copy.
pub fn display_image(id: &str) -> String {
    format!("/d/{id}.webp")
}

/// A video's display copy: what plays.
pub fn display_video(id: &str) -> String {
    format!("/d/{id}.mp4")
}

/// The picture a link preview shows.
pub fn preview(id: &str) -> String {
    format!("/p/{id}.jpg")
}

/// `path` on the public pages' own address, `base` (`Config::public_url`):
/// what is copied and shared.
pub fn absolute(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

// The app's pages.

pub const ALBUMS: &str = "/";
pub const ALBUM: &str = "/albums/{id}";
pub const ACCOUNT: &str = "/account";
pub const ACCOUNTS: &str = "/accounts";

pub fn album(id: &str) -> String {
    format!("/albums/{id}")
}

/// The album an album page's address names: `/albums/<id>`, with or without
/// a query.
pub fn album_of(address: &str) -> Option<&str> {
    let path = address.split(['?', '#']).next()?;
    path.strip_prefix("/albums/")
        .filter(|id| crate::id::valid(id))
}

// Signing in and out, and joining.

pub const LOGIN: &str = "/login";
pub const SETUP: &str = "/setup";
pub const JOIN: &str = "/join";
pub const LOGOUT: &str = "/auth/logout";

/// Where to sign in so as to come back to `path`.
pub fn login_next(path: &str) -> String {
    if path == ALBUMS {
        return LOGIN.into();
    }
    format!("{LOGIN}?{}", query(&[("next", path)]))
}

/// The link an invited account's owner joins with, on the app's address
/// `base`. The invite and the username ride in the fragment, which browsers
/// never send, so no log holds them.
pub fn invite(base: &str, username: &str, token: &str) -> String {
    format!(
        "{base}{JOIN}#{}",
        query(&[("invite", token), ("username", username)])
    )
}

// The streams that keep the app's pages current.

pub const STREAM_ALBUMS: &str = "/stream/albums";
pub const STREAM_ALBUM: &str = "/stream/album";
pub const STREAM_QUIET: &str = "/stream/quiet";

// The album page's actions.

pub const ALBUM_META: &str = "/albums/{id}/meta";
pub const ALBUM_ORDER: &str = "/albums/{id}/order";
pub const ALBUM_COVER: &str = "/albums/{id}/cover";
pub const ALBUM_DELETE: &str = "/albums/{id}/delete";
pub const ALBUM_MOVER: &str = "/albums/{id}/mover";
pub const VIEWER: &str = "/images/{id}/view";
pub const CAPTION: &str = "/images/{id}/caption";
pub const MOVE: &str = "/images/move";
pub const DELETE_IMAGES: &str = "/images/delete";

/// Where an album's title, description and privacy are saved.
pub fn album_meta(id: &str) -> String {
    format!("/albums/{id}/meta")
}

/// Where the script posts an album's images in their new order.
pub fn album_order(id: &str) -> String {
    format!("/albums/{id}/order")
}

/// Setting the album's cover: the script adds the image it is to be.
pub fn album_cover(album: &str) -> String {
    format!("/albums/{album}/cover")
}

/// Making `image` the album's cover.
pub fn album_cover_of(album: &str, image: &str) -> String {
    format!("/albums/{album}/cover?{}", query(&[("image", image)]))
}

pub fn album_delete(id: &str) -> String {
    format!("/albums/{id}/delete")
}

/// The move dialog's body, for moving `images` out of `album`. The script
/// writes the images chosen into the selection's button as they change.
pub fn album_mover(album: &str, images: &[&str]) -> String {
    format!(
        "/albums/{album}/mover?{}",
        query(&[("ids", &images.join(","))])
    )
}

/// The move dialog's address before anything is chosen: the script appends
/// the images to it.
pub fn album_mover_base(album: &str) -> String {
    format!("/albums/{album}/mover")
}

/// An image in the album page's viewer.
pub fn viewer(image: &str) -> String {
    format!("/images/{image}/view")
}

pub fn caption(image: &str) -> String {
    format!("/images/{image}/caption")
}

/// Deleting `images`, named in the address, since the page's question posts
/// the page's signals rather than a body of its own.
pub fn delete_images(images: &[&str]) -> String {
    format!("{DELETE_IMAGES}?{}", query(&[("ids", &images.join(","))]))
}

// The uploader's JSON (`api`). The answers name the addresses that follow,
// so a client formats none of its own.

pub const API_ALBUMS: &str = "/api/albums";
pub const API_UPLOADS: &str = "/api/albums/{id}/uploads";
pub const API_UPLOAD: &str = "/api/uploads/{id}";
pub const API_FINISH: &str = "/api/uploads/{id}/finish";

/// Where an album's uploads are declared.
pub fn api_uploads(album: &str) -> String {
    format!("/api/albums/{album}/uploads")
}

/// An upload's own address: its chunks, its state, cancelling it.
pub fn api_upload(id: &str) -> String {
    format!("/api/uploads/{id}")
}

pub fn api_finish(id: &str) -> String {
    format!("/api/uploads/{id}/finish")
}

// The accounts page's actions.

pub const ACCOUNT_PASSWORD: &str = "/account/password";
pub const ACCOUNT_ACTION: &str = "/accounts/{id}/{action}";

/// What an admin can do to an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountAction {
    Invite,
    Reset,
    Role,
    Delete,
}

impl AccountAction {
    fn as_str(self) -> &'static str {
        match self {
            AccountAction::Invite => "invite",
            AccountAction::Reset => "reset",
            AccountAction::Role => "role",
            AccountAction::Delete => "delete",
        }
    }
}

pub fn account_action(id: i64, action: AccountAction) -> String {
    format!("/accounts/{id}/{}", action.as_str())
}

/// `pairs` as a query, each value encoded.
fn query(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_address_carries_where_to_return() {
        assert_eq!(login_next("/"), "/login");
        assert_eq!(
            login_next("/albums/a?b=c"),
            "/login?next=%2Falbums%2Fa%3Fb%3Dc"
        );
    }

    #[test]
    fn an_album_page_names_its_album() {
        assert_eq!(album_of("/albums/abcdefghij"), Some("abcdefghij"));
        assert_eq!(album_of("/albums/abcdefghij?x=1"), Some("abcdefghij"));
        assert_eq!(album_of("/albums/../x"), None);
        assert_eq!(album_of("/"), None);
    }

    #[test]
    fn a_route_and_its_address_agree() {
        let filled = |pattern: &str, id: &str| pattern.replace("{id}", id);
        assert_eq!(album("x"), filled(ALBUM, "x"));
        assert_eq!(album_meta("x"), filled(ALBUM_META, "x"));
        assert_eq!(album_order("x"), filled(ALBUM_ORDER, "x"));
        assert_eq!(album_delete("x"), filled(ALBUM_DELETE, "x"));
        assert_eq!(album_cover("x"), filled(ALBUM_COVER, "x"));
        assert_eq!(album_mover_base("x"), filled(ALBUM_MOVER, "x"));
        assert_eq!(viewer("x"), filled(VIEWER, "x"));
        assert_eq!(caption("x"), filled(CAPTION, "x"));
        assert_eq!(public_album("x"), filled(PUBLIC_ALBUM, "x"));
        assert_eq!(api_uploads("x"), filled(API_UPLOADS, "x"));
        assert_eq!(api_upload("x"), filled(API_UPLOAD, "x"));
        assert_eq!(api_finish("x"), filled(API_FINISH, "x"));
        assert_eq!(
            account_action(7, AccountAction::Reset),
            ACCOUNT_ACTION
                .replace("{id}", "7")
                .replace("{action}", "reset")
        );
    }
}
