//! A safe Markdown subset for texts that come from data (catalogue descriptions): paragraphs,
//! `#` headings, `-`/`*` bullet lists, `` `code` ``, `**bold**` and `[text](https://…)` links.
//! Everything else stays literal text. Only text nodes and fixed elements are built (no
//! `inner_html`, G1), so no markup in the source can reach the page.

use leptos::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(String),
    Para(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    Text(String),
    Code(String),
    Bold(String),
    /// Text and an `https://` address (other schemes stay text).
    Link(String, String),
}

/// Blocks: a blank line ends a paragraph or list; lines of a paragraph are joined.
pub fn blocks(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut list: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<&str>, list: &mut Vec<String>, out: &mut Vec<Block>| {
        if !para.is_empty() {
            out.push(Block::Para(para.join(" ")));
            para.clear();
        }
        if !list.is_empty() {
            out.push(Block::List(std::mem::take(list)));
        }
    };
    for line in src.lines() {
        let l = line.trim();
        if l.is_empty() {
            flush(&mut para, &mut list, &mut out);
        } else if let Some(h) = l.strip_prefix('#') {
            flush(&mut para, &mut list, &mut out);
            out.push(Block::Heading(h.trim_start_matches('#').trim().to_string()));
        } else if let Some(item) = l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")) {
            if !para.is_empty() {
                out.push(Block::Para(para.join(" ")));
                para.clear();
            }
            list.push(item.to_string());
        } else if !list.is_empty() && line.starts_with(' ') {
            // continuation of the last item
            if let Some(last) = list.last_mut() {
                last.push(' ');
                last.push_str(l);
            }
        } else {
            if !list.is_empty() {
                out.push(Block::List(std::mem::take(&mut list)));
            }
            para.push(l);
        }
    }
    flush(&mut para, &mut list, &mut out);
    out
}

/// Inline spans of one block's text.
pub fn spans(s: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    let push_text = |text: &mut String, out: &mut Vec<Span>| {
        if !text.is_empty() {
            out.push(Span::Text(std::mem::take(text)));
        }
    };
    while let Some(c) = rest.chars().next() {
        if c == '`'
            && let Some(end) = rest[1..].find('`')
        {
            push_text(&mut text, &mut out);
            out.push(Span::Code(rest[1..1 + end].to_string()));
            rest = &rest[end + 2..];
            continue;
        }
        if rest.starts_with("**")
            && let Some(end) = rest[2..].find("**")
            && end > 0
        {
            push_text(&mut text, &mut out);
            out.push(Span::Bold(rest[2..2 + end].to_string()));
            rest = &rest[end + 4..];
            continue;
        }
        if c == '['
            && let Some(close) = rest.find("](")
            && let Some(end) = rest[close + 2..].find(')')
        {
            let label = &rest[1..close];
            let url = &rest[close + 2..close + 2 + end];
            push_text(&mut text, &mut out);
            if url.starts_with("https://") && !url.contains(char::is_whitespace) {
                out.push(Span::Link(label.to_string(), url.to_string()));
            } else {
                out.push(Span::Text(label.to_string()));
            }
            rest = &rest[close + 3 + end..];
            continue;
        }
        text.push(c);
        rest = &rest[c.len_utf8()..];
    }
    push_text(&mut text, &mut out);
    out
}

fn inline(s: &str) -> AnyView {
    spans(s)
        .into_iter()
        .map(|sp| match sp {
            Span::Text(t) => t.into_any(),
            Span::Code(c) => view! { <code>{c}</code> }.into_any(),
            Span::Bold(b) => view! { <strong>{b}</strong> }.into_any(),
            Span::Link(t, u) => view! { <a href=u target="_blank" rel="noopener noreferrer">{t}</a> }.into_any(),
        })
        .collect_view()
        .into_any()
}

/// The text as a view (`lang` marks the language of the source).
pub fn render(src: &str, lang: &'static str) -> AnyView {
    let body = blocks(src)
        .into_iter()
        .map(|b| match b {
            Block::Heading(h) => view! { <h4 class="md-h">{inline(&h)}</h4> }.into_any(),
            Block::Para(p) => view! { <p>{inline(&p)}</p> }.into_any(),
            Block::List(items) => view! { <ul>{items.iter().map(|i| view! { <li>{inline(i)}</li> }).collect_view()}</ul> }.into_any(),
        })
        .collect_view();
    view! { <div class="md" lang=lang>{body}</div> }.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_spans() {
        let b = blocks("# Title\nOne line\nand more.\n\n- a\n- b `x`\n  cont\nAfter <b>html</b>");
        assert_eq!(
            b,
            vec![
                Block::Heading("Title".into()),
                Block::Para("One line and more.".into()),
                Block::List(vec!["a".into(), "b `x` cont".into()]),
                Block::Para("After <b>html</b>".into()),
            ]
        );
        assert_eq!(
            spans("set `A` **now** [doc](https://x.org) [bad](javascript:alert(1)) `open"),
            vec![
                Span::Text("set ".into()),
                Span::Code("A".into()),
                Span::Text(" ".into()),
                Span::Bold("now".into()),
                Span::Text(" ".into()),
                Span::Link("doc".into(), "https://x.org".into()),
                Span::Text(" ".into()),
                Span::Text("bad".into()),
                Span::Text(") `open".into()),
            ]
        );
    }
}
