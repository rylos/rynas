//! Checks of `style/main.css` (guidelines G3, G4): balanced blocks, colours only as tokens, spacing
//! on the 4 px scale, font sizes from the type scale, no rule set twice.

const CSS: &str = include_str!("../style/main.css");

/// The CSS without comments.
fn css() -> String {
    let mut out = String::with_capacity(CSS.len());
    let mut rest = CSS;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        let j = rest[i..].find("*/").map(|j| i + j + 2).unwrap_or(rest.len());
        // keep the line count
        out.extend(rest[i..j].chars().filter(|c| *c == '\n'));
        rest = &rest[j..];
    }
    out.push_str(rest);
    out
}

/// (context, selector, body, line) of every rule; `@media`/`@container` bodies are walked.
fn rules(text: &str, ctx: &str, line0: usize, out: &mut Vec<(String, String, String, usize)>) {
    let mut i = 0;
    let b = text.as_bytes();
    while let Some(open) = text[i..].find('{').map(|o| i + o) {
        let sel = text[i..open].trim().to_string();
        let mut depth = 0;
        let mut close = open;
        for (k, c) in b.iter().enumerate().skip(open) {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = k;
                        break;
                    }
                }
                _ => {}
            }
        }
        if close == open {
            // unclosed block: blocks_are_balanced reports it
            return;
        }
        let line = line0 + text[..open].matches('\n').count();
        let body = &text[open + 1..close];
        if sel.starts_with("@media") || sel.starts_with("@container") || sel.starts_with("@supports") {
            rules(body, &sel, line0 + text[..open + 1].matches('\n').count(), out);
        } else {
            out.push((ctx.to_string(), sel, body.to_string(), line));
        }
        i = close + 1;
    }
}

fn all_rules() -> Vec<(String, String, String, usize)> {
    let mut v = Vec::new();
    rules(&css(), "", 1, &mut v);
    v
}

/// Declarations `(property, value)` of a rule body.
fn decls(body: &str) -> impl Iterator<Item = (&str, &str)> {
    body.split(';').filter_map(|d| d.split_once(':')).map(|(p, v)| (p.trim(), v.trim()))
}

/// Pixel values in a declaration value.
fn px_values(v: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let b = v.as_bytes();
    for (i, w) in v.match_indices("px") {
        let _ = w;
        let mut j = i;
        while j > 0 && b[j - 1].is_ascii_digit() {
            j -= 1;
        }
        let prev_ok = j == 0 || !(b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'.' || b[j - 1] == b'-');
        if j < i && prev_ok {
            out.push(v[j..i].parse().unwrap_or(0));
        }
    }
    out
}

#[test]
fn blocks_are_balanced() {
    let text = css();
    let mut depth = 0i32;
    for (n, line) in text.lines().enumerate() {
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    assert!(depth >= 0, "unmatched }} at line {}", n + 1);
                }
                _ => {}
            }
        }
    }
    assert_eq!(depth, 0, "unclosed block");
}

#[test]
fn colours_only_as_tokens() {
    // the token blocks: `:root` and `:root[data-theme="dark"]`
    let mut bad = Vec::new();
    for (_, sel, body, line) in all_rules() {
        if sel == ":root" || sel == ":root[data-theme=\"dark\"]" {
            continue;
        }
        for (p, v) in decls(&body) {
            let hex = v.char_indices().any(|(i, c)| c == '#' && v[i + 1..].chars().take(3).all(|h| h.is_ascii_hexdigit()));
            if !p.starts_with("--") && (hex || v.contains("rgb(") || v.contains("rgba(")) {
                bad.push(format!("line {line}: {sel} {p}: {v}"));
            }
        }
    }
    assert!(bad.is_empty(), "colour literals outside the tokens: {bad:#?}");
}

#[test]
fn spacing_and_type_scale() {
    const SCALE: [u32; 9] = [0, 1, 4, 8, 12, 16, 24, 32, 48];
    let mut bad = Vec::new();
    for (_, sel, body, line) in all_rules() {
        for (p, v) in decls(&body) {
            let spacing = p.starts_with("padding") || p.starts_with("margin") || p == "gap" || p.ends_with("-gap");
            if spacing && px_values(v).iter().any(|n| !SCALE.contains(n)) {
                bad.push(format!("line {line}: {sel} {p}: {v}"));
            }
            let font = p == "font-size" || p == "font";
            if font && !px_values(v).is_empty() {
                bad.push(format!("line {line}: {sel} {p}: {v}"));
            }
        }
    }
    assert!(bad.is_empty(), "off-scale spacing or font size: {bad:#?}");
}

#[test]
fn no_rule_set_twice() {
    let mut seen = std::collections::HashMap::new();
    let mut dups = Vec::new();
    for (ctx, sel, _, line) in all_rules() {
        if sel.starts_with("@keyframes") || sel.starts_with("@font-face") {
            let key = (String::new(), sel.clone());
            if sel.starts_with("@keyframes")
                && let Some(prev) = seen.insert(key, line)
            {
                dups.push(format!("{sel}: lines {prev} and {line}"));
            }
            continue;
        }
        let norm = sel.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(prev) = seen.insert((ctx.clone(), norm.clone()), line) {
            dups.push(format!("{ctx} {norm}: lines {prev} and {line}"));
        }
    }
    assert!(dups.is_empty(), "the same selector in two rules: {dups:#?}");
}

/// G7: nothing scrolls sideways. A rule that lets an element scroll horizontally
/// (`overflow-x: auto|scroll`, or `overflow: auto|scroll` for both axes) needs a reason here; the
/// browser walk of the review checks that none of them actually scrolls at 390–1280 px (the chassis
/// drawing had such a rule and scrolled inside its card).
#[test]
fn horizontal_scroll_containers_are_listed() {
    const ALLOWED: &[(&str, &str)] = &[
        (".table-wrap", "safety net only: tables wrap to fit their card"),
        (".task-list", "scrolls vertically; its table fits"),
        (".log", "log viewer, wide by nature (and pre-wrap)"),
        (".subnav", "strip of two short tab links, scrollbar hidden"),
        (".stepper", "wizard steps, the inactive ones shrink to their number"),
        (".code-area", "code editor (compose files): indentation matters, lines are not wrapped"),
    ];
    let mut bad = Vec::new();
    for (_, sel, body, line) in all_rules() {
        for (p, v) in decls(&body) {
            let x = match p {
                "overflow-x" => v,
                // `overflow: <x> <y>` or one value for both axes
                "overflow" => v.split_whitespace().next().unwrap_or(""),
                _ => continue,
            };
            if (x == "auto" || x == "scroll") && !ALLOWED.iter().any(|(s, _)| *s == sel) {
                bad.push(format!("line {line}: {sel} {p}: {v}"));
            }
        }
    }
    assert!(bad.is_empty(), "elements that may scroll sideways (G7), not listed: {bad:#?}");
}
