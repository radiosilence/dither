//! Captions and album descriptions as Markdown: CommonMark with GitHub's
//! tables, strikethrough, task lists and bare links.
//!
//! The text is the owner's, shown on public pages, so nothing in it reaches
//! a page as markup: raw HTML is shown as the text it is, and a link whose
//! address could run script (`javascript:` and the like) loses its address.
//! An image becomes a link to it, since the public pages load no picture
//! from elsewhere. Headings start at `<h2>`, below the page's own.

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, Options};

fn options() -> Options<'static> {
    let mut o = Options::default();
    o.extension.strikethrough = true;
    o.extension.table = true;
    o.extension.tasklist = true;
    o.extension.autolink = true;
    // A line break in the box is one on the page, as people expect of a caption.
    o.render.hardbreaks = true;
    o.render.escape = true;
    o
}

/// The text as HTML, safe to put in a page as it is.
pub fn html(text: &str) -> askama_datastar::Html {
    let arena = Arena::new();
    let o = options();
    let root = comrak::parse_document(&arena, text, &o);
    let nodes: Vec<_> = root.descendants().collect();
    for node in nodes {
        let link = match &mut node.data_mut().value {
            NodeValue::Image(link) => link.clone(),
            NodeValue::Heading(h) => {
                h.level = (h.level + 1).min(6);
                continue;
            }
            _ => continue,
        };
        let in_link = node
            .ancestors()
            .skip(1)
            .any(|a| matches!(a.data().value, NodeValue::Link(_)));
        if in_link {
            // A linked image (a badge, a thumbnail) is its words, inside the
            // link it was in, since a link cannot hold another.
            let words: Vec<_> = node.children().collect();
            for word in words {
                node.insert_before(word);
            }
            node.detach();
            continue;
        }
        if node.first_child().is_none() {
            node.append(arena.alloc(NodeValue::Text(link.url.clone().into()).into()));
        }
        node.data_mut().value = NodeValue::Link(link);
    }
    let mut out = String::new();
    comrak::format_html(root, &o, &mut out).expect("writing to a String");
    askama_datastar::Html::trusted(out)
}

/// The words of the text without its Markdown, one block to a line: for alt
/// text, link previews and the media controls' title.
pub fn plain(text: &str) -> String {
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, text, &options());
    let mut out = String::new();
    collect(root, &mut out);
    out.trim().to_string()
}

/// The first line of [`plain`]'s words that has any: what names a picture
/// where one line is wanted, as alt text, a video's title in the media
/// controls, and a link preview's.
pub fn line(text: &str) -> String {
    plain(text)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn collect<'a>(node: &'a AstNode<'a>, out: &mut String) {
    let data = node.data();
    let value = &data.value;
    let next_cell = matches!(value, NodeValue::TableCell) && node.previous_sibling().is_some();
    if value.block() && !next_cell && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    match value {
        NodeValue::Text(t) => out.push_str(t),
        NodeValue::Code(c) => out.push_str(&c.literal),
        NodeValue::CodeBlock(c) => out.push_str(c.literal.trim_end()),
        NodeValue::HtmlInline(h) => out.push_str(h),
        NodeValue::HtmlBlock(h) => out.push_str(h.literal.trim_end()),
        NodeValue::SoftBreak | NodeValue::LineBreak => out.push('\n'),
        NodeValue::TableCell if next_cell => out.push(' '),
        _ => {}
    }
    for child in node.children() {
        collect(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(text: &str) -> String {
        super::html(text).as_str().to_owned()
    }

    #[test]
    fn renders_markdown() {
        assert_eq!(
            html("**hi** _there_\nnext ~~line~~"),
            "<p><strong>hi</strong> <em>there</em><br />\nnext <del>line</del></p>\n"
        );
        assert!(html("- [x] done").contains("checked"));
        assert!(html("see https://example.com").contains(r#"<a href="https://example.com">"#));
    }

    #[test]
    fn keeps_markup_out() {
        assert_eq!(
            html("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;\n"
        );
        assert_eq!(
            html("<b onclick=x>hi</b>"),
            "<p>&lt;b onclick=x&gt;hi&lt;/b&gt;</p>\n"
        );
        assert!(!html("[x](javascript:alert(1))").contains("javascript"));
        assert!(
            !html("[x](JaVaScRiPt:alert(1))")
                .to_lowercase()
                .contains("javascript")
        );
    }

    #[test]
    fn images_become_links() {
        assert_eq!(
            html("![a cat](https://example.com/cat.png)"),
            "<p><a href=\"https://example.com/cat.png\">a cat</a></p>\n"
        );
    }

    #[test]
    fn linked_and_unnamed_images() {
        assert_eq!(
            html("[![logo](https://x/logo.png)](https://example.com)"),
            "<p><a href=\"https://example.com\">logo</a></p>\n"
        );
        assert_eq!(
            html("![](https://x/cat.png)"),
            "<p><a href=\"https://x/cat.png\">https://x/cat.png</a></p>\n"
        );
    }

    #[test]
    fn headings_sit_below_the_page() {
        assert_eq!(html("# one"), "<h2>one</h2>\n");
        assert_eq!(html("###### six"), "<h6>six</h6>\n");
    }

    #[test]
    fn plain_text() {
        assert_eq!(
            plain("# A **trip**\n\nto `the` [sea](https://x)"),
            "A trip\nto the sea"
        );
        assert_eq!(plain("one\ntwo"), "one\ntwo");
        assert_eq!(plain("- a\n- b"), "a\nb");
        assert_eq!(plain("| a | b |\n|---|---|\n| 1 | 2 |"), "a b\n1 2");
        assert_eq!(plain("<b>x</b>"), "<b>x</b>");
        assert_eq!(plain("   "), "");
        assert_eq!(line("\n\n  **first**  \nsecond"), "first");
    }
}
