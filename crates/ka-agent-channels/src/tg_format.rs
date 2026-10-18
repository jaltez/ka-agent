//! Markdown → Telegram HTML (safe subset). Input is escaped first; only
//! tags this module emits itself can appear in the output. Unbalanced
//! markers stay literal text. No italics (single `*`/`_` are ambiguous).

/// Convert model markdown into Telegram's HTML `parse_mode` subset.
pub fn to_telegram_html(md: &str) -> String {
    let escaped = escape(md);
    let lines: Vec<&str> = escaped.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.trim_start().strip_prefix("```")
            && let Some(close) = (i + 1..lines.len()).find(|&j| lines[j].trim() == "```")
        {
            let lang = rest.trim();
            let body = lines[i + 1..close].join("\n");
            let valid_lang = !lang.is_empty()
                && lang
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.'));
            if valid_lang {
                out.push(format!(
                    "<pre><code class=\"language-{lang}\">{body}</code></pre>"
                ));
            } else {
                out.push(format!("<pre><code>{body}</code></pre>"));
            }
            i = close + 1;
            continue;
        }
        out.push(convert_line(line));
        i += 1;
    }
    out.join("\n")
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            _ => o.push(c),
        }
    }
    o
}

fn convert_line(line: &str) -> String {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes)
        && let Some(rest) = line[hashes..].strip_prefix(' ')
    {
        return format!("<b>{}</b>", inline(rest, false));
    }
    inline(line, true)
}

/// Inline code spans first; markup only outside them.
fn inline(s: &str, bold: bool) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) if end > 0 => {
                out.push_str(&spans(&rest[..start], bold));
                out.push_str("<code>");
                out.push_str(&after[..end]);
                out.push_str("</code>");
                rest = &after[end + 1..];
            }
            _ => {
                out.push_str(&spans(&rest[..start + 1], bold));
                rest = after;
            }
        }
    }
    out.push_str(&spans(rest, bold));
    out
}

/// Links and bold within a code-free segment.
fn spans(s: &str, bold: bool) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let tail = &s[i..];
        if let Some((html, used)) = link(tail) {
            out.push_str(&html);
            i += used;
            continue;
        }
        if bold {
            for m in ["**", "__"] {
                if let Some(after) = tail.strip_prefix(m)
                    && let Some(end) = after.find(m)
                    && end > 0
                    && !after[..end].trim().is_empty()
                {
                    out.push_str("<b>");
                    out.push_str(&spans(&after[..end], false));
                    out.push_str("</b>");
                    i += 2 * m.len() + end;
                    break;
                }
            }
            if i > s.len() - tail.len() {
                continue;
            }
        }
        let c = tail.chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn link(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix('[')?;
    let close = rest.find("](")?;
    let text = &rest[..close];
    if text.is_empty() || text.contains('[') {
        return None;
    }
    let url_part = &rest[close + 2..];
    let end = url_part.find(')')?;
    let url = &url_part[..end];
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.chars().any(char::is_whitespace)
    {
        return None;
    }
    let href = url.replace('"', "&quot;");
    let used = 1 + close + 2 + end + 1;
    Some((format!("<a href=\"{href}\">{text}</a>"), used))
}

#[cfg(test)]
mod tests {
    use super::to_telegram_html as h;

    #[test]
    fn escapes_everything() {
        assert_eq!(
            h("<script>alert(1)</script> & co"),
            "&lt;script&gt;alert(1)&lt;/script&gt; &amp; co"
        );
    }

    #[test]
    fn fences() {
        assert_eq!(
            h("```rust\nfn a(){}\n```"),
            "<pre><code class=\"language-rust\">fn a(){}</code></pre>"
        );
        assert_eq!(h("```\nx < y\n```"), "<pre><code>x &lt; y</code></pre>");
        assert_eq!(h("```\n**x**\n```"), "<pre><code>**x**</code></pre>");
        assert_eq!(h("```py\n**x**"), "```py\n<b>x</b>");
        assert_eq!(h("```\" onclick=x\nhi\n```"), "<pre><code>hi</code></pre>");
    }

    #[test]
    fn inline_code() {
        assert_eq!(h("run `a<b` now"), "run <code>a&lt;b</code> now");
        assert_eq!(h("`**x**`"), "<code>**x**</code>");
        assert_eq!(h("a ` b"), "a ` b");
    }

    #[test]
    fn bold() {
        assert_eq!(h("**hi** and __yo__"), "<b>hi</b> and <b>yo</b>");
        assert_eq!(h("**open"), "**open");
        assert_eq!(h("**a\nb**"), "**a\nb**");
        assert_eq!(h("*x* _y_"), "*x* _y_");
        assert_eq!(
            h("**a [l](https://e.x) b**"),
            "<b>a <a href=\"https://e.x\">l</a> b</b>"
        );
    }

    #[test]
    fn headings() {
        assert_eq!(h("## Title **x**"), "<b>Title **x**</b>");
        assert_eq!(h("#nospace"), "#nospace");
        assert_eq!(h("####### seven"), "####### seven");
    }

    #[test]
    fn links() {
        assert_eq!(
            h("[site](https://a.b/?q=1&r=\"x\")"),
            "<a href=\"https://a.b/?q=1&amp;r=&quot;x&quot;\">site</a>"
        );
        assert_eq!(h("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert_eq!(h("[x](http://a"), "[x](http://a");
    }

    #[test]
    fn mixed() {
        assert_eq!(
            h("**hi** `x`\n```rust\nfn a(){}\n```"),
            "<b>hi</b> <code>x</code>\n<pre><code class=\"language-rust\">fn a(){}</code></pre>"
        );
    }
}
