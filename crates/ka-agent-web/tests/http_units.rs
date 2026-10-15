//! Unit tests for the web crate's pure helpers: escaping and URL decoding.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ka_agent_web::{escape_html, url_decode};

#[test]
fn escape_html_neutralizes_every_dangerous_char() {
    assert_eq!(
        escape_html("<script>alert('x')</script>"),
        "&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;"
    );
    assert_eq!(
        escape_html(r#"<img src=x onerror="a">"#),
        "&lt;img src=x onerror=&quot;a&quot;&gt;"
    );
    // `&` is escaped first, so entity output is never double-escaped back.
    assert_eq!(escape_html("a & b"), "a &amp; b");
    assert_eq!(escape_html("&lt;"), "&amp;lt;");
    assert_eq!(escape_html(""), "");
    assert_eq!(escape_html("plain text"), "plain text");
}

#[test]
fn url_decode_handles_percent_plus_and_malformed_input() {
    assert_eq!(url_decode("a+b"), "a b");
    assert_eq!(url_decode("a%20b"), "a b");
    assert_eq!(url_decode("%3Cscript%3E"), "<script>");
    assert_eq!(url_decode("plain"), "plain");
    assert_eq!(url_decode(""), "");
    // Malformed escapes must never panic.
    assert_eq!(url_decode("%zz"), "");
    assert_eq!(url_decode("%"), "");
    assert_eq!(url_decode("%2"), "\u{2}");
    assert_eq!(url_decode("id=1&decision=y"), "id=1&decision=y");
}
