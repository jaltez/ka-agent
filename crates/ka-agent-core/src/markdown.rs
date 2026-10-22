//! Model Markdown → each surface's markup, from one parser so the terminal,
//! Telegram, Matrix and the web panel agree on what a reply looks like.
//!
//! Deliberately small: fenced code, `#` headings, bullet lists, `**bold**`
//! / `__bold__`, `` `code` ``, `[text](http(s)://…)` links. No italics
//! (single `*`/`_` are ambiguous in model prose). Unbalanced markers stay
//! literal. Every target escapes model text first, so only markup this
//! module emits reaches the surface — including the terminal, where ESC
//! and other control characters from the model are dropped.

/// Where the markup goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Telegram `parse_mode: HTML` subset (`<b>`, `<code>`, `<pre>`, `<a>`).
    Telegram,
    /// HTML for the web panel and Matrix `formatted_body`.
    Html,
    /// ANSI SGR for a terminal.
    Ansi,
}

/// Render a whole reply.
pub fn render(md: &str, target: Target) -> String {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut list_open = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.trim_start().strip_prefix("```")
            && let Some(close) = (i + 1..lines.len()).find(|&j| lines[j].trim() == "```")
        {
            close_list(&mut out, &mut list_open, target);
            out.push(code_block(fence_lang(rest), &lines[i + 1..close], target));
            i = close + 1;
            continue;
        }
        let bullet = bullet_text(line);
        if target == Target::Html {
            match (bullet, list_open) {
                (Some(_), false) => {
                    out.push("<ul>".into());
                    list_open = true;
                }
                (None, true) => close_list(&mut out, &mut list_open, target),
                _ => {}
            }
        }
        out.push(render_line(line, target));
        i += 1;
    }
    close_list(&mut out, &mut list_open, target);
    match target {
        // Block elements carry their own breaks; plain lines need <br>.
        Target::Html => join_html(&out),
        _ => out.join("\n"),
    }
}

/// One complete line outside a code fence (streaming terminals render line
/// by line; fences are tracked by the caller with [`fence_lang`]).
pub fn render_line(line: &str, target: Target) -> String {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes)
        && let Some(rest) = line[hashes..].strip_prefix(' ')
    {
        let body = inline(rest, false, target);
        return match target {
            Target::Telegram => format!("<b>{body}</b>"),
            Target::Html => format!("<h4>{body}</h4>"),
            Target::Ansi => format!("\x1b[1;4m{body}\x1b[22;24m"),
        };
    }
    if let Some(item) = bullet_text(line) {
        let indent = &line[..line.len() - line.trim_start().len()];
        let body = inline(item, true, target);
        return match target {
            Target::Html => format!("<li>{body}</li>"),
            Target::Telegram => format!("{}• {body}", escape(indent, target)),
            Target::Ansi => format!("{indent}• {body}"),
        };
    }
    inline(line, true, target)
}

/// The language of an opening fence's remainder (`rust` in "```rust"),
/// when it is a plain identifier; anything else is dropped.
pub fn fence_lang(rest: &str) -> Option<&str> {
    let lang = rest.trim();
    let ok = !lang.is_empty()
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.'));
    ok.then_some(lang)
}

/// One line inside a fenced block, for streaming terminals.
pub fn render_code_line(line: &str, target: Target) -> String {
    match target {
        Target::Ansi => format!("\x1b[36m  {}\x1b[39m", escape(line, target)),
        _ => escape(line, target),
    }
}

/// The header a streaming terminal prints for an opening fence.
pub fn render_fence_open(lang: Option<&str>) -> String {
    format!("\x1b[2m── {} ──\x1b[22m", lang.unwrap_or("code"))
}

fn code_block(lang: Option<&str>, body: &[&str], target: Target) -> String {
    match target {
        Target::Telegram | Target::Html => {
            let body = escape(&body.join("\n"), target);
            match lang {
                Some(l) => format!("<pre><code class=\"language-{l}\">{body}</code></pre>"),
                None => format!("<pre><code>{body}</code></pre>"),
            }
        }
        Target::Ansi => {
            let mut out = render_fence_open(lang);
            for l in body {
                out.push('\n');
                out.push_str(&render_code_line(l, target));
            }
            out
        }
    }
}

fn bullet_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
}

fn close_list(out: &mut Vec<String>, open: &mut bool, target: Target) {
    if *open && target == Target::Html {
        out.push("</ul>".into());
        *open = false;
    }
}

fn join_html(blocks: &[String]) -> String {
    let mut out = String::new();
    let mut prev_block = true;
    for b in blocks {
        let is_block = b.starts_with("<ul>")
            || b.starts_with("</ul>")
            || b.starts_with("<li>")
            || b.starts_with("<pre>")
            || b.starts_with("<h4>");
        if !out.is_empty() && !prev_block && !is_block {
            out.push_str("<br>");
        }
        out.push_str(b);
        prev_block = is_block;
    }
    out
}

/// Escape model text for the target. The terminal "escape" drops control
/// characters (ESC, BEL, …) so a reply can never drive the terminal.
pub fn escape(s: &str, target: Target) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match target {
            Target::Telegram | Target::Html => match c {
                '&' => o.push_str("&amp;"),
                '<' => o.push_str("&lt;"),
                '>' => o.push_str("&gt;"),
                '"' if target == Target::Html => o.push_str("&quot;"),
                _ => o.push(c),
            },
            Target::Ansi => {
                if !c.is_control() || c == '\n' || c == '\t' {
                    o.push(c);
                }
            }
        }
    }
    o
}

/// Inline code spans first; markup only outside them.
fn inline(s: &str, bold: bool, target: Target) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) if end > 0 => {
                out.push_str(&spans(&rest[..start], bold, target));
                let code = escape(&after[..end], target);
                match target {
                    Target::Ansi => out.push_str(&format!("\x1b[36m{code}\x1b[39m")),
                    _ => out.push_str(&format!("<code>{code}</code>")),
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push_str(&spans(&rest[..start + 1], bold, target));
                rest = after;
            }
        }
    }
    out.push_str(&spans(rest, bold, target));
    out
}

/// Links and bold within a code-free segment.
fn spans(s: &str, bold: bool, target: Target) -> String {
    let mut out = String::new();
    let mut i = 0;
    'scan: while i < s.len() {
        let tail = &s[i..];
        if let Some((text, url, used)) = link(tail) {
            out.push_str(&render_link(text, url, target));
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
                    let inner = spans(&after[..end], false, target);
                    match target {
                        Target::Telegram => out.push_str(&format!("<b>{inner}</b>")),
                        Target::Html => out.push_str(&format!("<strong>{inner}</strong>")),
                        Target::Ansi => out.push_str(&format!("\x1b[1m{inner}\x1b[22m")),
                    }
                    i += 2 * m.len() + end;
                    continue 'scan;
                }
            }
        }
        let c = tail.chars().next().unwrap_or(' ');
        let mut buf = [0u8; 4];
        out.push_str(&escape(c.encode_utf8(&mut buf), target));
        i += c.len_utf8();
    }
    out
}

fn render_link(text: &str, url: &str, target: Target) -> String {
    let text_out = escape(text, target);
    match target {
        Target::Telegram => {
            let href = escape(url, target).replace('"', "&quot;");
            format!("<a href=\"{href}\">{text_out}</a>")
        }
        Target::Html => {
            let href = escape(url, target);
            format!(
                "<a href=\"{href}\" rel=\"noopener noreferrer\" target=\"_blank\">{text_out}</a>"
            )
        }
        Target::Ansi => {
            let url_out = escape(url, target);
            if text == url {
                format!("\x1b[4m{url_out}\x1b[24m")
            } else {
                format!("{text_out} \x1b[2m({url_out})\x1b[22m")
            }
        }
    }
}

/// `[text](http(s)://url)` at the start of `s`: (text, url, bytes used).
fn link(s: &str) -> Option<(&str, &str, usize)> {
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
    Some((text, url, 1 + close + 2 + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(md: &str) -> String {
        render(md, Target::Telegram)
    }

    #[test]
    fn telegram_escapes_everything() {
        assert_eq!(
            h("<script>alert(1)</script> & co"),
            "&lt;script&gt;alert(1)&lt;/script&gt; &amp; co"
        );
    }

    #[test]
    fn telegram_fences() {
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
    fn telegram_inline_code() {
        assert_eq!(h("run `a<b` now"), "run <code>a&lt;b</code> now");
        assert_eq!(h("`**x**`"), "<code>**x**</code>");
        assert_eq!(h("a ` b"), "a ` b");
    }

    #[test]
    fn telegram_bold() {
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
    fn telegram_headings_and_bullets() {
        assert_eq!(h("## Title **x**"), "<b>Title **x**</b>");
        assert_eq!(h("#nospace"), "#nospace");
        assert_eq!(h("####### seven"), "####### seven");
        assert_eq!(h("- one\n  * **two**"), "• one\n  • <b>two</b>");
    }

    #[test]
    fn telegram_links() {
        assert_eq!(
            h("[site](https://a.b/?q=1&r=\"x\")"),
            "<a href=\"https://a.b/?q=1&amp;r=&quot;x&quot;\">site</a>"
        );
        assert_eq!(h("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert_eq!(h("[x](http://a"), "[x](http://a");
    }

    #[test]
    fn html_wraps_lists_and_breaks_lines() {
        assert_eq!(
            render("Hi **there**\n- a\n- b\nbye <x>", Target::Html),
            "Hi <strong>there</strong><ul><li>a</li><li>b</li></ul>bye &lt;x&gt;"
        );
        assert_eq!(render("a\nb", Target::Html), "a<br>b");
        assert_eq!(
            render("[x](https://e.x/\"y)", Target::Html),
            "<a href=\"https://e.x/&quot;y\" rel=\"noopener noreferrer\" target=\"_blank\">x</a>"
        );
    }

    #[test]
    fn ansi_styles_and_drops_control_characters() {
        assert_eq!(
            render("**b** `c`", Target::Ansi),
            "\x1b[1mb\x1b[22m \x1b[36mc\x1b[39m"
        );
        assert_eq!(render("- x", Target::Ansi), "• x");
        // A model cannot smuggle terminal escapes through.
        assert_eq!(render("a\x1b]0;pwned\x07b", Target::Ansi), "a]0;pwnedb");
        assert_eq!(
            render("```sh\nls\n```", Target::Ansi),
            "\x1b[2m── sh ──\x1b[22m\n\x1b[36m  ls\x1b[39m"
        );
        assert_eq!(
            render("[docs](https://d.x)", Target::Ansi),
            "docs \x1b[2m(https://d.x)\x1b[22m"
        );
    }
}
