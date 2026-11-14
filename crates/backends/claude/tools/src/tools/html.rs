use super::entities::ENTITIES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Space {
    Html,
    MathMl,
    Svg,
}

#[derive(Debug)]
pub(super) enum Data {
    Comment,
    Element {
        attributes: Vec<(String, String)>,
        name: String,
        space: Space,
    },
    Text(String),
}

#[derive(Debug)]
pub(super) struct Node {
    pub(super) content: Option<usize>,
    pub(super) data: Data,
    pub(super) first: Option<usize>,
    pub(super) last: Option<usize>,
    pub(super) next: Option<usize>,
    pub(super) parent: Option<usize>,
    pub(super) previous: Option<usize>,
}

#[derive(Debug, Default)]
pub(super) struct Tree {
    pub(super) nodes: Vec<Node>,
}

impl Tree {
    fn create(&mut self, data: Data) -> usize {
        self.nodes.push(Node {
            content: None,
            data,
            first: None,
            last: None,
            next: None,
            parent: None,
            previous: None,
        });
        self.nodes.len() - 1
    }

    pub(super) fn detach(&mut self, child: usize) {
        let Some(parent) = self.nodes[child].parent else {
            return;
        };
        let previous = self.nodes[child].previous;
        let next = self.nodes[child].next;
        match previous {
            Some(previous) => self.nodes[previous].next = next,
            None => self.nodes[parent].first = next,
        }
        match next {
            Some(next) => self.nodes[next].previous = previous,
            None => self.nodes[parent].last = previous,
        }
        let node = &mut self.nodes[child];
        node.parent = None;
        node.previous = None;
        node.next = None;
    }

    fn append(&mut self, parent: usize, child: usize) {
        self.detach(child);
        let last = self.nodes[parent].last;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].previous = last;
        match last {
            Some(last) => self.nodes[last].next = Some(child),
            None => self.nodes[parent].first = Some(child),
        }
        self.nodes[parent].last = Some(child);
    }

    fn insert_before(&mut self, parent: usize, child: usize, reference: usize) {
        self.detach(child);
        let previous = self.nodes[reference].previous;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].previous = previous;
        self.nodes[child].next = Some(reference);
        self.nodes[reference].previous = Some(child);
        match previous {
            Some(previous) => self.nodes[previous].next = Some(child),
            None => self.nodes[parent].first = Some(child),
        }
    }

    pub(super) fn element(&self, node: usize) -> Option<(&str, Space)> {
        match &self.nodes[node].data {
            Data::Element { name, space, .. } => Some((name, *space)),
            _ => None,
        }
    }

    pub(super) fn html(&self, node: usize) -> Option<&str> {
        match self.element(node) {
            Some((name, Space::Html)) => Some(name),
            _ => None,
        }
    }

    fn is(&self, node: usize, name: &str) -> bool {
        self.html(node) == Some(name)
    }

    pub(super) fn attribute(&self, node: usize, key: &str) -> Option<&str> {
        match &self.nodes[node].data {
            Data::Element { attributes, .. } => attributes
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    pub(super) fn children(&self, node: usize) -> Vec<usize> {
        let mut children = Vec::new();
        let mut child = self.nodes[node].first;
        while let Some(current) = child {
            children.push(current);
            child = self.nodes[current].next;
        }
        children
    }

    pub(super) fn text_content(&self, node: usize) -> String {
        let mut out = String::new();
        self.collect_text(node, &mut out);
        out
    }

    fn collect_text(&self, node: usize, out: &mut String) {
        match &self.nodes[node].data {
            Data::Text(text) => out.push_str(text),
            Data::Element { .. } => {
                let mut child = self.nodes[node].first;
                while let Some(current) = child {
                    self.collect_text(current, out);
                    child = self.nodes[current].next;
                }
            }
            Data::Comment => {}
        }
    }

    fn insert_text(&mut self, parent: usize, text: &str) {
        if let Some(last) = self.nodes[parent].last
            && let Data::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            return;
        }
        let node = self.create(Data::Text(text.to_owned()));
        self.append(parent, node);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Body,
    Caption,
    Cell,
    ColumnGroup,
    Row,
    Select,
    SelectInTable,
    Table,
    TableBody,
    Template,
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    Plaintext,
    RawText,
    Rcdata,
}

enum Token {
    Comment,
    End(String),
    Start(String, Vec<(String, String)>, bool),
    Text(String),
}

#[derive(Clone)]
enum Entry {
    Bookmark,
    Element(usize, Vec<(String, String)>),
    Marker,
}

const SPECIAL: &[&str] = &[
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "base",
    "basefont",
    "bgsound",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "li",
    "link",
    "listing",
    "main",
    "marquee",
    "menu",
    "meta",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "ol",
    "p",
    "param",
    "plaintext",
    "pre",
    "script",
    "section",
    "select",
    "source",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
    "wbr",
    "xmp",
];
const SCOPE: &[&str] = &[
    "applet", "caption", "html", "table", "td", "th", "marquee", "object", "template",
];
const IMPLIED: &[&str] = &[
    "dd", "dt", "li", "menuitem", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
];
const THOROUGH: &[&str] = &[
    "caption", "colgroup", "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
    "tbody", "td", "tfoot", "th", "thead", "tr",
];
const TABLE_ROWS: &[&str] = &["table", "tbody", "tfoot", "thead", "tr"];
const FORMATTING: &[&str] = &[
    "b", "big", "code", "em", "font", "i", "s", "small", "strike", "strong", "tt", "u",
];
const BREAKOUT: &[&str] = &[
    "b",
    "big",
    "blockquote",
    "body",
    "br",
    "center",
    "code",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "hr",
    "i",
    "img",
    "li",
    "listing",
    "menu",
    "meta",
    "nobr",
    "ol",
    "p",
    "pre",
    "ruby",
    "s",
    "small",
    "span",
    "strong",
    "strike",
    "sub",
    "sup",
    "table",
    "tt",
    "u",
    "ul",
    "var",
];
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "header",
    "hgroup",
    "main",
    "nav",
    "ol",
    "p",
    "section",
    "summary",
    "ul",
];
const CLOSERS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "button",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "header",
    "hgroup",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "pre",
    "section",
    "summary",
    "ul",
];
const C1: [u32; 32] = [
    8364, 129, 8218, 402, 8222, 8230, 8224, 8225, 710, 8240, 352, 8249, 338, 141, 381, 143, 144,
    8216, 8217, 8220, 8221, 8226, 8211, 8212, 732, 8482, 353, 8250, 339, 157, 382, 376,
];

struct Parser<'a> {
    form: Option<usize>,
    formatting: Vec<Entry>,
    foster: bool,
    input: &'a str,
    mode: Mode,
    original: Mode,
    position: usize,
    skip_newline: bool,
    stack: Vec<usize>,
    state: State,
    templates: Vec<Mode>,
    text: String,
    tree: Tree,
    until: String,
}

pub(super) fn parse(html: &str) -> (Tree, usize) {
    let input = html.replace("\r\n", "\n").replace('\r', "\n");
    let mut tree = Tree::default();
    let root = tree.create(Data::Element {
        attributes: Vec::new(),
        name: "html".into(),
        space: Space::Html,
    });
    let head = tree.create(Data::Element {
        attributes: Vec::new(),
        name: "head".into(),
        space: Space::Html,
    });
    tree.append(root, head);
    let body = tree.create(Data::Element {
        attributes: Vec::new(),
        name: "body".into(),
        space: Space::Html,
    });
    tree.append(root, body);
    let mut parser = Parser {
        form: None,
        formatting: Vec::new(),
        foster: false,
        input: &input,
        mode: Mode::Body,
        original: Mode::Body,
        position: 0,
        skip_newline: false,
        stack: vec![root, body],
        state: State::Data,
        templates: Vec::new(),
        text: String::new(),
        tree,
        until: String::new(),
    };
    parser.dispatch(Token::Start(
        "x-turndown".into(),
        vec![("id".into(), "turndown-root".into())],
        false,
    ));
    let wrapper = parser.stack.last().copied().unwrap_or(body);
    parser.run();
    parser.input = "</x-turndown>";
    parser.position = 0;
    parser.run();
    (parser.tree, wrapper)
}

const fn whitespace(ch: char) -> bool {
    matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.input[self.position..]
    }

    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let mut text = std::mem::take(&mut self.text);
        if self.skip_newline {
            self.skip_newline = false;
            if text.starts_with('\n') {
                text.remove(0);
            }
            if text.is_empty() {
                return;
            }
        }
        self.dispatch(Token::Text(text));
    }

    fn emit(&mut self, token: Token) {
        self.flush();
        self.skip_newline = false;
        self.dispatch(token);
    }

    fn run(&mut self) {
        while self.position < self.input.len() {
            match self.state {
                State::Data => self.data(),
                State::Plaintext => {
                    let rest = self.rest().replace('\0', "\u{FFFD}");
                    self.text.push_str(&rest);
                    self.position = self.input.len();
                }
                State::RawText | State::Rcdata => self.raw(),
            }
        }
        self.flush();
    }

    fn raw(&mut self) {
        let rcdata = self.state == State::Rcdata;
        let rest = self.rest();
        let Some(at) = rest.find(['<', '&', '\0']) else {
            self.text.push_str(rest);
            self.position = self.input.len();
            return;
        };
        let (before, from) = rest.split_at(at);
        self.text.push_str(before);
        self.position += at;
        if from.starts_with('\0') {
            self.text.push('\u{FFFD}');
            self.position += 1;
            return;
        }
        if from.starts_with('&') {
            if rcdata {
                self.position += 1;
                self.reference(false);
            } else {
                self.text.push('&');
                self.position += 1;
            }
            return;
        }
        let name = &from[1..];
        let ends = name.strip_prefix('/').is_some_and(|tail| {
            tail.len() > self.until.len()
                && tail.as_bytes()[..self.until.len()].eq_ignore_ascii_case(self.until.as_bytes())
                && matches!(
                    tail.as_bytes()[self.until.len()],
                    b'\t' | b'\n' | b'\x0C' | b' ' | b'/' | b'>'
                )
        });
        if ends {
            self.position += 2;
            self.state = State::Data;
            self.tag(true);
        } else {
            self.text.push('<');
            self.position += 1;
        }
    }

    fn data(&mut self) {
        let rest = self.rest();
        let Some(at) = rest.find(['<', '&']) else {
            self.text.push_str(rest);
            self.position = self.input.len();
            return;
        };
        self.text.push_str(&rest[..at]);
        self.position += at;
        if self.rest().starts_with('&') {
            self.position += 1;
            self.reference(false);
            return;
        }
        let next = self.rest().as_bytes().get(1).copied();
        match next {
            Some(b'!') => {
                self.position += 2;
                self.declaration();
            }
            Some(b'/') => {
                let after = self.rest().as_bytes().get(2).copied();
                match after {
                    Some(byte) if byte.is_ascii_alphabetic() => {
                        self.position += 2;
                        self.tag(true);
                    }
                    Some(b'>') => self.position += 3,
                    None => {
                        self.text.push_str("</");
                        self.position += 2;
                    }
                    Some(_) => {
                        self.position += 2;
                        self.bogus();
                    }
                }
            }
            Some(byte) if byte.is_ascii_alphabetic() => {
                self.position += 1;
                self.tag(false);
            }
            Some(b'?') => {
                self.position += 1;
                self.bogus();
            }
            _ => {
                self.text.push('<');
                self.position += 1;
            }
        }
    }

    fn bogus(&mut self) {
        let rest = self.rest();
        self.position += rest.find('>').map_or(rest.len(), |end| end + 1);
        self.emit(Token::Comment);
    }

    fn declaration(&mut self) {
        let rest = self.rest();
        if let Some(comment) = rest.strip_prefix("--") {
            self.position += 2;
            if comment.starts_with('>') {
                self.position += 1;
            } else if comment.starts_with("->") {
                self.position += 2;
            } else {
                let end = [
                    comment.find("-->").map(|at| at + 3),
                    comment.find("--!>").map(|at| at + 4),
                ]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(comment.len());
                self.position += end;
            }
            self.emit(Token::Comment);
            return;
        }
        if rest.len() >= 7 && rest.as_bytes()[..7].eq_ignore_ascii_case(b"DOCTYPE") {
            self.position += rest.find('>').map_or(rest.len(), |end| end + 1);
            self.flush();
            return;
        }
        if let Some(cdata) = rest.strip_prefix("[CDATA[")
            && let Some(&top) = self.stack.last()
            && !matches!(self.tree.element(top), Some((_, Space::Html)) | None)
        {
            let end = cdata.find("]]>");
            self.text.push_str(&cdata[..end.unwrap_or(cdata.len())]);
            self.position += 7 + end.map_or(cdata.len(), |end| end + 3);
            return;
        }
        self.bogus();
    }

    fn tag(&mut self, end: bool) {
        let bytes = self.input.as_bytes();
        let mut name = String::new();
        let mut attributes: Vec<(String, String)> = Vec::new();
        let mut closing = false;
        let mut at = self.position;
        while at < bytes.len() && !matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ' | b'/' | b'>')
        {
            let ch = self.input[at..].chars().next().unwrap_or('\0');
            name.push(if ch == '\0' {
                '\u{FFFD}'
            } else {
                ch.to_ascii_lowercase()
            });
            at += ch.len_utf8();
        }
        loop {
            while at < bytes.len() && matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ') {
                at += 1;
            }
            if at >= bytes.len() {
                self.position = at;
                self.flush();
                return;
            }
            match bytes[at] {
                b'>' => {
                    at += 1;
                    break;
                }
                b'/' => {
                    at += 1;
                    if bytes.get(at) == Some(&b'>') {
                        closing = true;
                        at += 1;
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            let mut key = String::new();
            let first = at;
            while at < bytes.len()
                && !matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ' | b'/' | b'>')
                && (at == first || bytes[at] != b'=')
            {
                let ch = self.input[at..].chars().next().unwrap_or('\0');
                key.push(if ch == '\0' {
                    '\u{FFFD}'
                } else {
                    ch.to_ascii_lowercase()
                });
                at += ch.len_utf8();
            }
            while at < bytes.len() && matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ') {
                at += 1;
            }
            let mut value = String::new();
            if bytes.get(at) == Some(&b'=') {
                at += 1;
                while at < bytes.len() && matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ') {
                    at += 1;
                }
                match bytes.get(at) {
                    Some(&quote @ (b'"' | b'\'')) => {
                        at += 1;
                        let end = self.input[at..].find(quote as char).map(|end| at + end);
                        let Some(end) = end else {
                            self.position = bytes.len();
                            self.flush();
                            return;
                        };
                        value = attribute_value(&self.input[at..end]);
                        at = end + 1;
                    }
                    Some(b'>') => {}
                    _ => {
                        let start = at;
                        while at < bytes.len()
                            && !matches!(bytes[at], b'\t' | b'\n' | b'\x0C' | b' ' | b'>')
                        {
                            at += 1;
                        }
                        value = attribute_value(&self.input[start..at]);
                    }
                }
            }
            if !attributes.iter().any(|(existing, _)| *existing == key) {
                attributes.push((key, value));
            }
        }
        self.position = at;
        if end {
            self.emit(Token::End(name));
        } else {
            self.emit(Token::Start(name, attributes, closing));
        }
    }

    fn reference(&mut self, attribute: bool) {
        let rest = self.rest();
        let (text, used) = reference(rest, attribute);
        self.text.push_str(&text);
        self.position += used;
    }

    fn top(&self) -> usize {
        self.stack[self.stack.len() - 1]
    }

    fn dispatch(&mut self, token: Token) {
        let top = self.top();
        let Some((name, space)) = self.tree.element(top) else {
            return self.mode_token(token);
        };
        if space == Space::Html {
            return self.mode_token(token);
        }
        let text_point =
            space == Space::MathMl && matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext");
        let html_point = self.integration(top);
        let into = match &token {
            Token::Text(_) => text_point || html_point,
            Token::Start(tag, ..) => {
                (text_point && tag != "mglyph" && tag != "malignmark")
                    || (tag == "svg" && space == Space::MathMl && name == "annotation-xml")
                    || html_point
            }
            _ => false,
        };
        if into {
            self.mode_token(token);
        } else {
            self.foreign(token);
        }
    }

    fn integration(&self, node: usize) -> bool {
        match self.tree.element(node) {
            Some((name, Space::Svg)) => matches!(name, "foreignobject" | "desc" | "title"),
            Some(("annotation-xml", Space::MathMl)) => self
                .tree
                .attribute(node, "encoding")
                .map(str::to_ascii_lowercase)
                .is_some_and(|encoding| {
                    encoding == "text/html" || encoding == "application/xhtml+xml"
                }),
            _ => false,
        }
    }

    fn foreign(&mut self, token: Token) {
        match token {
            Token::Text(text) => {
                let text = text.replace('\0', "\u{FFFD}");
                let top = self.top();
                self.tree.insert_text(top, &text);
            }
            Token::Comment => {}
            Token::Start(name, attributes, closing) => {
                let breakout = BREAKOUT.contains(&name.as_str())
                    || (name == "font"
                        && attributes
                            .iter()
                            .any(|(key, _)| matches!(key.as_str(), "color" | "face" | "size")));
                if breakout {
                    loop {
                        self.stack.pop();
                        let top = self.top();
                        let text_point = matches!(
                            self.tree.element(top),
                            Some(("mi" | "mo" | "mn" | "ms" | "mtext", Space::MathMl))
                        );
                        if self.tree.html(top).is_some() || text_point || self.integration(top) {
                            break;
                        }
                    }
                    return self.dispatch(Token::Start(name, attributes, closing));
                }
                let space = self
                    .tree
                    .element(self.top())
                    .map_or(Space::Html, |(_, space)| space);
                self.insert(&name, attributes, space);
                if closing {
                    self.stack.pop();
                }
            }
            Token::End(name) => {
                let mut index = self.stack.len() - 1;
                loop {
                    let node = self.stack[index];
                    if self
                        .tree
                        .element(node)
                        .is_some_and(|(local, _)| local.eq_ignore_ascii_case(&name))
                    {
                        self.stack.truncate(index);
                        return;
                    }
                    if index == 0 {
                        return;
                    }
                    index -= 1;
                    if self.tree.html(self.stack[index]).is_some() {
                        return self.mode_token(Token::End(name));
                    }
                }
            }
        }
    }

    fn mode_token(&mut self, token: Token) {
        match self.mode {
            Mode::Body | Mode::Caption | Mode::Cell if matches!(token, Token::Text(_)) => {
                self.body(token)
            }
            Mode::Body => self.body(token),
            Mode::Caption => self.caption(token),
            Mode::Cell => self.cell(token),
            Mode::ColumnGroup => self.column_group(token),
            Mode::Row => self.row(token),
            Mode::Select => self.select(token),
            Mode::SelectInTable => self.select_in_table(token),
            Mode::Table => self.table(token),
            Mode::TableBody => self.table_body(token),
            Mode::Template => self.template(token),
            Mode::Text => match token {
                Token::Text(text) => {
                    let top = self.top();
                    self.tree.insert_text(top, &text);
                }
                Token::End(_) => {
                    self.stack.pop();
                    self.mode = self.original;
                }
                _ => {}
            },
        }
    }

    fn target(&mut self) -> (usize, Option<usize>) {
        let top = self.top();
        if self.foster
            && self
                .tree
                .html(top)
                .is_some_and(|name| TABLE_ROWS.contains(&name))
        {
            let table = self
                .stack
                .iter()
                .rposition(|&node| self.tree.is(node, "table"));
            let template = self
                .stack
                .iter()
                .rposition(|&node| self.tree.is(node, "template"));
            let (parent, before) = match (table, template) {
                (table, Some(template)) if table.is_none_or(|table| template > table) => {
                    (self.stack[template], None)
                }
                (Some(table), _) => match self.tree.nodes[self.stack[table]].parent {
                    Some(parent) => (parent, Some(self.stack[table])),
                    None => (self.stack[table - 1], None),
                },
                _ => (self.stack[0], None),
            };
            return (self.content(parent), before);
        }
        (self.content(top), None)
    }

    fn content(&mut self, node: usize) -> usize {
        if !self.tree.is(node, "template") {
            return node;
        }
        if let Some(content) = self.tree.nodes[node].content {
            return content;
        }
        let content = self.tree.create(Data::Comment);
        self.tree.nodes[node].content = Some(content);
        content
    }

    fn place(&mut self, node: usize) {
        let (parent, before) = self.target();
        match before {
            Some(before) => self.tree.insert_before(parent, node, before),
            None => self.tree.append(parent, node),
        }
    }

    fn insert(&mut self, name: &str, attributes: Vec<(String, String)>, space: Space) -> usize {
        let node = self.tree.create(Data::Element {
            attributes,
            name: name.to_owned(),
            space,
        });
        self.place(node);
        self.stack.push(node);
        node
    }

    fn html(&mut self, name: &str, attributes: Vec<(String, String)>) -> usize {
        self.insert(name, attributes, Space::Html)
    }

    fn void(&mut self, name: &str, attributes: Vec<(String, String)>) {
        self.html(name, attributes);
        self.stack.pop();
    }

    fn text(&mut self, text: &str) {
        let (parent, before) = self.target();
        match before {
            Some(before) => {
                if let Some(previous) = self.tree.nodes[before].previous
                    && let Data::Text(existing) = &mut self.tree.nodes[previous].data
                {
                    existing.push_str(text);
                    return;
                }
                let node = self.tree.create(Data::Text(text.to_owned()));
                self.tree.insert_before(parent, node, before);
            }
            None => self.tree.insert_text(parent, text),
        }
    }

    fn raw_element(&mut self, name: &str, attributes: Vec<(String, String)>, state: State) {
        self.html(name, attributes);
        self.state = state;
        self.until = name.to_owned();
        self.original = self.mode;
        self.mode = Mode::Text;
    }

    fn scoped(&self, matches: impl Fn(usize) -> bool, extra: &[&str]) -> bool {
        for &node in self.stack.iter().rev() {
            if matches(node) {
                return true;
            }
            let boundary = match self.tree.element(node) {
                Some((name, Space::Html)) => SCOPE.contains(&name) || extra.contains(&name),
                Some((name, Space::MathMl)) => {
                    matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml")
                }
                Some((name, Space::Svg)) => matches!(name, "foreignobject" | "desc" | "title"),
                None => false,
            };
            if boundary {
                return false;
            }
        }
        false
    }

    fn in_scope(&self, name: &str) -> bool {
        self.scoped(|node| self.tree.is(node, name), &[])
    }

    fn in_button_scope(&self, name: &str) -> bool {
        self.scoped(|node| self.tree.is(node, name), &["button"])
    }

    fn in_table_scope(&self, name: &str) -> bool {
        for &node in self.stack.iter().rev() {
            if self.tree.is(node, name) {
                return true;
            }
            if self
                .tree
                .html(node)
                .is_some_and(|tag| matches!(tag, "html" | "table" | "template"))
            {
                return false;
            }
        }
        false
    }

    fn contains(&self, name: &str) -> bool {
        self.stack.iter().any(|&node| self.tree.is(node, name))
    }

    fn implied(&mut self, except: Option<&str>, thorough: bool) {
        let list = if thorough { THOROUGH } else { IMPLIED };
        while let Some(&top) = self.stack.last() {
            let name = self.tree.html(top);
            if except.is_some() && name == except {
                break;
            }
            if !name.is_some_and(|name| list.contains(&name)) {
                break;
            }
            self.stack.pop();
        }
    }

    fn pop_tag(&mut self, name: &str) {
        let mut index = self.stack.len() - 1;
        while index > 0 && !self.tree.is(self.stack[index], name) {
            index -= 1;
        }
        self.stack.truncate(index);
    }

    fn pop_element(&mut self, node: usize) {
        let mut index = self.stack.len() - 1;
        while index > 0 && self.stack[index] != node {
            index -= 1;
        }
        self.stack.truncate(index);
    }

    fn remove_element(&mut self, node: usize) {
        if let Some(index) = self.stack.iter().rposition(|&open| open == node) {
            self.stack.remove(index);
        }
    }

    fn clear_to(&mut self, context: &[&str]) {
        let mut index = self.stack.len() - 1;
        while index > 0
            && !self
                .tree
                .html(self.stack[index])
                .is_some_and(|name| context.contains(&name))
        {
            index -= 1;
        }
        self.stack.truncate(index + 1);
    }

    fn close_p(&mut self) {
        if self.in_button_scope("p") {
            self.body(Token::End("p".into()));
        }
    }

    fn clear_to_marker(&mut self) {
        let index = self
            .formatting
            .iter()
            .rposition(|entry| matches!(entry, Entry::Marker))
            .unwrap_or(0);
        self.formatting.truncate(index);
    }

    fn find_formatting(&self, name: &str) -> Option<usize> {
        for (index, entry) in self.formatting.iter().enumerate().rev() {
            match entry {
                Entry::Marker => return None,
                Entry::Element(node, _) if self.tree.is(*node, name) => return Some(index),
                _ => {}
            }
        }
        None
    }

    fn formatting_index(&self, node: usize) -> Option<usize> {
        self.formatting
            .iter()
            .position(|entry| matches!(entry, Entry::Element(open, _) if *open == node))
    }

    fn push_formatting(&mut self, node: usize, attributes: Vec<(String, String)>) {
        let mut same = 0;
        for index in (0..self.formatting.len()).rev() {
            let Entry::Element(open, saved) = &self.formatting[index] else {
                if matches!(self.formatting[index], Entry::Marker) {
                    break;
                }
                continue;
            };
            let equal = self.tree.html(*open) == self.tree.html(node)
                && match &self.tree.nodes[*open].data {
                    Data::Element {
                        attributes: current,
                        ..
                    } => {
                        current.len() == attributes.len()
                            && attributes
                                .iter()
                                .all(|attribute| current.contains(attribute))
                    }
                    _ => false,
                };
            let _ = saved;
            if equal {
                same += 1;
                if same == 3 {
                    self.formatting.remove(index);
                    break;
                }
            }
        }
        self.formatting.push(Entry::Element(node, attributes));
    }

    fn recreate(&mut self, index: usize) -> usize {
        let Entry::Element(node, attributes) = self.formatting[index].clone() else {
            return 0;
        };
        let name = self.tree.html(node).unwrap_or_default().to_owned();
        self.tree.create(Data::Element {
            attributes,
            name,
            space: Space::Html,
        })
    }

    fn reconstruct(&mut self) {
        let Some(last) = self.formatting.last() else {
            return;
        };
        match last {
            Entry::Marker => return,
            Entry::Element(node, _) if self.stack.contains(node) => return,
            _ => {}
        }
        let mut index = self.formatting.len() - 1;
        while index > 0 {
            index -= 1;
            match &self.formatting[index] {
                Entry::Marker => {
                    index += 1;
                    break;
                }
                Entry::Element(node, _) if self.stack.contains(node) => {
                    index += 1;
                    break;
                }
                _ => {}
            }
            if index == 0 {
                break;
            }
        }
        for at in index..self.formatting.len() {
            let created = self.recreate(at);
            self.place(created);
            self.stack.push(created);
            if let Entry::Element(node, _) = &mut self.formatting[at] {
                *node = created;
            }
        }
    }

    fn adoption(&mut self, name: &str) -> bool {
        let top = self.top();
        if self.tree.is(top, name) && self.formatting_index(top).is_none() {
            self.stack.pop();
            return true;
        }
        for _ in 0..8 {
            let Some(entry) = self.find_formatting(name) else {
                return false;
            };
            let Entry::Element(formatting, _) = self.formatting[entry].clone() else {
                return false;
            };
            let Some(position) = self.stack.iter().rposition(|&node| node == formatting) else {
                self.formatting.remove(entry);
                return true;
            };
            if !self.scoped(|node| node == formatting, &[]) {
                return true;
            }
            let furthest = (position + 1..self.stack.len()).find(|&index| {
                self.tree
                    .html(self.stack[index])
                    .is_some_and(|tag| SPECIAL.contains(&tag))
                    || matches!(
                        self.tree.element(self.stack[index]),
                        Some((
                            "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml",
                            Space::MathMl
                        )) | Some(("foreignobject" | "desc" | "title", Space::Svg))
                    )
            });
            let Some(mut index) = furthest else {
                self.pop_element(formatting);
                self.formatting.remove(entry);
                return true;
            };
            let block = self.stack[index];
            let common = self.stack[position - 1];
            self.formatting.insert(entry + 1, Entry::Bookmark);
            let mut last = block;
            let mut count = 0;
            loop {
                count += 1;
                index -= 1;
                let mut node = self.stack[index];
                if node == formatting {
                    break;
                }
                let mut at = self.formatting_index(node);
                if count > 3
                    && let Some(found) = at
                {
                    self.formatting.remove(found);
                    at = None;
                }
                let Some(found) = at else {
                    self.remove_element(node);
                    continue;
                };
                let created = self.recreate(found);
                let Entry::Element(_, attributes) = self.formatting[found].clone() else {
                    continue;
                };
                self.formatting[found] = Entry::Element(created, attributes);
                self.stack[index] = created;
                node = created;
                if last == block {
                    if let Some(bookmark) = self
                        .formatting
                        .iter()
                        .position(|entry| matches!(entry, Entry::Bookmark))
                    {
                        self.formatting.remove(bookmark);
                    }
                    let after = self
                        .formatting_index(created)
                        .map_or(self.formatting.len(), |at| at + 1);
                    self.formatting.insert(after, Entry::Bookmark);
                }
                self.tree.append(node, last);
                last = node;
            }
            let foster = self.foster
                && self
                    .tree
                    .html(common)
                    .is_some_and(|tag| TABLE_ROWS.contains(&tag));
            if foster {
                self.tree.detach(last);
                let saved = self.stack.clone();
                self.stack.push(common);
                let (parent, before) = self.target();
                self.stack = saved;
                match before {
                    Some(before) => self.tree.insert_before(parent, last, before),
                    None => self.tree.append(parent, last),
                }
            } else {
                let parent = self.content(common);
                self.tree.append(parent, last);
            }
            let Some(entry) = self.formatting_index(formatting) else {
                return true;
            };
            let created = self.recreate(entry);
            while let Some(child) = self.tree.nodes[block].first {
                self.tree.append(created, child);
            }
            self.tree.append(block, created);
            let Entry::Element(_, attributes) = self.formatting[entry].clone() else {
                return true;
            };
            self.formatting.remove(entry);
            if let Some(bookmark) = self
                .formatting
                .iter()
                .position(|entry| matches!(entry, Entry::Bookmark))
            {
                self.formatting[bookmark] = Entry::Element(created, attributes);
            }
            self.remove_element(formatting);
            let at = self
                .stack
                .iter()
                .rposition(|&node| node == block)
                .map_or(self.stack.len(), |at| at + 1);
            self.stack.insert(at, created);
        }
        true
    }

    fn reset(&mut self) {
        for index in (0..self.stack.len()).rev() {
            let node = self.stack[index];
            let last = index == 0;
            if let Some(name) = self.tree.html(node) {
                self.mode = match name {
                    "select" => {
                        let in_table = self.stack[..index]
                            .iter()
                            .rev()
                            .take_while(|&&open| !self.tree.is(open, "template"))
                            .any(|&open| self.tree.is(open, "table"));
                        if in_table {
                            Mode::SelectInTable
                        } else {
                            Mode::Select
                        }
                    }
                    "tr" => Mode::Row,
                    "tbody" | "tfoot" | "thead" => Mode::TableBody,
                    "caption" => Mode::Caption,
                    "colgroup" => Mode::ColumnGroup,
                    "table" => Mode::Table,
                    "template" => self.templates.last().copied().unwrap_or(Mode::Body),
                    "body" | "html" => Mode::Body,
                    "td" | "th" if !last => Mode::Cell,
                    _ if last => Mode::Body,
                    _ => continue,
                };
                return;
            }
            if last {
                self.mode = Mode::Body;
                return;
            }
        }
    }

    fn head(&mut self, token: Token) {
        match token {
            Token::Start(name, attributes, _) => match name.as_str() {
                "meta" | "base" | "basefont" | "bgsound" | "link" => self.void(&name, attributes),
                "title" => self.raw_element(&name, attributes, State::Rcdata),
                "noscript" | "noframes" | "style" | "script" => {
                    self.raw_element(&name, attributes, State::RawText)
                }
                "template" => {
                    self.html(&name, attributes);
                    self.formatting.push(Entry::Marker);
                    self.mode = Mode::Template;
                    self.templates.push(Mode::Template);
                }
                _ => {}
            },
            Token::End(name) if name == "template" => {
                if !self.contains("template") {
                    return;
                }
                self.implied(None, true);
                self.pop_tag("template");
                self.clear_to_marker();
                self.templates.pop();
                self.reset();
            }
            _ => {}
        }
    }

    fn body(&mut self, token: Token) {
        match token {
            Token::Text(text) => {
                let text = text.replace('\0', "");
                if text.is_empty() {
                    return;
                }
                self.reconstruct();
                self.text(&text);
            }
            Token::Comment => {
                let (parent, before) = self.target();
                let node = self.tree.create(Data::Comment);
                match before {
                    Some(before) => self.tree.insert_before(parent, node, before),
                    None => self.tree.append(parent, node),
                }
            }
            Token::Start(name, attributes, closing) => self.body_start(name, attributes, closing),
            Token::End(name) => self.body_end(name),
        }
    }

    fn body_start(&mut self, name: String, attributes: Vec<(String, String)>, closing: bool) {
        match name.as_str() {
            "html" | "body" | "frameset" | "caption" | "col" | "colgroup" | "frame" | "head"
            | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr" => {}
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style"
            | "template" | "title" => self.head(Token::Start(name, attributes, closing)),
            "menu" => {
                self.close_p();
                if self.tree.is(self.top(), "menuitem") {
                    self.stack.pop();
                }
                self.html(&name, attributes);
            }
            tag if BLOCKS.contains(&tag) => {
                self.close_p();
                self.html(&name, attributes);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.close_p();
                if self
                    .tree
                    .html(self.top())
                    .is_some_and(|tag| matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
                {
                    self.stack.pop();
                }
                self.html(&name, attributes);
            }
            "pre" | "listing" => {
                self.close_p();
                self.html(&name, attributes);
                self.skip_newline = true;
            }
            "form" => {
                if self.form.is_some() && !self.contains("template") {
                    return;
                }
                self.close_p();
                let node = self.html(&name, attributes);
                if !self.contains("template") {
                    self.form = Some(node);
                }
            }
            "li" | "dd" | "dt" => {
                for index in (0..self.stack.len()).rev() {
                    let node = self.stack[index];
                    let tag = self.tree.html(node);
                    let hit = match name.as_str() {
                        "li" => tag == Some("li"),
                        _ => matches!(tag, Some("dd" | "dt")),
                    };
                    if hit {
                        let tag = tag.unwrap_or_default().to_owned();
                        self.body_end(tag);
                        break;
                    }
                    let special = tag.is_some_and(|tag| SPECIAL.contains(&tag))
                        || (tag.is_none() && self.integration(node));
                    if special && !matches!(tag, Some("address" | "div" | "p")) {
                        break;
                    }
                }
                self.close_p();
                self.html(&name, attributes);
            }
            "plaintext" => {
                self.close_p();
                self.html(&name, attributes);
                self.state = State::Plaintext;
            }
            "button" => {
                if self.in_scope("button") {
                    self.body_end("button".into());
                    self.mode_token(Token::Start(name, attributes, closing));
                } else {
                    self.reconstruct();
                    self.html(&name, attributes);
                }
            }
            "a" | "nobr" => {
                if name == "a" {
                    if let Some(entry) = self.find_formatting("a")
                        && let Entry::Element(node, _) = self.formatting[entry].clone()
                    {
                        self.body_end("a".into());
                        if let Some(index) = self.formatting_index(node) {
                            self.formatting.remove(index);
                        }
                        self.remove_element(node);
                    }
                    self.reconstruct();
                } else {
                    self.reconstruct();
                    if self.in_scope("nobr") {
                        self.body_end("nobr".into());
                        self.reconstruct();
                    }
                }
                let node = self.html(&name, attributes.clone());
                self.push_formatting(node, attributes);
            }
            tag if FORMATTING.contains(&tag) => {
                self.reconstruct();
                let node = self.html(&name, attributes.clone());
                self.push_formatting(node, attributes);
            }
            "applet" | "marquee" | "object" => {
                self.reconstruct();
                self.html(&name, attributes);
                self.formatting.push(Entry::Marker);
            }
            "table" => {
                self.html(&name, attributes);
                self.mode = Mode::Table;
            }
            "area" | "br" | "embed" | "img" | "keygen" | "wbr" | "input" => {
                self.reconstruct();
                self.void(&name, attributes);
            }
            "param" | "source" | "track" => self.void(&name, attributes),
            "hr" => {
                self.close_p();
                if self.tree.is(self.top(), "menuitem") {
                    self.stack.pop();
                }
                self.void(&name, attributes);
            }
            "image" => self.body_start("img".into(), attributes, closing),
            "textarea" => {
                self.raw_element(&name, attributes, State::Rcdata);
                self.skip_newline = true;
            }
            "xmp" => {
                self.close_p();
                self.reconstruct();
                self.raw_element(&name, attributes, State::RawText);
            }
            "iframe" | "noembed" | "noscript" => {
                self.raw_element(&name, attributes, State::RawText)
            }
            "select" => {
                self.reconstruct();
                self.html(&name, attributes);
                self.mode = if matches!(
                    self.mode,
                    Mode::Table | Mode::Caption | Mode::TableBody | Mode::Row | Mode::Cell
                ) {
                    Mode::SelectInTable
                } else {
                    Mode::Select
                };
            }
            "optgroup" | "option" => {
                if self.tree.is(self.top(), "option") {
                    self.body_end("option".into());
                }
                self.reconstruct();
                self.html(&name, attributes);
            }
            "menuitem" => {
                if self.tree.is(self.top(), "menuitem") {
                    self.stack.pop();
                }
                self.reconstruct();
                self.html(&name, attributes);
            }
            "rb" | "rtc" => {
                if self.in_scope("ruby") {
                    self.implied(None, false);
                }
                self.html(&name, attributes);
            }
            "rp" | "rt" => {
                if self.in_scope("ruby") {
                    self.implied(Some("rtc"), false);
                }
                self.html(&name, attributes);
            }
            "math" | "svg" => {
                self.reconstruct();
                let space = if name == "math" {
                    Space::MathMl
                } else {
                    Space::Svg
                };
                self.insert(&name, attributes, space);
                if closing {
                    self.stack.pop();
                }
            }
            _ => {
                self.reconstruct();
                self.html(&name, attributes);
            }
        }
    }

    fn body_end(&mut self, name: String) {
        match name.as_str() {
            "template" => self.head(Token::End(name)),
            "body" | "html" => {}
            tag if CLOSERS.contains(&tag) => {
                if !self.in_scope(tag) {
                    return;
                }
                self.implied(None, false);
                self.pop_tag(tag);
            }
            "form" => {
                if !self.contains("template") {
                    let form = self.form.take();
                    let Some(form) = form.filter(|&form| self.scoped(|node| node == form, &[]))
                    else {
                        return;
                    };
                    self.implied(None, false);
                    self.remove_element(form);
                } else {
                    if !self.in_scope("form") {
                        return;
                    }
                    self.implied(None, false);
                    self.pop_tag("form");
                }
            }
            "p" => {
                if !self.in_button_scope("p") {
                    self.body_start("p".into(), Vec::new(), false);
                    self.mode_token(Token::End(name));
                } else {
                    self.implied(Some("p"), false);
                    self.pop_tag("p");
                }
            }
            "li" => {
                if !self.scoped(|node| self.tree.is(node, "li"), &["ol", "ul"]) {
                    return;
                }
                self.implied(Some("li"), false);
                self.pop_tag("li");
            }
            "dd" | "dt" => {
                if !self.in_scope(&name) {
                    return;
                }
                self.implied(Some(&name), false);
                self.pop_tag(&name);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let heading = |tree: &Tree, node: usize| {
                    tree.html(node)
                        .is_some_and(|tag| matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
                };
                if !self.scoped(|node| heading(&self.tree, node), &[]) {
                    return;
                }
                self.implied(None, false);
                let mut index = self.stack.len() - 1;
                while index > 0 && !heading(&self.tree, self.stack[index]) {
                    index -= 1;
                }
                self.stack.truncate(index);
            }
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small"
            | "strike" | "strong" | "tt" | "u"
                if self.adoption(&name) => {}
            "applet" | "marquee" | "object" => {
                if !self.in_scope(&name) {
                    return;
                }
                self.implied(None, false);
                self.pop_tag(&name);
                self.clear_to_marker();
            }
            "br" => self.body_start("br".into(), Vec::new(), false),
            _ => {
                for index in (0..self.stack.len()).rev() {
                    let node = self.stack[index];
                    if self.tree.is(node, &name) {
                        self.implied(Some(&name), false);
                        self.pop_element(node);
                        return;
                    }
                    let tag = self.tree.html(node);
                    if tag.is_some_and(|tag| SPECIAL.contains(&tag))
                        || (tag.is_none() && self.integration(node))
                        || matches!(
                            self.tree.element(node),
                            Some(("mi" | "mo" | "mn" | "ms" | "mtext", Space::MathMl))
                        )
                    {
                        return;
                    }
                }
            }
        }
    }

    fn anything_in_table(&mut self, token: Token) {
        self.foster = true;
        self.body(token);
        self.foster = false;
    }

    fn table(&mut self, token: Token) {
        match token {
            Token::Text(text) => {
                if !self
                    .tree
                    .html(self.top())
                    .is_some_and(|tag| TABLE_ROWS.contains(&tag))
                {
                    return self.anything_in_table(Token::Text(text));
                }
                let text = text.replace('\0', "");
                if text.is_empty() {
                    return;
                }
                if text.chars().any(|ch| !whitespace(ch)) {
                    self.anything_in_table(Token::Text(text));
                } else {
                    let top = self.top();
                    self.tree.insert_text(top, &text);
                }
            }
            Token::Comment => self.body(Token::Comment),
            Token::Start(name, attributes, closing) => match name.as_str() {
                "caption" => {
                    self.clear_to(&["table", "template", "html"]);
                    self.formatting.push(Entry::Marker);
                    self.html(&name, attributes);
                    self.mode = Mode::Caption;
                }
                "colgroup" => {
                    self.clear_to(&["table", "template", "html"]);
                    self.html(&name, attributes);
                    self.mode = Mode::ColumnGroup;
                }
                "col" => {
                    self.table(Token::Start("colgroup".into(), Vec::new(), false));
                    self.mode_token(Token::Start(name, attributes, closing));
                }
                "tbody" | "tfoot" | "thead" => {
                    self.clear_to(&["table", "template", "html"]);
                    self.html(&name, attributes);
                    self.mode = Mode::TableBody;
                }
                "td" | "th" | "tr" => {
                    self.table(Token::Start("tbody".into(), Vec::new(), false));
                    self.mode_token(Token::Start(name, attributes, closing));
                }
                "table" => {
                    if !self.in_table_scope("table") {
                        return;
                    }
                    self.table(Token::End("table".into()));
                    self.mode_token(Token::Start(name, attributes, closing));
                }
                "style" | "script" | "template" => {
                    self.head(Token::Start(name, attributes, closing))
                }
                "input"
                    if attributes
                        .iter()
                        .find(|(key, _)| key == "type")
                        .is_some_and(|(_, value)| value.eq_ignore_ascii_case("hidden")) =>
                {
                    self.void(&name, attributes);
                }
                "form" => {
                    if self.form.is_some() || self.contains("template") {
                        return;
                    }
                    let node = self.html(&name, attributes);
                    self.form = Some(node);
                    self.pop_element(node);
                }
                _ => self.anything_in_table(Token::Start(name, attributes, closing)),
            },
            Token::End(name) => match name.as_str() {
                "table" => {
                    if !self.in_table_scope("table") {
                        return;
                    }
                    self.pop_tag("table");
                    self.reset();
                }
                "body" | "caption" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot"
                | "th" | "thead" | "tr" => {}
                "template" => self.head(Token::End(name)),
                _ => self.anything_in_table(Token::End(name)),
            },
        }
    }

    fn caption(&mut self, token: Token) {
        let close = |parser: &mut Self| {
            if !parser.in_table_scope("caption") {
                return false;
            }
            parser.implied(None, false);
            parser.pop_tag("caption");
            parser.clear_to_marker();
            parser.mode = Mode::Table;
            true
        };
        match &token {
            Token::Start(name, ..)
                if matches!(
                    name.as_str(),
                    "caption"
                        | "col"
                        | "colgroup"
                        | "tbody"
                        | "td"
                        | "tfoot"
                        | "th"
                        | "thead"
                        | "tr"
                ) =>
            {
                if close(self) {
                    self.mode_token(token);
                }
            }
            Token::End(name) if name == "caption" => {
                close(self);
            }
            Token::End(name) if name == "table" => {
                if close(self) {
                    self.mode_token(token);
                }
            }
            Token::End(name)
                if matches!(
                    name.as_str(),
                    "body"
                        | "col"
                        | "colgroup"
                        | "html"
                        | "tbody"
                        | "td"
                        | "tfoot"
                        | "th"
                        | "thead"
                        | "tr"
                ) => {}
            _ => self.body(token),
        }
    }

    fn column_group(&mut self, token: Token) {
        match token {
            Token::Text(text) => {
                let rest = text.trim_start_matches(whitespace);
                let leading = &text[..text.len() - rest.len()];
                if !leading.is_empty() {
                    self.text(leading);
                }
                if rest.is_empty() {
                    return;
                }
                let rest = rest.to_owned();
                self.leave_column_group(Token::Text(rest));
            }
            Token::Comment => self.body(Token::Comment),
            Token::Start(name, attributes, closing) => match name.as_str() {
                "html" => self.body(Token::Start(name, attributes, closing)),
                "col" => self.void(&name, attributes),
                "template" => self.head(Token::Start(name, attributes, closing)),
                _ => self.leave_column_group(Token::Start(name, attributes, closing)),
            },
            Token::End(name) => match name.as_str() {
                "colgroup" => {
                    if self.tree.is(self.top(), "colgroup") {
                        self.stack.pop();
                        self.mode = Mode::Table;
                    }
                }
                "col" => {}
                "template" => self.head(Token::End(name)),
                _ => self.leave_column_group(Token::End(name)),
            },
        }
    }

    fn leave_column_group(&mut self, token: Token) {
        if !self.tree.is(self.top(), "colgroup") {
            return;
        }
        self.stack.pop();
        self.mode = Mode::Table;
        self.mode_token(token);
    }

    fn table_body(&mut self, token: Token) {
        let leave = |parser: &mut Self, token: Token| {
            if !parser.in_table_scope("tbody")
                && !parser.in_table_scope("thead")
                && !parser.in_table_scope("tfoot")
            {
                return;
            }
            parser.clear_to(&["tbody", "tfoot", "thead", "template", "html"]);
            parser.stack.pop();
            parser.mode = Mode::Table;
            parser.mode_token(token);
        };
        match token {
            Token::Start(name, attributes, closing) => match name.as_str() {
                "tr" => {
                    self.clear_to(&["tbody", "tfoot", "thead", "template", "html"]);
                    self.html(&name, attributes);
                    self.mode = Mode::Row;
                }
                "th" | "td" => {
                    self.table_body(Token::Start("tr".into(), Vec::new(), false));
                    self.mode_token(Token::Start(name, attributes, closing));
                }
                "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" => {
                    leave(self, Token::Start(name, attributes, closing))
                }
                _ => self.table(Token::Start(name, attributes, closing)),
            },
            Token::End(name) => match name.as_str() {
                "table" => leave(self, Token::End(name)),
                "tbody" | "tfoot" | "thead" => {
                    if self.in_table_scope(&name) {
                        self.clear_to(&["tbody", "tfoot", "thead", "template", "html"]);
                        self.stack.pop();
                        self.mode = Mode::Table;
                    }
                }
                "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" | "tr" => {}
                _ => self.table(Token::End(name)),
            },
            token => self.table(token),
        }
    }

    fn close_row(&mut self) -> bool {
        if !self.in_table_scope("tr") {
            return false;
        }
        self.clear_to(&["tr", "template", "html"]);
        self.stack.pop();
        self.mode = Mode::TableBody;
        true
    }

    fn row(&mut self, token: Token) {
        match token {
            Token::Start(name, attributes, closing) => match name.as_str() {
                "th" | "td" => {
                    self.clear_to(&["tr", "template", "html"]);
                    self.html(&name, attributes);
                    self.mode = Mode::Cell;
                    self.formatting.push(Entry::Marker);
                }
                "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" | "tr" => {
                    if self.close_row() {
                        self.mode_token(Token::Start(name, attributes, closing));
                    }
                }
                _ => self.table(Token::Start(name, attributes, closing)),
            },
            Token::End(name) => match name.as_str() {
                "tr" => {
                    self.close_row();
                }
                "table" => {
                    if self.close_row() {
                        self.mode_token(Token::End(name));
                    }
                }
                "tbody" | "tfoot" | "thead" => {
                    if self.in_table_scope(&name) && self.close_row() {
                        self.mode_token(Token::End(name));
                    }
                }
                "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" => {}
                _ => self.table(Token::End(name)),
            },
            token => self.table(token),
        }
    }

    fn close_cell(&mut self) {
        let name = if self.in_table_scope("td") {
            "td"
        } else {
            "th"
        };
        self.cell(Token::End(name.into()));
    }

    fn cell(&mut self, token: Token) {
        match &token {
            Token::Start(name, ..)
                if matches!(
                    name.as_str(),
                    "caption"
                        | "col"
                        | "colgroup"
                        | "tbody"
                        | "td"
                        | "tfoot"
                        | "th"
                        | "thead"
                        | "tr"
                ) =>
            {
                if self.in_table_scope("td") || self.in_table_scope("th") {
                    self.close_cell();
                    self.mode_token(token);
                }
            }
            Token::End(name) if matches!(name.as_str(), "td" | "th") => {
                if !self.in_table_scope(name) {
                    return;
                }
                self.implied(None, false);
                self.pop_tag(name);
                self.clear_to_marker();
                self.mode = Mode::Row;
            }
            Token::End(name)
                if matches!(
                    name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html"
                ) => {}
            Token::End(name)
                if matches!(name.as_str(), "table" | "tbody" | "tfoot" | "thead" | "tr") =>
            {
                if !self.in_table_scope(name) {
                    return;
                }
                self.close_cell();
                self.mode_token(token);
            }
            _ => self.body(token),
        }
    }

    fn select(&mut self, token: Token) {
        match token {
            Token::Text(text) => {
                let text = text.replace('\0', "");
                if !text.is_empty() {
                    self.text(&text);
                }
            }
            Token::Comment => self.body(Token::Comment),
            Token::Start(name, attributes, closing) => match name.as_str() {
                "html" => self.body(Token::Start(name, attributes, closing)),
                "option" => {
                    if self.tree.is(self.top(), "option") {
                        self.select(Token::End(name.clone()));
                    }
                    self.html(&name, attributes);
                }
                "optgroup" => {
                    if self.tree.is(self.top(), "option") {
                        self.select(Token::End("option".into()));
                    }
                    if self.tree.is(self.top(), "optgroup") {
                        self.select(Token::End(name.clone()));
                    }
                    self.html(&name, attributes);
                }
                "select" => self.select(Token::End(name)),
                "input" | "keygen" | "textarea" => {
                    if !self.in_select_scope() {
                        return;
                    }
                    self.select(Token::End("select".into()));
                    self.mode_token(Token::Start(name, attributes, closing));
                }
                "script" | "template" => self.head(Token::Start(name, attributes, closing)),
                _ => {}
            },
            Token::End(name) => match name.as_str() {
                "optgroup" => {
                    let length = self.stack.len();
                    if self.tree.is(self.top(), "option")
                        && length > 1
                        && self.tree.is(self.stack[length - 2], "optgroup")
                    {
                        self.select(Token::End("option".into()));
                    }
                    if self.tree.is(self.top(), "optgroup") {
                        self.stack.pop();
                    }
                }
                "option" => {
                    if self.tree.is(self.top(), "option") {
                        self.stack.pop();
                    }
                }
                "select" => {
                    if !self.in_select_scope() {
                        return;
                    }
                    self.pop_tag("select");
                    self.reset();
                }
                "template" => self.head(Token::End(name)),
                _ => {}
            },
        }
    }

    fn in_select_scope(&self) -> bool {
        for &node in self.stack.iter().rev() {
            match self.tree.html(node) {
                Some("select") => return true,
                Some("optgroup" | "option") => {}
                _ => return false,
            }
        }
        false
    }

    fn select_in_table(&mut self, token: Token) {
        let table = |name: &str| {
            matches!(
                name,
                "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
            )
        };
        match token {
            Token::Start(name, attributes, closing) if table(&name) => {
                self.select(Token::End("select".into()));
                self.mode_token(Token::Start(name, attributes, closing));
            }
            Token::End(name) if table(&name) => {
                if self.in_table_scope(&name) {
                    self.select(Token::End("select".into()));
                    self.mode_token(Token::End(name));
                }
            }
            token => self.select(token),
        }
    }

    fn template(&mut self, token: Token) {
        match token {
            Token::Text(_) | Token::Comment => self.body(token),
            Token::Start(name, attributes, closing) => {
                let mode = match name.as_str() {
                    "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script"
                    | "style" | "template" | "title" => {
                        return self.head(Token::Start(name, attributes, closing));
                    }
                    "caption" | "colgroup" | "tbody" | "tfoot" | "thead" => Mode::Table,
                    "col" => Mode::ColumnGroup,
                    "tr" => Mode::TableBody,
                    "td" | "th" => Mode::Row,
                    _ => Mode::Body,
                };
                self.mode = mode;
                if let Some(last) = self.templates.last_mut() {
                    *last = mode;
                }
                self.mode_token(Token::Start(name, attributes, closing));
            }
            Token::End(name) if name == "template" => self.head(Token::End(name)),
            Token::End(_) => {}
        }
    }
}

fn attribute_value(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(at) = rest.find(['&', '\0']) {
        out.push_str(&rest[..at]);
        if rest[at..].starts_with('\0') {
            out.push('\u{FFFD}');
            rest = &rest[at + 1..];
            continue;
        }
        let (text, used) = reference(&rest[at + 1..], true);
        out.push_str(&text);
        rest = &rest[at + 1 + used..];
    }
    out.push_str(rest);
    out
}

fn reference(rest: &str, attribute: bool) -> (String, usize) {
    let bytes = rest.as_bytes();
    match bytes.first() {
        None | Some(b'\t' | b'\n' | b'\x0C' | b' ' | b'<' | b'&') => ("&".into(), 0),
        Some(b'#') => {
            let hex = matches!(bytes.get(1), Some(b'x' | b'X'));
            let start = if hex { 2 } else { 1 };
            let digits = bytes[start..]
                .iter()
                .take_while(|byte| {
                    if hex {
                        byte.is_ascii_hexdigit()
                    } else {
                        byte.is_ascii_digit()
                    }
                })
                .count();
            if digits == 0 {
                return ("&".into(), 0);
            }
            let text = &rest[start..start + digits];
            let value = u32::from_str_radix(text, if hex { 16 } else { 10 }).unwrap_or(u32::MAX);
            let mut used = start + digits;
            if bytes.get(used) == Some(&b';') {
                used += 1;
            }
            let ch = match value {
                0 => '\u{FFFD}',
                0x80..=0x9F => char::from_u32(C1[(value - 0x80) as usize]).unwrap_or('\u{FFFD}'),
                _ => char::from_u32(value).unwrap_or('\u{FFFD}'),
            };
            (ch.to_string(), used)
        }
        Some(_) => {
            let limit = rest.len().min(32);
            let mut found = None;
            for length in (1..=limit).rev() {
                if !rest.is_char_boundary(length) {
                    continue;
                }
                if let Ok(index) = ENTITIES.binary_search_by(|(name, _)| name.cmp(&&rest[..length]))
                {
                    found = Some((length, ENTITIES[index].1));
                    break;
                }
            }
            let Some((length, value)) = found else {
                return ("&".into(), 0);
            };
            if attribute
                && !rest[..length].ends_with(';')
                && bytes
                    .get(length)
                    .is_some_and(|byte| *byte == b'=' || byte.is_ascii_alphanumeric())
            {
                return (format!("&{}", &rest[..length]), length);
            }
            (value.to_owned(), length)
        }
    }
}
