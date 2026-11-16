//! Cross-reference tokenizer: `#12`, `owner/repo#12`, `@user`, commit shas,
//! and closing keywords (`fixes #12`). Shared by markdown rendering and the
//! post-receive commit scanner.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Issue { repo: Option<(String, String)>, number: i64, closing: bool },
    User(String),
    Sha(String),
}

pub enum Segment<'a> {
    Text(&'a str),
    Ref(Token, &'a str),
}

const CLOSING: &[&str] = &["close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved"];

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._-".contains(&b)
}

/// Is the text just before `at` a closing keyword (optionally followed by `:`)?
fn preceded_by_closing(s: &str, at: usize) -> bool {
    let before = s[..at].trim_end();
    let before = before.strip_suffix(':').unwrap_or(before).trim_end();
    let word_start = before.rfind(|c: char| !c.is_ascii_alphabetic()).map_or(0, |i| i + 1);
    let word = &before[word_start..];
    CLOSING.iter().any(|k| k.eq_ignore_ascii_case(word))
}

/// Split text into plain runs and reference tokens.
pub fn scan(s: &str) -> Vec<Segment<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        let prev_ok = i == 0 || !is_word(b[i - 1]) && b[i - 1] != b'/' && b[i - 1] != b'&';
        let found: Option<(usize, usize, Token)> = match b[i] {
            b'#' if prev_ok => {
                let end = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
                let digits = &s[i + 1..end];
                let bounded = end == b.len() || !is_word(b[end]);
                match digits.parse::<i64>() {
                    Ok(n) if !digits.is_empty() && digits.len() < 10 && bounded && n > 0 => {
                        Some((i, end, Token::Issue { repo: None, number: n, closing: preceded_by_closing(s, i) }))
                    }
                    _ => None,
                }
            }
            b'@' if i == 0 || !(is_name(b[i - 1]) || b[i - 1] == b'@') => {
                let end = (i + 1..b.len()).find(|&j| !is_name(b[j])).unwrap_or(b.len());
                let name = s[i + 1..end].trim_end_matches(['.', '-']);
                let end = i + 1 + name.len();
                (!name.is_empty() && crate::db::valid_name(name)).then(|| (i, end, Token::User(name.to_owned())))
            }
            c if c.is_ascii_alphanumeric() && (i == 0 || !is_name(b[i - 1])) => {
                let end = (i..b.len()).find(|&j| !is_name(b[j]) && b[j] != b'/' && b[j] != b'#').unwrap_or(b.len());
                let word = &s[i..end];
                if let Some(t) = qualified_issue(word, s, i) {
                    Some((i, i + t.1, t.0))
                } else {
                    let hex_end = (i..b.len()).find(|&j| !b[j].is_ascii_hexdigit()).unwrap_or(b.len());
                    let len = hex_end - i;
                    let bounded = hex_end == b.len() || !is_word(b[hex_end]);
                    let hex = &s[i..hex_end];
                    let has_digit = hex.bytes().any(|c| c.is_ascii_digit());
                    let has_alpha = hex.bytes().any(|c| c.is_ascii_alphabetic());
                    ((7..=40).contains(&len) && bounded && has_digit && has_alpha)
                        .then(|| (i, hex_end, Token::Sha(hex.to_ascii_lowercase())))
                }
            }
            _ => None,
        };
        match found {
            Some((start, end, tok)) => {
                if start > last {
                    out.push(Segment::Text(&s[last..start]));
                }
                out.push(Segment::Ref(tok, &s[start..end]));
                last = end;
                i = end;
            }
            None => i += s[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if last < s.len() {
        out.push(Segment::Text(&s[last..]));
    }
    out
}

/// `owner/repo#12` at the start of `word`; returns the token and its byte length.
fn qualified_issue(word: &str, full: &str, at: usize) -> Option<(Token, usize)> {
    let (owner, rest) = word.split_once('/')?;
    let (repo, rest) = rest.split_once('#')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let number: i64 = digits.parse().ok().filter(|n| *n > 0)?;
    if !crate::db::valid_name(owner) || !crate::db::valid_name(repo) || digits.len() >= 10 {
        return None;
    }
    let len = owner.len() + 1 + repo.len() + 1 + digits.len();
    let end = at + len;
    if end < full.len() && is_word(full.as_bytes()[end]) {
        return None;
    }
    Some((Token::Issue { repo: Some((owner.to_owned(), repo.to_owned())), number, closing: preceded_by_closing(full, at) }, len))
}

/// Issue references in `text`, deduped: (repo, number, closing).
pub fn issue_refs(text: &str) -> Vec<(Option<(String, String)>, i64, bool)> {
    let mut v: Vec<(Option<(String, String)>, i64, bool)> = Vec::new();
    for seg in scan(text) {
        if let Segment::Ref(Token::Issue { repo, number, closing }, _) = seg {
            match v.iter_mut().find(|(r, n, _)| *r == repo && *n == number) {
                Some(e) => e.2 |= closing,
                None => v.push((repo, number, closing)),
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        scan(s).into_iter().filter_map(|x| if let Segment::Ref(t, _) = x { Some(t) } else { None }).collect()
    }

    #[test]
    fn issues_and_closing() {
        assert_eq!(
            toks("Fixes #12, see #3"),
            vec![
                Token::Issue { repo: None, number: 12, closing: true },
                Token::Issue { repo: None, number: 3, closing: false }
            ]
        );
        assert_eq!(toks("closes: #7"), vec![Token::Issue { repo: None, number: 7, closing: true }]);
        assert_eq!(
            toks("resolved acme/tools#4"),
            vec![Token::Issue { repo: Some(("acme".into(), "tools".into())), number: 4, closing: true }]
        );
    }

    #[test]
    fn not_references() {
        assert!(toks("a#1 &#38; #0 #abc url/#5 C#").is_empty());
        assert!(toks("mail me@example.com").is_empty());
        assert!(toks("deadbeef cafe 1234567").is_empty(), "pure hex words / pure digits are not shas");
    }

    #[test]
    fn users_and_shas() {
        assert_eq!(toks("thanks @bob."), vec![Token::User("bob".into())]);
        assert_eq!(toks("in 3fbcad1275"), vec![Token::Sha("3fbcad1275".into())]);
    }
}
