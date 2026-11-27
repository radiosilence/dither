//! Every page, whole and in place, every stream and every region a page
//! reads, drawn with markup-shaped text in every field a person controls: an
//! album's title and description, an image's caption and the name it was
//! uploaded under, and a username. Each answer must carry that text escaped
//! exactly once, and no markup of the templates' own may come out escaped:
//! that is a fragment spliced in as text, which shows a page of raw HTML.
//!
//! A new page, region, stream or field a person controls belongs here.

use std::time::Duration;

use askama_datastar::escaping::{Guard, escaped, send};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;

use super::fixture::{App, app, jpeg, made};
use super::paths;
use crate::auth::cookie::SESSION_COOKIE;

/// Text shaped like markup, with every character escaping changes.
const HOSTILE: &str = r#"<em>hostile "q" & 'x'</em>"#;
/// A username shaped like markup: one may hold anything but spaces.
const NAME: &str = r#"<b>who</b>"&'"#;

/// What the router answers to a GET of `uri` as `cookie`'s browser loading
/// it, or as Datastar asking (`in_place`): a stream is cut off having sent
/// what it draws first.
async fn get(app: &App, uri: &str, cookie: &str, in_place: bool) -> (StatusCode, String) {
    let mut request = Request::get(uri)
        .header(header::HOST, "app.test")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
    if in_place {
        request = request.header("datastar-request", "true");
    }
    let request = request.body(Body::empty()).unwrap();
    send(&app.router, request, Duration::from_millis(700)).await
}

/// A stream's address, with the signals of the page at `url` that has drawn
/// nothing yet, so the stream draws it whole.
fn stream(path: &str, url: &str) -> String {
    let signals = json!({ "url": url }).to_string();
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("datastar", &signals)
        .finish();
    format!("{path}?{query}")
}

/// `answer` without the value of the viewer's HTML snippet. The snippet is
/// markup shown as text, to be pasted into another page, so the caption in
/// its `alt` is escaped as the snippet's attribute and again as the copy
/// field's: twice, by right. `viewer::tests` checks the snippet itself.
fn without_html_snippet(answer: &str) -> String {
    let Some(name) = answer.find(r#"name="html""#) else {
        return answer.to_string();
    };
    let Some(start) = answer[..name].rfind(r#"value=""#) else {
        return answer.to_string();
    };
    let end = start + answer[start + 7..].find('"').map_or(0, |n| n + 8);
    format!("{}{}", &answer[..start], &answer[end..])
}

#[tokio::test]
async fn markup_from_people_is_escaped_once_everywhere() {
    let mut app = app().await;
    let admin = app.cookie.clone();
    // The person whose albums these are, named with markup.
    let owner = app.session(NAME).await;
    app.cookie = owner.clone();

    let album = app.post("/api/albums", json!({ "title": "" })).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let file = jpeg(40, 30);
    let begun = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": format!("{HOSTILE}.jpg"), "size": file.len() }] }),
        )
        .await;
    let upload = begun.json()["uploads"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    app.chunk(&upload, &file, 0, file.len()).await;
    let finished = app
        .post(&format!("/api/uploads/{upload}/finish"), json!({}))
        .await;
    let image = finished.json()["image"]["id"].as_str().unwrap().to_string();
    made(&app, &image).await;
    for (uri, body) in [
        (
            paths::album_meta(&album),
            json!({ "title": HOSTILE, "description": HOSTILE }),
        ),
        (paths::caption(&image), json!({ "caption": HOSTILE })),
    ] {
        let edited = app.post(&uri, body).await;
        assert!(edited.status.is_success(), "{uri}: {}", edited.status);
    }
    // A second album, for the move dialog to offer.
    app.post("/api/albums", json!({ "title": HOSTILE })).await;

    let guard = Guard::new(&[HOSTILE, NAME])
        .templates(concat!(env!("CARGO_MANIFEST_DIR"), "/templates"))
        .expect("templates/")
        // The kit's components write these.
        .elements([
            "button", "svg", "path", "label", "input", "dialog", "progress", "select", "option",
            "textarea", "a", "p", "div", "span", "form",
        ]);
    let mut found = Vec::new();
    let mut check = |at: &str, status: StatusCode, answer: &str, shows: &[&str]| {
        let answer = &without_html_snippet(answer);
        if !(status.is_success() || status.is_redirection()) {
            found.push(format!("{at}: {status}"));
        }
        found.extend(guard.problems(answer).iter().map(|p| format!("{at}: {p}")));
        for text in shows {
            if !answer.contains(&escaped(text)) {
                found.push(format!("{at}: {text:?} is missing"));
            }
        }
    };

    let album_page = paths::album(&album);
    // Each signed-in page, as whom, and what it must show.
    let pages: [(&str, &str, &[&str]); 4] = [
        (paths::ALBUMS, &owner, &[HOSTILE, NAME]),
        (&album_page, &owner, &[HOSTILE, NAME]),
        (paths::ACCOUNT, &owner, &[NAME]),
        (paths::ACCOUNTS, &admin, &[NAME]),
    ];
    for (uri, cookie, shows) in pages {
        for in_place in [false, true] {
            let (status, text) = get(&app, uri, cookie, in_place).await;
            check(
                &format!("{uri} (in place: {in_place})"),
                status,
                &text,
                shows,
            );
        }
    }
    // The regions the album page reads, and its streams.
    let regions = [
        (paths::viewer(&image), &[HOSTILE][..]),
        (paths::album_mover(&album, &[&image]), &[HOSTILE][..]),
        (stream(paths::STREAM_ALBUMS, paths::ALBUMS), &[HOSTILE][..]),
        (stream(paths::STREAM_ALBUM, &album_page), &[HOSTILE][..]),
        (stream(paths::STREAM_QUIET, paths::ACCOUNT), &[][..]),
    ];
    for (uri, shows) in regions {
        let (status, text) = get(&app, &uri, &owner, true).await;
        check(&uri, status, &text, shows);
    }
    // What anyone with the link sees.
    for uri in [paths::public_album(&album), paths::image_page(&image)] {
        let (status, text) = get(&app, &uri, "", false).await;
        check(&uri, status, &text, &[HOSTILE]);
    }
    // A refused sign-in shows the name it was given back.
    let refused = app
        .send(
            Request::post(paths::LOGIN)
                .header(header::HOST, "app.test")
                .header(header::ORIGIN, "http://app.test")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("username", NAME)
                        .append_pair("password", "wrong")
                        .finish(),
                ))
                .unwrap(),
        )
        .await;
    check(paths::LOGIN, StatusCode::OK, &refused.text(), &[NAME]);
    assert_eq!(found, Vec::<String>::new());
}
