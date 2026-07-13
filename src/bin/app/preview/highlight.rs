//! Lightweight, zero-dependency syntax highlighting for the Quick preview viewer's code display
//! (code files + markdown fenced code blocks). A single-pass per-line lexer per language — line/
//! block comments (with cross-line state), string literals, numbers, and a per-language keyword
//! set — NOT a real parser. Deliberately small (no syntect / onig / regex). Colours come from the
//! theme (`dark.rs`). Keyword tables are intentionally incomplete: enough to "look colourized",
//! not to be a grammar.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    DrawTextW, ExtTextOutW, GetTextExtentPoint32W, GetTextMetricsW, SelectObject, SetTextColor,
    DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, ETO_CLIPPED, HDC, HFONT, TEXTMETRICW,
};

/// Languages we specially lex. `Plain` = no colouring (falls back to today's uncoloured draw).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Lang {
    Rust,
    Py,
    Js,
    Json,
    Yaml,
    Toml,
    C,
    Cs,
    Java,
    Go,
    Ruby,
    Php,
    Lua,
    Kotlin,
    Swift,
    Sh,
    Html,
    Css,
    Xml,
    Sql,
    Plain,
}

#[derive(Clone, Copy, PartialEq)]
enum Tag {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// Map a file extension (no dot) to a language.
pub(super) fn lang_from_ext(ext: &str) -> Lang {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Py,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" => Lang::C,
        "cs" => Lang::Cs,
        "java" => Lang::Java,
        "go" => Lang::Go,
        "rb" | "rake" | "gemspec" => Lang::Ruby,
        "php" | "phtml" => Lang::Php,
        "lua" => Lang::Lua,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => Lang::Sh,
        "html" | "htm" | "xhtml" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        "xml" => Lang::Xml,
        "sql" => Lang::Sql,
        // ini/cfg files share TOML's shape (# / ; comments, key=value, quoted strings).
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" | "gitconfig" => Lang::Toml,
        _ => Lang::Plain,
    }
}

/// Map a markdown fenced-code info-string tag (e.g. ```` ```rust ````) to a language. Also accepts
/// a bare extension as the tag.
pub(super) fn lang_from_fence(tag: &str) -> Lang {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" => Lang::Py,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "cpp" | "c++" | "h" | "hpp" => Lang::C,
        "cs" | "csharp" | "c#" => Lang::Cs,
        "java" => Lang::Java,
        "go" | "golang" => Lang::Go,
        "ruby" | "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "lua" => Lang::Lua,
        "kotlin" | "kt" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "shell" | "zsh" | "ps1" | "powershell" | "bat" | "console" => Lang::Sh,
        "html" | "htm" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        other => lang_from_ext(other),
    }
}

/// Per-language lexer spec.
struct Spec {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [u8], // quote chars
    keywords: &'static [&'static str],
}

/// The per-language tuple `spec()` builds from: (line comments, block comment, quotes, keywords).
type SpecParts = (
    &'static [&'static str],
    Option<(&'static str, &'static str)>,
    &'static [u8],
    &'static [&'static str],
);

fn spec(lang: Lang) -> Spec {
    let (lc, bl, st, kw): SpecParts = match lang {
        Lang::Rust => (&["//"], Some(("/*", "*/")), b"\"", RUST_KW),
        Lang::Py => (&["#"], None, b"\"'", PY_KW),
        Lang::Js => (&["//"], Some(("/*", "*/")), b"\"'`", JS_KW),
        Lang::Json => (&[], None, b"\"", &["true", "false", "null"]),
        Lang::Yaml => (&["#"], None, b"\"'", YAML_KW),
        Lang::Toml => (&["#"], None, b"\"'", &["true", "false"]),
        Lang::C => (&["//"], Some(("/*", "*/")), b"\"'", C_KW),
        Lang::Cs => (&["//"], Some(("/*", "*/")), b"\"'", CS_KW),
        Lang::Java => (&["//"], Some(("/*", "*/")), b"\"", JAVA_KW),
        Lang::Go => (&["//"], Some(("/*", "*/")), b"\"`", GO_KW),
        Lang::Ruby => (&["#"], None, b"\"'", RUBY_KW),
        Lang::Php => (&["//", "#"], Some(("/*", "*/")), b"\"'", PHP_KW),
        Lang::Lua => (&["--"], None, b"\"'", LUA_KW),
        Lang::Kotlin => (&["//"], Some(("/*", "*/")), b"\"", KOTLIN_KW),
        Lang::Swift => (&["//"], Some(("/*", "*/")), b"\"", SWIFT_KW),
        Lang::Sh => (&["#"], None, b"\"'", SH_KW),
        Lang::Html | Lang::Xml => (&[], Some(("<!--", "-->")), b"\"'", &[]),
        Lang::Css => (&[], Some(("/*", "*/")), b"\"'", &[]),
        Lang::Sql => (&["--"], Some(("/*", "*/")), b"'", SQL_KW),
        Lang::Plain => (&[], None, b"", &[]),
    };
    Spec { line_comment: lc, block: bl, strings: st, keywords: kw }
}

/// UTF-8 byte length of the char starting with lead byte `b`.
fn utf8_len(b: u8) -> usize {
    if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..).and_then(|s| s.find(needle)).map(|p| p + from)
}

/// Tokenize one line into `(tag, slice)` runs. `in_block` carries block-comment state across
/// lines. Non-keyword identifiers + punctuation stay in `Plain` runs (few runs per line).
fn tokenize<'a>(line: &'a str, sp: &Spec, in_block: &mut bool) -> Vec<(Tag, &'a str)> {
    let b = line.as_bytes();
    let n = b.len();
    let mut out: Vec<(Tag, &'a str)> = Vec::new();
    let mut i = 0usize;
    let mut seg = 0usize; // start of the pending Plain segment

    macro_rules! flush {
        ($upto:expr) => {
            if $upto > seg {
                out.push((Tag::Plain, &line[seg..$upto]));
            }
        };
    }

    while i < n {
        // carried-over block comment
        if *in_block {
            if let Some((_, close)) = sp.block {
                if let Some(pos) = find_from(line, i, close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                    *in_block = false;
                    continue;
                }
            }
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // line comment -> rest of line
        if sp.line_comment.iter().any(|c| line[i..].starts_with(*c)) {
            flush!(i);
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // block comment open
        if let Some((open, close)) = sp.block {
            if line[i..].starts_with(open) {
                flush!(i);
                if let Some(pos) = find_from(line, i + open.len(), close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                } else {
                    out.push((Tag::Comment, &line[i..]));
                    i = n;
                    seg = n;
                    *in_block = true;
                }
                continue;
            }
        }
        let ch = b[i];
        // string literal
        if sp.strings.contains(&ch) {
            flush!(i);
            let start = i;
            i += 1;
            while i < n {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == ch {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(n);
            // A string immediately followed by `:` is an object KEY / property — colour it like a
            // keyword (matches QuickLook's blue property names) instead of an orange string value.
            let is_key = line.get(end..).is_some_and(|r| r.trim_start().starts_with(':'));
            out.push((if is_key { Tag::Keyword } else { Tag::Str }, &line[start..end]));
            i = end;
            seg = i;
            continue;
        }
        // number literal
        if ch.is_ascii_digit() {
            flush!(i);
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            out.push((Tag::Num, &line[start..i]));
            seg = i;
            continue;
        }
        // identifier -> keyword lookup (non-keywords stay in the plain segment)
        if ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if sp.keywords.contains(&word) {
                flush!(start);
                out.push((Tag::Keyword, word));
                seg = i;
            }
            continue;
        }
        // plain char (advance by full UTF-8 char so slices never split a codepoint)
        i += if ch < 0x80 { 1 } else { utf8_len(ch) };
    }
    flush!(n);
    out
}

/// Theme-resolved code colours (plain uses the caller's `fg`).
struct Colors {
    plain: u32,
    comment: u32,
    string: u32,
    num: u32,
    keyword: u32,
}
impl Colors {
    fn of(&self, t: Tag) -> u32 {
        match t {
            Tag::Plain => self.plain,
            Tag::Comment => self.comment,
            Tag::Str => self.string,
            Tag::Num => self.num,
            Tag::Keyword => self.keyword,
        }
    }
}

/// Draw `text` as syntax-highlighted monospace lines starting at `(x, y0)`, one line per source
/// line, each run clipped to `[x, x+width]` (long code lines clip at the pane edge rather than
/// wrapping — normal for a code view). Lines fully outside `[clip_top, clip_bottom)` are not drawn
/// (scroll culling) but are still lexed so cross-line block-comment state stays correct. `font`
/// must be the mono font. Returns the total content height. `Lang::Plain` draws every run in `fg`.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_lines(
    hdc: HDC,
    text: &str,
    lang: Lang,
    x: i32,
    y0: i32,
    width: i32,
    clip_top: i32,
    clip_bottom: i32,
    font: HFONT,
    fg: u32,
) -> i32 {
    let colors = Colors {
        plain: fg,
        comment: crate::dark::CODE_COMMENT().0,
        string: crate::dark::CODE_STRING().0,
        num: crate::dark::CODE_NUMBER().0,
        keyword: crate::dark::CODE_KEYWORD().0,
    };
    let sp = spec(lang);
    // `ExtTextOutW` doesn't expand tabs (unlike the plain path's DT_EXPANDTABS), so tab-indented
    // code (Go, Makefiles) would collapse its indentation — expand once up front.
    let text = text.replace('\t', "    ");
    let text = text.as_str();
    let old = SelectObject(hdc, font.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading;

    // Left gutter: right-aligned 1-based line numbers in a muted colour, like QuickLook / an editor.
    // Its width is sized to the digit count of the last line, so the code column shifts right by it.
    let total_lines = text.split('\n').count().max(1);
    let char_w = tm.tmAveCharWidth.max(1);
    let digits = total_lines.to_string().len() as i32;
    let gutter_pad = char_w; // gap between the numbers and the code
    let gutter_w = digits * char_w + gutter_pad * 2;
    let code_x = x + gutter_w;
    let code_right = x + width; // the code column ends at the same right edge as before
    let gutter_fg = crate::dark::HEADER_TEXT().0;

    let mut in_block = false;
    let mut y = y0;
    let mut line_no = 0usize;
    for line in text.split('\n') {
        line_no += 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let runs = tokenize(line, &sp, &mut in_block); // always lex (block-comment state)
        if y + line_h > clip_top && y < clip_bottom {
            // line number, right-aligned in [x, x+gutter_w-pad]
            SetTextColor(hdc, COLORREF(gutter_fg));
            let mut num: Vec<u16> = line_no.to_string().encode_utf16().collect();
            let mut nr = RECT { left: x, top: y, right: x + gutter_w - gutter_pad, bottom: y + line_h };
            DrawTextW(hdc, &mut num, &mut nr, DT_RIGHT | DT_SINGLELINE | DT_NOPREFIX);
            // code runs, starting past the gutter
            let mut cx = code_x;
            for (tag, s) in runs {
                if s.is_empty() {
                    continue;
                }
                SetTextColor(hdc, COLORREF(colors.of(tag)));
                let w16: Vec<u16> = s.encode_utf16().collect();
                let clip = RECT { left: cx, top: y, right: code_right, bottom: y + line_h };
                let _ = ExtTextOutW(hdc, cx, y, ETO_CLIPPED, Some(&clip as *const RECT), PCWSTR(w16.as_ptr()), w16.len() as u32, None);
                let mut sz = SIZE::default();
                let _ = GetTextExtentPoint32W(hdc, &w16, &mut sz);
                cx += sz.cx;
                if cx > code_right {
                    break; // rest of the line is off the pane
                }
            }
        }
        y += line_h;
    }
    SelectObject(hdc, old);
    y - y0
}

// ---- keyword tables (intentionally small / incomplete) -----------------------------------

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128",
    "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "Vec", "Option", "Some",
    "None", "Result", "Ok", "Err", "Box", "Rc", "Arc",
];
const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "print", "len", "range", "int", "str", "float", "bool", "list", "dict",
    "set", "tuple",
];
const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default",
    "delete", "do", "else", "enum", "export", "extends", "false", "finally", "for", "function", "if",
    "implements", "import", "in", "instanceof", "interface", "let", "new", "null", "of", "return",
    "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined",
    "var", "void", "while", "yield", "string", "number", "boolean", "any", "readonly",
];
const YAML_KW: &[&str] = &["true", "false", "null", "yes", "no", "on", "off"];
const C_KW: &[&str] = &[
    "auto", "bool", "break", "case", "char", "class", "const", "continue", "default", "do", "double",
    "else", "enum", "extern", "false", "float", "for", "goto", "if", "inline", "int", "long",
    "namespace", "new", "nullptr", "private", "protected", "public", "register", "return", "short",
    "signed", "sizeof", "static", "struct", "switch", "template", "this", "true", "typedef", "union",
    "unsigned", "using", "virtual", "void", "volatile", "while",
];
const CS_KW: &[&str] = &[
    "abstract", "as", "async", "await", "base", "bool", "break", "byte", "case", "catch", "char",
    "class", "const", "continue", "decimal", "default", "delegate", "do", "double", "else", "enum",
    "event", "false", "finally", "float", "for", "foreach", "get", "if", "in", "int", "interface",
    "internal", "is", "long", "namespace", "new", "null", "object", "out", "override", "params",
    "private", "protected", "public", "readonly", "ref", "return", "sealed", "set", "static",
    "string", "struct", "switch", "this", "throw", "true", "try", "typeof", "using", "var",
    "virtual", "void", "while", "var",
];
const SH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case", "esac",
    "function", "return", "exit", "echo", "export", "local", "set", "source", "sudo", "cd", "param",
    "function", "foreach", "begin", "process", "end", "true", "false",
];
const SQL_KW: &[&str] = &[
    "SELECT", "select", "FROM", "from", "WHERE", "where", "INSERT", "insert", "UPDATE", "update",
    "DELETE", "delete", "INTO", "into", "VALUES", "values", "SET", "set", "JOIN", "join", "LEFT",
    "left", "RIGHT", "right", "INNER", "inner", "OUTER", "outer", "ON", "on", "AND", "and", "OR",
    "or", "NOT", "not", "NULL", "null", "AS", "as", "ORDER", "order", "BY", "by", "GROUP", "group",
    "HAVING", "having", "LIMIT", "limit", "CREATE", "create", "TABLE", "table", "PRIMARY", "primary",
    "KEY", "key", "DROP", "drop", "ALTER", "alter", "INDEX", "index", "DISTINCT", "distinct",
];

// The seven tables below are distilled from Monaco Editor's Monarch grammars
// (monaco-editor/src/basic-languages/<lang>, MIT) — the `keywords` + `typeKeywords`
// arrays, trimmed to the ~most-seen subset. Same "look colourized, not a grammar"
// stance as the tables above.

const JAVA_KW: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
    "continue", "default", "do", "double", "else", "enum", "extends", "false", "final", "finally",
    "float", "for", "goto", "if", "implements", "import", "instanceof", "int", "interface", "long",
    "native", "new", "null", "package", "permits", "private", "protected", "public", "record",
    "return", "sealed", "short", "static", "strictfp", "super", "switch", "synchronized", "this",
    "throw", "throws", "transient", "true", "try", "var", "void", "volatile", "while", "yield",
    "String", "Integer", "Boolean", "Object", "List", "Map",
];

const GO_KW: &[&str] = &[
    "any", "append", "bool", "break", "byte", "cap", "case", "chan", "close", "complex64",
    "complex128", "const", "continue", "copy", "default", "defer", "delete", "else", "error",
    "fallthrough", "false", "float32", "float64", "for", "func", "go", "goto", "if", "import",
    "int", "int8", "int16", "int32", "int64", "interface", "iota", "len", "make", "map", "new",
    "nil", "package", "panic", "range", "recover", "return", "rune", "select", "string", "struct",
    "switch", "true", "type", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "var",
];

const RUBY_KW: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif",
    "end", "ensure", "extend", "false", "for", "if", "in", "include", "loop", "module", "next",
    "nil", "not", "or", "private", "protected", "public", "puts", "raise", "redo", "require",
    "require_relative", "rescue", "retry", "return", "self", "super", "then", "true", "undef",
    "unless", "until", "when", "while", "yield", "attr_accessor", "attr_reader", "attr_writer",
    "lambda", "proc", "new",
];

const PHP_KW: &[&str] = &[
    "abstract", "and", "array", "as", "break", "callable", "case", "catch", "class", "clone",
    "const", "continue", "declare", "default", "die", "do", "echo", "else", "elseif", "empty",
    "enum", "exit", "extends", "false", "final", "finally", "fn", "for", "foreach", "function",
    "global", "if",
    "implements", "include", "include_once", "instanceof", "interface", "isset", "list", "match",
    "namespace", "new", "null", "or", "print", "private", "protected", "public", "readonly",
    "require", "require_once", "return", "static", "switch", "throw", "trait", "true", "try",
    "unset", "use", "var", "while", "xor", "yield", "int", "float", "string", "bool", "void",
    "mixed", "never", "object", "self", "parent", "this",
];

const LUA_KW: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "print",
    "pairs", "ipairs", "type", "tostring", "tonumber", "require", "pcall", "error", "self",
];

const KOTLIN_KW: &[&str] = &[
    "abstract", "actual", "annotation", "as", "break", "by", "catch", "class", "companion",
    "const", "constructor", "continue", "crossinline", "data", "do", "else", "enum", "expect",
    "external", "false", "final", "finally", "for", "fun", "get", "if", "import", "in", "infix",
    "init", "inline", "inner", "interface", "internal", "is", "it", "lateinit", "noinline",
    "null", "object", "open", "operator", "out", "override", "package", "private", "protected",
    "public", "reified", "return", "sealed", "set", "super", "suspend", "tailrec", "this",
    "throw", "true", "try", "typealias", "val", "var", "vararg", "when", "where", "while",
    "Int", "Long", "Float", "Double", "Boolean", "String", "Unit", "Any", "List", "Map",
];

const SWIFT_KW: &[&str] = &[
    "actor", "as", "associatedtype", "async", "await", "break", "case", "catch", "class",
    "continue", "convenience", "default", "defer", "deinit", "didSet", "do", "dynamic", "else",
    "enum", "extension", "fallthrough", "false", "fileprivate", "final", "for", "func", "get",
    "guard", "if", "import", "in", "indirect", "infix", "init", "inout", "internal", "is",
    "lazy", "let", "mutating", "nil", "nonmutating", "open", "operator", "optional", "override",
    "postfix", "precedencegroup", "prefix", "private", "protocol", "public", "repeat", "required",
    "rethrows", "return", "self", "Self", "set", "some", "static", "struct", "subscript", "super",
    "switch", "throw", "throws", "true", "try", "typealias", "unowned", "var", "weak", "where",
    "while", "willSet", "Int", "Double", "Float", "Bool", "String", "Array", "Dictionary",
    "Optional", "Void",
];
