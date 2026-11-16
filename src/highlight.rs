//! A deliberately small syntax highlighter: comments, strings, numbers,
//! keywords and type-ish identifiers for the languages people keep in git.
//! No dependencies, no regex — a scanner over bytes, so a 100 KB file costs
//! well under a millisecond and nothing is cached in RAM.

struct Lang {
    line_comments: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    quotes: &'static [u8],
    keywords: &'static [&'static str],
    types: bool,
}

const RUST: Lang = Lang {
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    quotes: b"\"",
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for",
        "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
        "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "union",
    ],
    types: true,
};

const C: Lang = Lang {
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    quotes: b"\"'",
    keywords: &[
        "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for", "goto", "if", "inline",
        "register", "restrict", "return", "sizeof", "static", "struct", "switch", "typedef", "union", "volatile", "while",
        "class", "namespace", "template", "public", "private", "protected", "virtual", "new", "delete", "using", "nullptr",
        "true", "false", "throw", "try", "catch",
    ],
    types: true,
};

const PY: Lang = Lang {
    line_comments: &["#"],
    block: None,
    quotes: b"\"'",
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
        "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or",
        "pass", "raise", "return", "True", "try", "while", "with", "yield",
    ],
    types: false,
};

const JS: Lang = Lang {
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    quotes: b"\"'`",
    keywords: &[
        "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
        "else", "export", "extends", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "let", "new",
        "of", "return", "static", "super", "switch", "this", "throw", "try", "typeof", "var", "void", "while", "yield",
        "true", "false", "null", "undefined",
    ],
    types: false,
};

const GO: Lang = Lang {
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    quotes: b"\"'`",
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto",
        "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var",
        "nil", "true", "false",
    ],
    types: true,
};

const SH: Lang = Lang {
    line_comments: &["#"],
    block: None,
    quotes: b"\"'",
    keywords: &["if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac", "function", "return", "local", "export", "set", "unset", "shift", "echo", "cd"],
    types: false,
};

const SQL: Lang = Lang {
    line_comments: &["--"],
    block: Some(("/*", "*/")),
    quotes: b"'\"",
    keywords: &[
        "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "index", "drop",
        "alter", "join", "left", "right", "inner", "outer", "on", "group", "by", "order", "having", "limit", "offset", "as",
        "and", "or", "not", "null", "primary", "key", "foreign", "references", "default", "union", "distinct", "case", "when",
        "then", "else", "end",
    ],
    types: false,
};

const JSON: Lang = Lang { line_comments: &[], block: None, quotes: b"\"", keywords: &["true", "false", "null"], types: false };
const TOML: Lang = Lang {
    line_comments: &["#"],
    block: None,
    quotes: b"\"'",
    keywords: &["true", "false"],
    types: false,
};

fn lang_for(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    Some(match ext {
        "rs" => RUST,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "kt" | "swift" | "scala" | "m" | "mm" => C,
        "py" | "pyi" | "rb" | "pl" | "r" => PY,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "vue" | "svelte" => JS,
        "go" => GO,
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "ps1" => SH,
        "sql" => SQL,
        "json" | "jsonc" | "json5" => JSON,
        "toml" => TOML,
        "yml" | "yaml" => Lang { line_comments: &["#"], block: None, quotes: b"\"'", keywords: &["true", "false", "null"], types: false },
        "ini" | "cfg" | "conf" => Lang { line_comments: &["#", ";"], block: None, quotes: b"\"", keywords: &["true", "false"], types: false },
        _ if name == "makefile" || name.starts_with("makefile") || ext == "mk" => {
            Lang { line_comments: &["#"], block: None, quotes: b"\"'", keywords: &["ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "export", "define", "endef"], types: false }
        }
        _ => return None,
    })
}

fn esc(out: &mut String, s: &str) {
    out.push_str(&crate::ui::escape(s));
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Highlighted HTML for each line (comments can span lines).
pub fn lines(path: &str, text: &str) -> Vec<String> {
    let Some(l) = lang_for(path) else {
        return text.lines().map(|x| crate::ui::escape(x)).collect();
    };
    let mut out = Vec::new();
    let mut block: Option<&'static str> = None;
    for line in text.lines() {
        let b = line.as_bytes();
        let mut h = String::with_capacity(line.len() + 32);
        let mut i = 0;
        let mut plain = 0;
        let flush = |h: &mut String, from: usize, to: usize, text: &str| {
            if from < to {
                esc(h, &text[from..to]);
            }
        };
        while i < b.len() {
            if let Some(end) = block {
                match line[i..].find(end) {
                    Some(p) => {
                        let stop = i + p + end.len();
                        flush(&mut h, plain, i, line);
                        h.push_str("<span class=\"tok-c\">");
                        esc(&mut h, &line[i..stop]);
                        h.push_str("</span>");
                        i = stop;
                        plain = i;
                        block = None;
                    }
                    None => {
                        flush(&mut h, plain, i, line);
                        h.push_str("<span class=\"tok-c\">");
                        esc(&mut h, &line[i..]);
                        h.push_str("</span>");
                        i = b.len();
                        plain = i;
                    }
                }
                continue;
            }
            let rest = &line[i..];
            if l.line_comments.iter().any(|c| rest.starts_with(c)) {
                flush(&mut h, plain, i, line);
                h.push_str("<span class=\"tok-c\">");
                esc(&mut h, rest);
                h.push_str("</span>");
                break;
            }
            if let Some((start, end)) = l.block {
                if rest.starts_with(start) {
                    flush(&mut h, plain, i, line);
                    match rest[start.len()..].find(end) {
                        Some(p) => {
                            let stop = i + start.len() + p + end.len();
                            h.push_str("<span class=\"tok-c\">");
                            esc(&mut h, &line[i..stop]);
                            h.push_str("</span>");
                            i = stop;
                            plain = i;
                        }
                        None => {
                            h.push_str("<span class=\"tok-c\">");
                            esc(&mut h, rest);
                            h.push_str("</span>");
                            i = b.len();
                            plain = i;
                            block = Some(end);
                        }
                    }
                    continue;
                }
            }
            let c = b[i];
            if l.quotes.contains(&c) {
                flush(&mut h, plain, i, line);
                let mut j = i + 1;
                let mut esc_next = false;
                while j < b.len() {
                    if esc_next {
                        esc_next = false;
                    } else if b[j] == b'\\' {
                        esc_next = true;
                    } else if b[j] == c {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                h.push_str("<span class=\"tok-s\">");
                esc(&mut h, &line[i..j.min(b.len())]);
                h.push_str("</span>");
                i = j;
                plain = i;
                continue;
            }
            if c.is_ascii_digit() && (i == 0 || !is_ident(b[i - 1])) {
                flush(&mut h, plain, i, line);
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
                    j += 1;
                }
                h.push_str("<span class=\"tok-n\">");
                esc(&mut h, &line[i..j]);
                h.push_str("</span>");
                i = j;
                plain = i;
                continue;
            }
            if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_ident(b[i - 1])) {
                let mut j = i;
                while j < b.len() && is_ident(b[j]) {
                    j += 1;
                }
                let word = &line[i..j];
                let lower = word.to_ascii_lowercase();
                let class = if l.keywords.iter().any(|k| *k == word || *k == lower) {
                    Some("tok-k")
                } else if l.types && word.as_bytes()[0].is_ascii_uppercase() && !word.as_bytes().iter().all(u8::is_ascii_uppercase) {
                    Some("tok-t")
                } else {
                    None
                };
                if let Some(k) = class {
                    flush(&mut h, plain, i, line);
                    h.push_str(&format!("<span class=\"{k}\">"));
                    esc(&mut h, word);
                    h.push_str("</span>");
                    plain = j;
                }
                i = j;
                continue;
            }
            i += 1;
        }
        flush(&mut h, plain, b.len(), line);
        out.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_and_marks() {
        let l = super::lines("x.rs", "fn main() { let s = \"<b>\"; } // hi <i>");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("tok-k\">fn<"));
        assert!(l[0].contains("&quot;&lt;b&gt;&quot;"));
        assert!(l[0].contains("tok-c\">// hi &lt;i&gt;"));
    }

    #[test]
    fn block_comment_spans_lines() {
        let l = super::lines("a.c", "/* start\nstill <x>\n*/ int y;");
        assert!(l[0].starts_with("<span class=\"tok-c\">"));
        assert!(l[1].contains("tok-c\">still &lt;x&gt;"));
        assert!(l[2].contains("tok-c\">*/</span>"));
    }

    #[test]
    fn unknown_language_is_escaped_text() {
        let l = super::lines("a.xyz", "<script>");
        assert_eq!(l, vec!["&lt;script&gt;".to_string()]);
    }
}
