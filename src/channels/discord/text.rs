//! UTF-16-safe message splitting.
//! Discord counts message length in UTF-16 code units, not code points.
/// Count UTF-16 code units in `s` (surrogate pairs count as two).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Split `s` into chunks of at most `limit` UTF-16 units, never slicing a
/// character in half. Returns every character exactly once, in order.
pub fn split_message(s: &str, limit: usize) -> Result<Vec<String>, String> {
    if limit < 2 {
        return Err("Message limit must be at least two UTF-16 units".to_string());
    }
    let mut chunks = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let prefix = prefix_within_limit(rest, limit);
        debug_assert!(!prefix.is_empty(), "limit >= 2 always fits one char");
        chunks.push(prefix.to_string());
        rest = &rest[prefix.len()..];
    }
    Ok(chunks)
}

/// Longest prefix of `s` (on a char boundary) within `limit` UTF-16 units.
pub fn prefix_within_limit(s: &str, limit: usize) -> &str {
    if utf16_len(s) <= limit {
        return s;
    }
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if used + w > limit {
            break;
        }
        used += w;
        end = i + c.len_utf8();
    }
    // Elide the allocation the Python version needed for its binary search;
    // char indices give the same boundary directly. Guard against a
    // zero-width result only to keep the caller's loop total.
    &s[..end]
}

/// What model output
/// needs before Discord renders it. Three transforms, no regex crate.
pub fn sanitize(text: &str) -> String {
    text.split('\n')
        .map(sanitize_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_line(line: &str) -> String {
    let line = wrap_urls(line);
    let line = collapse_doubled_heading(&line);
    replace_emoji_names(&line)
}

/// A bare link becomes an embed card; inside `<>` it stays inline. Markdown
/// links survive untouched — the token inside `(...)` never starts a word.
fn wrap_urls(line: &str) -> String {
    if !line.contains("http") && !line.contains("www.") {
        return line.to_string();
    }
    line.split(' ')
        .map(|word| match strip_url(word) {
            Some((url, tail)) => format!("<{url}>{tail}"),
            None => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_url(word: &str) -> Option<(&str, &str)> {
    let scheme = ["https://", "http://", "www."]
        .into_iter()
        .find(|p| word.starts_with(p))?;
    if word.len() <= scheme.len() + 3 || word.starts_with('<') {
        return None;
    }
    // Trailing punctuation stays outside the bracket.
    let mut end = word.len();
    while end > scheme.len() + 3
        && matches!(word.as_bytes()[end - 1], b'.' | b',' | b'!' | b'?' | b')')
    {
        end -= 1;
    }
    Some((&word[..end], &word[end..]))
}

/// `#### ## Title` -> `## Title`.
fn collapse_doubled_heading(line: &str) -> String {
    let trimmed = line.trim_start();
    let first = trimmed.len() - trimmed.trim_start_matches('#').len();
    if first == 0 || first == trimmed.len() {
        return line.to_string();
    }
    let rest = &trimmed[first..];
    let ws = rest.len() - rest.trim_start().len();
    if ws == 0 {
        return line.to_string();
    }
    let body = rest.trim_start();
    let second = body.len() - body.trim_start_matches('#').len();
    if second == 0 || second == body.len() {
        return line.to_string();
    }
    format!("{}{}", "#".repeat(first), &body[second..])
}

/// `:fire:` -> 🔥. Same short table as the Python: the names a model
/// actually emits. `::` (Rust paths) and `12:30` times never match.
fn replace_emoji_names(text: &str) -> String {
    const TABLE: &[(&str, &str)] = &[
        ("shrug", "\u{1f937}"),
        ("think", "\u{1f914}"),
        ("thonk", "\u{1f914}"),
        ("huh", "\u{1f914}"),
        ("wave", "\u{1f44b}"),
        ("check", "\u{2705}"),
        ("yes", "\u{2705}"),
        ("done", "\u{2705}"),
        ("warning", "\u{26a0}\u{fe0f}"),
        ("alert", "\u{26a0}\u{fe0f}"),
        ("fire", "\u{1f525}"),
        ("party", "\u{1f389}"),
        ("sparkles", "\u{2728}"),
    ];
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b':' && bytes.get(i + 1) != Some(&b':') {
            if let Some(close) = text[i + 1..].find(':') {
                let name = &text[i + 1..i + 1 + close];
                if (2..=32).contains(&name.len())
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    if let Some((_, emoji)) = TABLE.iter().find(|(n, _)| *n == name) {
                        out.push_str(emoji);
                        i += 1 + close + 1;
                        continue;
                    }
                }
            }
        }
        let len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + len]);
        i += len;
    }
    out
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn bare_urls_stop_becoming_embeds() {
        assert_eq!(
            sanitize("see https://example.com/a?b=c now"),
            "see <https://example.com/a?b=c> now"
        );
    }

    #[test]
    fn trailing_punctuation_stays_outside_the_bracket() {
        assert_eq!(
            sanitize("go to https://example.com."),
            "go to <https://example.com>."
        );
    }

    #[test]
    fn already_wrapped_links_are_left_alone() {
        assert_eq!(sanitize("<https://example.com>"), "<https://example.com>");
    }

    #[test]
    fn markdown_links_survive() {
        let input = "[docs](https://example.com/x) here";
        assert_eq!(sanitize(input), input);
    }

    #[test]
    fn colons_in_paths_and_times_survive() {
        assert_eq!(sanitize("12:30 and a::b"), "12:30 and a::b");
    }

    #[test]
    fn emoji_names_become_unicode() {
        assert_eq!(sanitize("that is :fire: and :+1:"), "that is 🔥 and :+1:");
    }

    #[test]
    fn doubled_heading_hashes_collapse() {
        assert_eq!(sanitize("#### ## Title"), "#### Title");
        assert_eq!(sanitize("## Title"), "## Title");
    }

    #[test]
    fn plain_text_is_untouched() {
        let input = "just a normal answer\nwith two lines";
        assert_eq!(sanitize(input), input);
    }
}
