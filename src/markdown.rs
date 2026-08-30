//! Small terminal Markdown renderer for model-authored prose.
//!
//! It deliberately covers the structures that improve coding-agent answers
//! without becoming a browser: headings, lists, quotes, inline emphasis/code,
//! links, and fenced code. Known code fences get a lightweight lexical pass;
//! unknown languages remain plain terminal text.

use crate::theme;
use ratatui::style::{Modifier, Style};

pub(crate) type StyledSegment = (String, Style);

#[derive(Debug)]
pub(crate) enum MarkdownLine {
    Blank,
    Prose {
        prefix: Vec<StyledSegment>,
        content: Vec<StyledSegment>,
    },
    Code(Vec<StyledSegment>),
}

#[derive(Clone, Copy)]
enum Language {
    Plain,
    Rust,
    Python,
    JavaScript,
    Go,
    Shell,
    Json,
    Toml,
    Yaml,
}

struct Fence {
    marker: char,
    len: usize,
    language: Language,
}

pub(crate) fn parse(text: &str) -> Vec<MarkdownLine> {
    let mut out = Vec::new();
    let mut paragraph = String::new();
    let mut fence: Option<Fence> = None;

    for line in text.lines() {
        if let Some(open) = &fence {
            if closes_fence(line, open) {
                fence = None;
            } else {
                out.push(MarkdownLine::Code(highlight_code(line, open.language)));
            }
            continue;
        }

        if let Some(open) = opens_fence(line) {
            flush_paragraph(&mut out, &mut paragraph);
            fence = Some(open);
            continue;
        }

        if line.trim().is_empty() {
            flush_paragraph(&mut out, &mut paragraph);
            push_blank(&mut out);
            continue;
        }

        if let Some(heading) = heading(line) {
            flush_paragraph(&mut out, &mut paragraph);
            out.push(MarkdownLine::Prose {
                prefix: Vec::new(),
                content: parse_inline(heading, theme::normal().add_modifier(Modifier::BOLD)),
            });
            continue;
        }

        if let Some((prefix, item)) = list_item(line) {
            flush_paragraph(&mut out, &mut paragraph);
            out.push(MarkdownLine::Prose {
                prefix: vec![(prefix, theme::normal())],
                content: parse_inline(item, theme::normal()),
            });
            continue;
        }

        if let Some((prefix, quote)) = block_quote(line) {
            flush_paragraph(&mut out, &mut paragraph);
            out.push(MarkdownLine::Prose {
                prefix: vec![(prefix, theme::dim())],
                content: parse_inline(quote, theme::dim().add_modifier(Modifier::ITALIC)),
            });
            continue;
        }

        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(line.trim());
    }

    flush_paragraph(&mut out, &mut paragraph);
    while matches!(out.last(), Some(MarkdownLine::Blank)) {
        out.pop();
    }
    out
}

fn flush_paragraph(out: &mut Vec<MarkdownLine>, paragraph: &mut String) {
    if paragraph.is_empty() {
        return;
    }
    out.push(MarkdownLine::Prose {
        prefix: Vec::new(),
        content: parse_inline(paragraph, theme::normal()),
    });
    paragraph.clear();
}

fn push_blank(out: &mut Vec<MarkdownLine>) {
    if !out.is_empty() && !matches!(out.last(), Some(MarkdownLine::Blank)) {
        out.push(MarkdownLine::Blank);
    }
}

fn heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) || trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    Some(trimmed[level + 1..].trim_end())
}

fn list_item(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    for marker in ["- ", "* ", "+ "] {
        if let Some(item) = trimmed.strip_prefix(marker) {
            return Some((format!("{indent}- "), item));
        }
    }

    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    let marker_len = if rest.starts_with(". ") || rest.starts_with(") ") {
        2
    } else {
        return None;
    };
    Some((
        format!("{indent}{}", &trimmed[..digits + marker_len]),
        &trimmed[digits + marker_len..],
    ))
}

fn block_quote(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    trimmed
        .strip_prefix("> ")
        .map(|quote| (format!("{indent}> "), quote))
}

fn opens_fence(line: &str) -> Option<Fence> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let len = trimmed.chars().take_while(|c| *c == marker).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].trim();
    let label = info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c| matches!(c, '{' | '}' | '.'));
    Some(Fence {
        marker,
        len,
        language: language(label),
    })
}

fn closes_fence(line: &str, fence: &Fence) -> bool {
    let trimmed = line.trim_start();
    let count = trimmed.chars().take_while(|c| *c == fence.marker).count();
    count >= fence.len && trimmed[count..].trim().is_empty()
}

fn language(label: &str) -> Language {
    match label.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "python" | "py" => Language::Python,
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" => Language::JavaScript,
        "go" | "golang" => Language::Go,
        "sh" | "shell" | "bash" | "zsh" | "fish" => Language::Shell,
        "json" | "jsonc" => Language::Json,
        "toml" => Language::Toml,
        "yaml" | "yml" => Language::Yaml,
        _ => Language::Plain,
    }
}

fn parse_inline(text: &str, base: Style) -> Vec<StyledSegment> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];

        if let Some(escaped) = rest.strip_prefix('\\') {
            if let Some(ch) = escaped.chars().next() {
                push_segment(&mut out, ch.to_string(), base);
                pos += 1 + ch.len_utf8();
                continue;
            }
        }

        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(end) = after_tick.find('`') {
                push_segment(&mut out, after_tick[..end].to_owned(), theme::blue());
                pos += end + 2;
                continue;
            }
        }

        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                extend_segments(
                    &mut out,
                    parse_inline(&after[..end], base.add_modifier(Modifier::BOLD)),
                );
                pos += end + 4;
                continue;
            }
        }

        if let Some(after) = rest.strip_prefix("~~") {
            if let Some(end) = after.find("~~") {
                extend_segments(
                    &mut out,
                    parse_inline(&after[..end], base.add_modifier(Modifier::CROSSED_OUT)),
                );
                pos += end + 4;
                continue;
            }
        }

        if let Some(after) = rest.strip_prefix('*') {
            if let Some(end) = after.find('*') {
                if end > 0 {
                    extend_segments(
                        &mut out,
                        parse_inline(&after[..end], base.add_modifier(Modifier::ITALIC)),
                    );
                    pos += end + 2;
                    continue;
                }
            }
        }

        if let Some(after_open) = rest.strip_prefix('[') {
            if let Some(label_end) = after_open.find("](") {
                let after_label = &after_open[label_end + 2..];
                if let Some(url_end) = after_label.find(')') {
                    let label = &after_open[..label_end];
                    let url = &after_label[..url_end];
                    extend_segments(
                        &mut out,
                        parse_inline(label, theme::blue().add_modifier(Modifier::UNDERLINED)),
                    );
                    if label != url && !url.is_empty() {
                        push_segment(&mut out, format!(" ({url})"), theme::dim());
                    }
                    pos += label_end + url_end + 4;
                    continue;
                }
            }
        }

        let ch = rest.chars().next().unwrap();
        push_segment(&mut out, ch.to_string(), base);
        pos += ch.len_utf8();
    }
    out
}

fn highlight_code(line: &str, language: Language) -> Vec<StyledSegment> {
    if matches!(language, Language::Plain) {
        return vec![(line.to_owned(), theme::normal())];
    }

    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if is_comment_start(&chars, i, language) {
            let comment: String = chars[i..].iter().collect();
            push_segment(&mut out, comment, theme::dim());
            i = chars.len();
            continue;
        }

        let ch = chars[i];
        if matches!(ch, '"' | '\'' | '`') {
            if let Some(end) = quoted_end(&chars, i, ch) {
                let value: String = chars[i..=end].iter().collect();
                push_segment(&mut out, value, theme::green());
                i = end + 1;
                continue;
            }
        }

        if ch.is_ascii_digit() && (i == 0 || !is_ident(chars[i.saturating_sub(1)])) {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_' | '+' | '-'))
            {
                i += 1;
            }
            push_segment(&mut out, chars[start..i].iter().collect(), theme::amber());
            continue;
        }

        if is_ident(ch) {
            let start = i;
            i += 1;
            while i < chars.len() && is_ident(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let style = if is_keyword(&word, language) {
                theme::blue().add_modifier(Modifier::BOLD)
            } else {
                theme::normal()
            };
            push_segment(&mut out, word, style);
            continue;
        }

        push_segment(&mut out, ch.to_string(), theme::normal());
        i += 1;
    }
    out
}

fn is_comment_start(chars: &[char], i: usize, language: Language) -> bool {
    match language {
        Language::Rust | Language::JavaScript | Language::Go
            if chars.get(i) == Some(&'/') && chars.get(i + 1) == Some(&'/') =>
        {
            true
        }
        Language::Python | Language::Shell | Language::Toml | Language::Yaml
            if chars.get(i) == Some(&'#') =>
        {
            true
        }
        _ => false,
    }
}

fn quoted_end(chars: &[char], start: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (i, ch) in chars.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if *ch == '\\' {
            escaped = true;
        } else if *ch == quote {
            return Some(i);
        }
    }
    None
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_keyword(word: &str, language: Language) -> bool {
    match language {
        Language::Rust => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        Language::Python => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "False"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "None"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "True"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        Language::JavaScript => matches!(
            word,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "new"
                | "null"
                | "of"
                | "private"
                | "protected"
                | "public"
                | "readonly"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "type"
                | "typeof"
                | "undefined"
                | "var"
                | "void"
                | "while"
                | "yield"
        ),
        Language::Go => matches!(
            word,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "fallthrough"
                | "false"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "nil"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "true"
                | "type"
                | "var"
        ),
        Language::Shell => matches!(
            word,
            "case"
                | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "select"
                | "then"
                | "time"
                | "until"
                | "while"
        ),
        Language::Json | Language::Toml | Language::Yaml => {
            matches!(word, "true" | "false" | "null")
        }
        Language::Plain => false,
    }
}

fn push_segment(out: &mut Vec<StyledSegment>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((last, previous)) if *previous == style => last.push_str(&text),
        _ => out.push((text, style)),
    }
}

fn extend_segments(out: &mut Vec<StyledSegment>, segments: Vec<StyledSegment>) {
    for (text, style) in segments {
        push_segment(out, text, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn parses_blocks_and_inline_styles() {
        let lines = parse("# Result\n\n- run `cargo test` and **inspect** output\n\n> safe quote");
        assert!(matches!(lines[0], MarkdownLine::Prose { .. }));
        let MarkdownLine::Prose { content, .. } = &lines[0] else {
            panic!("heading")
        };
        assert!(content[0].1.add_modifier.contains(Modifier::BOLD));

        let MarkdownLine::Prose { prefix, content } = &lines[2] else {
            panic!("list")
        };
        assert_eq!(prefix[0].0, "- ");
        assert!(content
            .iter()
            .any(|(text, style)| { text == "cargo test" && style.fg == Some(Color::Blue) }));
        assert!(content.iter().any(|(text, style)| {
            text == "inspect" && style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn highlights_known_fences_and_leaves_unknown_plain() {
        let known = parse("```rust\nfn main() { // hello\n    let n = 42;\n}\n```");
        let MarkdownLine::Code(first) = &known[0] else {
            panic!("code")
        };
        assert!(first
            .iter()
            .any(|(text, style)| text == "fn" && style.fg == Some(Color::Blue)));
        assert!(first
            .iter()
            .any(|(text, style)| text.contains("// hello")
                && style.add_modifier.contains(Modifier::DIM)));
        let MarkdownLine::Code(second) = &known[1] else {
            panic!("code")
        };
        assert!(second
            .iter()
            .any(|(text, style)| text == "42" && style.fg == Some(Color::Yellow)));

        let plain = parse("```unknown\nlet value = 1\n```");
        let MarkdownLine::Code(segments) = &plain[0] else {
            panic!("plain code")
        };
        assert_eq!(segments, &vec![("let value = 1".into(), theme::normal())]);
    }
}
