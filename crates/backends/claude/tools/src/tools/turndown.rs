use super::html::{self, Data, Tree};

const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "audio",
    "blockquote",
    "body",
    "canvas",
    "center",
    "dd",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "html",
    "isindex",
    "li",
    "main",
    "menu",
    "nav",
    "noframes",
    "noscript",
    "ol",
    "output",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];
const VOID: &[&str] = &[
    "area", "base", "br", "col", "command", "embed", "hr", "img", "input", "keygen", "link",
    "meta", "param", "source", "track", "wbr",
];
const MEANINGFUL: &[&str] = &[
    "a", "table", "thead", "tbody", "tfoot", "th", "td", "iframe", "script", "audio", "video",
];
const REMOVED: &[&str] = &["style", "script", "noscript", "iframe"];
const TRUNCATED: &str = "\n\n[Content truncated due to length...]";
const TICK: char = '\x60';

pub fn markdown(page: &str, limit: usize) -> String {
    let cut = utf16_prefix(page, limit);
    let (mut tree, root) = html::parse(&page[..cut]);
    collapse(&mut tree, root);
    let converter = Converter { tree: &tree };
    let mut out = trim_output(&converter.children(root, false));
    if cut < page.len() {
        out.push_str(TRUNCATED);
    }
    out
}

pub fn bound(text: String, limit: usize) -> String {
    let cut = utf16_prefix(&text, limit);
    if cut == text.len() {
        return text;
    }
    format!("{}{TRUNCATED}", &text[..cut])
}

fn utf16_prefix(text: &str, limit: usize) -> usize {
    let mut units = 0;
    for (at, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > limit {
            return at;
        }
    }
    text.len()
}

const fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

const fn ascii_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

const fn terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn trim_output(text: &str) -> String {
    text.trim_start_matches(['\t', '\r', '\n'])
        .trim_end_matches(js_space)
        .to_owned()
}

fn join(output: String, addition: &str) -> String {
    let kept = output.trim_end_matches('\n');
    let rest = addition.trim_start_matches('\n');
    let newlines = (output.len() - kept.len())
        .max(addition.len() - rest.len())
        .min(2);
    let mut joined = String::with_capacity(kept.len() + newlines + rest.len());
    joined.push_str(kept);
    joined.push_str(&"\n\n"[..newlines]);
    joined.push_str(rest);
    joined
}

fn is_block(tree: &Tree, node: usize) -> bool {
    tree.html(node).is_some_and(|name| BLOCK.contains(&name))
}

fn is_void(tree: &Tree, node: usize) -> bool {
    tree.html(node).is_some_and(|name| VOID.contains(&name))
}

fn is_pre(tree: &Tree, node: usize) -> bool {
    tree.html(node) == Some("pre")
}

fn collapse(tree: &mut Tree, root: usize) {
    if tree.nodes[root].first.is_none() || is_pre(tree, root) {
        return;
    }
    let mut previous_text: Option<usize> = None;
    let mut keep_leading = false;
    let mut previous: Option<usize> = None;
    let mut node = step(tree, previous, root);
    while node != root {
        match &tree.nodes[node].data {
            Data::Text(text) => {
                let mut collapsed = String::with_capacity(text.len());
                let mut in_space = false;
                for ch in text.chars() {
                    if ascii_space(ch) {
                        if !in_space {
                            collapsed.push(' ');
                        }
                        in_space = true;
                    } else {
                        collapsed.push(ch);
                        in_space = false;
                    }
                }
                let strip = previous_text.is_none_or(|text| match &tree.nodes[text].data {
                    Data::Text(value) => value.ends_with(' '),
                    _ => false,
                });
                if strip && !keep_leading && collapsed.starts_with(' ') {
                    collapsed.remove(0);
                }
                if collapsed.is_empty() {
                    node = remove(tree, node);
                    continue;
                }
                tree.nodes[node].data = Data::Text(collapsed);
                previous_text = Some(node);
            }
            Data::Element { .. } => {
                if is_block(tree, node) || tree.html(node) == Some("br") {
                    if let Some(text) = previous_text {
                        trim_space(tree, text);
                    }
                    previous_text = None;
                    keep_leading = false;
                } else if is_void(tree, node) || is_pre(tree, node) {
                    previous_text = None;
                    keep_leading = true;
                } else if previous_text.is_some() {
                    keep_leading = false;
                }
            }
            Data::Comment => {
                node = remove(tree, node);
                continue;
            }
        }
        let next = step(tree, previous, node);
        previous = Some(node);
        node = next;
    }
    if let Some(text) = previous_text {
        trim_space(tree, text);
        if matches!(&tree.nodes[text].data, Data::Text(value) if value.is_empty()) {
            remove(tree, text);
        }
    }
}

fn trim_space(tree: &mut Tree, node: usize) {
    if let Data::Text(value) = &mut tree.nodes[node].data
        && value.ends_with(' ')
    {
        value.pop();
    }
}

fn remove(tree: &mut Tree, node: usize) -> usize {
    let next = tree.nodes[node]
        .next
        .or(tree.nodes[node].parent)
        .unwrap_or(node);
    tree.detach(node);
    next
}

fn step(tree: &Tree, previous: Option<usize>, node: usize) -> usize {
    let current = &tree.nodes[node];
    if previous.is_some_and(|previous| tree.nodes[previous].parent == Some(node))
        || is_pre(tree, node)
    {
        return current.next.or(current.parent).unwrap_or(node);
    }
    current
        .first
        .or(current.next)
        .or(current.parent)
        .unwrap_or(node)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | TICK | '[' | ']' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    if out.starts_with('-') || out.starts_with("+ ") || out.starts_with('=') {
        out.insert(0, '\\');
    } else if out.starts_with('#') {
        let hashes = out.bytes().take_while(|byte| *byte == b'#').count();
        if hashes <= 6 && out[hashes..].starts_with(' ') {
            out.insert(0, '\\');
        }
    }
    if out.starts_with("~~~") || out.starts_with('>') {
        out.insert(0, '\\');
    }
    let digits = out.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && out[digits..].starts_with(". ") {
        out.insert(digits, '\\');
    }
    out
}

fn clean_attribute(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\n' {
            out.push(ch);
            continue;
        }
        while chars.peek().is_some_and(|next| js_space(*next)) {
            chars.next();
        }
        out.push('\n');
    }
    out
}

fn js_number(text: &str) -> f64 {
    let text = text.trim_matches(js_space);
    if text.is_empty() {
        return 0.0;
    }
    let lower = text.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = lower.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|ch| ch.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |value, ch| {
                value * f64::from(radix) + f64::from(ch.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let valid = unsigned
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E' | '+' | '-'))
        && unsigned
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit() || ch == '.');
    if !valid {
        return f64::NAN;
    }
    text.parse().unwrap_or(f64::NAN)
}

fn js_number_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if value.fract() == 0.0 && value.abs() < 1e21 {
        return format!("{value:.0}");
    }
    format!("{value}")
}

fn has_descendant(tree: &Tree, node: usize, names: &[&str]) -> bool {
    tree.children(node).into_iter().any(|child| {
        tree.html(child).is_some_and(|name| names.contains(&name))
            || has_descendant(tree, child, names)
    })
}

fn inline_code(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let mut code = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                code.push(' ');
            }
            '\n' => code.push(' '),
            _ => code.push(ch),
        }
    }
    let padded = code.starts_with(TICK) || code.ends_with(TICK) || {
        let chars: Vec<char> = code.chars().collect();
        chars.len() >= 3 && chars[0] == ' ' && chars[chars.len() - 1] == ' ' && {
            let middle = &chars[1..chars.len() - 1];
            match middle.iter().filter(|ch| terminator(**ch)).count() {
                0 => middle.iter().any(|ch| *ch != ' '),
                1 => true,
                _ => false,
            }
        }
    };
    let pad = if padded { " " } else { "" };
    let runs: Vec<usize> = code
        .split(|ch| ch != TICK)
        .filter(|run| !run.is_empty())
        .map(str::len)
        .collect();
    let mut width = 1;
    while runs.contains(&width) {
        width += 1;
    }
    let fence = TICK.to_string().repeat(width);
    format!("{fence}{pad}{code}{pad}{fence}")
}

struct Converter<'a> {
    tree: &'a Tree,
}

impl Converter<'_> {
    fn children(&self, parent: usize, code: bool) -> String {
        let mut out = String::new();
        for child in self.tree.children(parent) {
            let addition = match &self.tree.nodes[child].data {
                Data::Text(text) if code => text.clone(),
                Data::Text(text) => escape(text),
                Data::Element { .. } => self.element(child, code),
                Data::Comment => String::new(),
            };
            out = join(out, &addition);
        }
        out
    }

    fn blank(&self, node: usize) -> bool {
        let tree = self.tree;
        !is_void(tree, node)
            && !tree
                .html(node)
                .is_some_and(|name| MEANINGFUL.contains(&name))
            && tree.text_content(node).chars().all(js_space)
            && !has_descendant(tree, node, VOID)
            && !has_descendant(tree, node, MEANINGFUL)
    }

    fn spaced(&self, sibling: Option<usize>, left: bool) -> bool {
        let Some(sibling) = sibling else {
            return false;
        };
        let check = |text: &str| {
            if left {
                text.ends_with(' ')
            } else {
                text.starts_with(' ')
            }
        };
        match &self.tree.nodes[sibling].data {
            Data::Text(text) => check(text),
            Data::Element { .. } if !is_block(self.tree, sibling) => {
                check(&self.tree.text_content(sibling))
            }
            _ => false,
        }
    }

    fn flanking(&self, node: usize) -> (String, String) {
        let text = self.tree.text_content(node);
        let leading_end = text.len() - text.trim_start_matches(js_space).len();
        let (leading, trailing) = if leading_end == text.len() {
            (text.as_str(), "")
        } else {
            (
                &text[..leading_end],
                &text[text.trim_end_matches(js_space).len()..],
            )
        };
        let leading_ascii = leading.len() - leading.trim_start_matches(ascii_space).len();
        let trailing_ascii = trailing.len() - trailing.trim_end_matches(ascii_space).len();
        let node_data = &self.tree.nodes[node];
        let lead = if leading_ascii > 0 && self.spaced(node_data.previous, true) {
            &leading[leading_ascii..]
        } else {
            leading
        };
        let trail = if trailing_ascii > 0 && self.spaced(node_data.next, false) {
            &trailing[..trailing.len() - trailing_ascii]
        } else {
            trailing
        };
        (lead.to_owned(), trail.to_owned())
    }

    fn element(&self, node: usize, parent_code: bool) -> String {
        let tree = self.tree;
        let code = tree.html(node) == Some("code") || parent_code;
        let block = is_block(tree, node);
        let (lead, trail) = if block {
            (String::new(), String::new())
        } else {
            self.flanking(node)
        };
        let mut content = self.children(node, code);
        if !lead.is_empty() || !trail.is_empty() {
            content = content.trim_matches(js_space).to_owned();
        }
        let replaced = if self.blank(node) {
            if block {
                "\n\n".to_owned()
            } else {
                String::new()
            }
        } else {
            self.replace(node, content, block)
        };
        format!("{lead}{replaced}{trail}")
    }

    fn replace(&self, node: usize, content: String, block: bool) -> String {
        let tree = self.tree;
        let name = tree.element(node).map(|(name, _)| name).unwrap_or_default();
        let html = tree.html(node);
        let parent = tree.nodes[node].parent;
        let first = tree.nodes[node].first;
        match name {
            "p" => return format!("\n\n{content}\n\n"),
            "br" => return "  \n".into(),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                if level < 3 {
                    let mark = if level == 1 { "=" } else { "-" };
                    let width = content.encode_utf16().count();
                    return format!("\n\n{content}\n{}\n\n", mark.repeat(width));
                }
                return format!("\n\n{} {content}\n\n", "#".repeat(level));
            }
            "blockquote" => {
                let inner = content.trim_matches('\n');
                let mut quoted = String::from("> ");
                for ch in inner.chars() {
                    quoted.push(ch);
                    if terminator(ch) {
                        quoted.push_str("> ");
                    }
                }
                return format!("\n\n{quoted}\n\n");
            }
            "ul" | "ol" => {
                let last_element = parent.and_then(|parent| {
                    tree.children(parent)
                        .into_iter()
                        .rev()
                        .find(|&child| tree.element(child).is_some())
                });
                if parent.is_some_and(|parent| tree.html(parent) == Some("li"))
                    && last_element == Some(node)
                {
                    return format!("\n{content}");
                }
                return format!("\n\n{content}\n\n");
            }
            "li" => {
                let body = content.trim_start_matches('\n');
                let kept = body.trim_end_matches('\n');
                let body = if kept.len() < body.len() {
                    format!("{kept}\n")
                } else {
                    kept.to_owned()
                };
                let body = body.replace('\n', "\n    ");
                let mut prefix = "*   ".to_owned();
                if let Some(parent) = parent
                    && tree.html(parent) == Some("ol")
                {
                    let index = tree
                        .children(parent)
                        .into_iter()
                        .filter(|&child| tree.element(child).is_some())
                        .position(|child| child == node)
                        .unwrap_or(0);
                    let number = match tree
                        .attribute(parent, "start")
                        .filter(|start| !start.is_empty())
                    {
                        Some(start) => js_number_text(js_number(start) + index as f64),
                        None => (index + 1).to_string(),
                    };
                    prefix = format!("{number}.  ");
                }
                let tail = if tree.nodes[node].next.is_some() && !body.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                return format!("{prefix}{body}{tail}");
            }
            _ => {}
        }
        if html == Some("pre")
            && let Some(first) = first.filter(|&first| tree.html(first) == Some("code"))
        {
            let code = tree.text_content(first);
            return format!("\n\n    {}\n\n", code.replace('\n', "\n    "));
        }
        if name == "hr" {
            return "\n\n* * *\n\n".into();
        }
        if html == Some("a")
            && let Some(href) = tree.attribute(node, "href").filter(|href| !href.is_empty())
        {
            let href = href.replace('(', "\\(").replace(')', "\\)");
            let title = clean_attribute(tree.attribute(node, "title"));
            let title = if title.is_empty() {
                title
            } else {
                format!(" \"{}\"", title.replace('"', "\\\""))
            };
            return format!("[{content}]({href}{title})");
        }
        if matches!(name, "em" | "i" | "strong" | "b") {
            if content.trim_matches(js_space).is_empty() {
                return String::new();
            }
            let mark = if matches!(name, "em" | "i") {
                "_"
            } else {
                "**"
            };
            return format!("{mark}{content}{mark}");
        }
        if html == Some("code") {
            let alone = parent.is_some_and(|parent| tree.html(parent) == Some("pre"))
                && tree.nodes[node].previous.is_none()
                && tree.nodes[node].next.is_none();
            if !alone {
                return inline_code(&content);
            }
        }
        if name == "img" {
            let alt = clean_attribute(tree.attribute(node, "alt"));
            let src = tree.attribute(node, "src").unwrap_or_default();
            let title = clean_attribute(tree.attribute(node, "title"));
            let title = if title.is_empty() {
                title
            } else {
                format!(" \"{title}\"")
            };
            return if src.is_empty() {
                String::new()
            } else {
                format!("![{alt}]({src}{title})")
            };
        }
        if REMOVED.contains(&name) {
            return String::new();
        }
        if block {
            format!("\n\n{content}\n\n")
        } else {
            content
        }
    }
}
