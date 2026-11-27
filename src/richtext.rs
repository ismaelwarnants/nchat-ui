//! Message text -> Slint StyledText (Markdown): clickable links, line breaks kept, and
//! WhatsApp-style formatting (*bold*, _italic_, ~strike~, `code`) shown as formatting.
//! Everything else is escaped, so message text can never inject markup.

/// URLs in the text (http(s)://… and www.…), trailing punctuation trimmed.
pub fn links(text: &str) -> Vec<String> {
    text.split_whitespace().filter_map(link_of).map(|(url, _)| url).collect()
}

/// (url to open, length of the token part that belongs to the link)
fn link_of(token: &str) -> Option<(String, usize)> {
    let start = if token.starts_with("https://") || token.starts_with("http://") || token.starts_with("www.") {
        0
    } else {
        return None;
    };
    let trimmed = token[start..].trim_end_matches(['.', ',', ')', '!', '?', ';', ':', '"', '\'', '*', '_', '~']);
    if trimmed.len() < 8 {
        return None;
    }
    let url = if trimmed.starts_with("www.") { format!("https://{trimmed}") } else { trimmed.to_string() };
    Some((url, trimmed.len()))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn escape_url(url: &str) -> String {
    url.replace(' ', "%20").replace('<', "%3C").replace('>', "%3E")
}

/// WhatsApp formatting markers that may open/close a span here.
fn marker_md(c: char) -> Option<&'static str> {
    match c {
        '*' => Some("**"),
        '_' => Some("*"),
        '~' => Some("~~"),
        '`' => Some("`"),
        _ => None,
    }
}

/// Converts one line (no newlines) to Markdown.
fn line_to_md(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // leading spaces would turn into a Markdown code block
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        out.push('\u{00a0}');
        i += 1;
    }
    while i < chars.len() {
        let at_word_start = i == 0 || chars[i - 1].is_whitespace() || chars[i - 1].is_ascii_punctuation();
        // links
        if at_word_start {
            let token: String = chars[i..].iter().take_while(|c| !c.is_whitespace()).collect();
            if let Some((url, len)) = link_of(&token) {
                let shown: String = token.chars().take(len).collect();
                out.push_str(&format!("[{}](<{}>)", escape(&shown), escape_url(&url)));
                i += shown.chars().count();
                continue;
            }
        }
        // *bold* _italic_ ~strike~ `code`: marker at word start, closing marker before a
        // word end, no spaces right inside the markers
        if let Some(md) = marker_md(chars[i]) {
            if at_word_start && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
                let m = chars[i];
                if let Some(rel) = chars[i + 1..].iter().position(|c| *c == m) {
                    let close = i + 1 + rel;
                    let inner: String = chars[i + 1..close].iter().collect();
                    let after_ok = close + 1 >= chars.len()
                        || chars[close + 1].is_whitespace()
                        || chars[close + 1].is_ascii_punctuation();
                    if !inner.is_empty() && !inner.ends_with(char::is_whitespace) && after_ok {
                        let body = if m == '`' { inner.replace('`', "") } else { line_to_md(&inner) };
                        out.push_str(md);
                        out.push_str(&body);
                        out.push_str(md);
                        i = close + 1;
                        continue;
                    }
                }
            }
        }
        let c = chars[i];
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
        i += 1;
    }
    out
}

pub fn to_markdown(text: &str) -> String {
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| {
            let md = line_to_md(l.trim_end_matches('\r'));
            // keep empty lines (Markdown would merge them away)
            if md.trim().is_empty() { "\u{200b}".to_string() } else { md }
        })
        .collect();
    // backslash at line end = hard line break
    lines.join("\\\n")
}

pub fn styled(text: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(&to_markdown(text)).unwrap_or_else(|_| slint::StyledText::from_plain_text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_links() {
        assert_eq!(to_markdown("a*b"), "a\\*b");
        assert_eq!(to_markdown("see https://x.org/a?b=1."), "see [https\\:\\/\\/x\\.org\\/a\\?b\\=1](<https://x.org/a?b=1>)\\.");
        assert_eq!(to_markdown("*bold* and _it_"), "**bold** and *it*");
        assert_eq!(to_markdown("2*3*4"), "2\\*3\\*4");
        assert_eq!(to_markdown("l1\n\nl3"), "l1\\\n\u{200b}\\\nl3");
        assert_eq!(links("go www.example.com, now"), vec!["https://www.example.com".to_string()]);
    }
}
