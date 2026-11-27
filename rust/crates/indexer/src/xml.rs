//! Minimal XML DOM parsing with quick-xml, ported from the XDocument-based parsing in the
//! v1 `NewznabClient`. DTDs are prohibited (a `DOCTYPE` is a format error; quick-xml never
//! resolves external entities), matching `XmlReaderSettings.DtdProcessing = Prohibit`.
//! Predefined entities and numeric character references (`&#x27;`, `&amp;`) resolve
//! exactly like .NET `XmlReader` does; any other named entity is undeclared and a format
//! error. Namespace prefixes are tracked manually so `newznab:attr` elements resolve to
//! the Newznab attribute namespace exactly like `XNamespace` matching in v1.

use std::borrow::Cow;

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;

/// The Newznab feed attribute namespace (`http://www.newznab.com/DTD/2010/feeds/attributes/`).
pub(crate) const NEWZNAB_NS: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";

/// One XML element: local name, resolved namespace, attributes (local name, value),
/// child elements, and concatenated text content.
#[derive(Debug, Clone)]
pub(crate) struct Element {
    pub local: String,
    pub ns: Option<String>,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Self>,
    pub text: String,
}

impl Element {
    /// Attribute by exact (case-sensitive, no-namespace) name, like `XElement.Attribute`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// First child element with no namespace and the exact local name, like
    /// `XElement.Element(name)`.
    pub fn child(&self, local: &str) -> Option<&Self> {
        self.children
            .iter()
            .find(|c| c.ns.is_none() && c.local == local)
    }

    /// Child elements with no namespace and the exact local name (`Elements(name)`).
    pub fn children_named<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Self> + 'a {
        self.children
            .iter()
            .filter(move |c| c.ns.is_none() && c.local == local)
    }

    /// First child element whose local name matches case-insensitively (any namespace),
    /// with trimmed, non-empty text — the v1 `GetElementText` fallback elements.
    pub fn child_text_ci(&self, local: &str) -> Option<String> {
        self.children
            .iter()
            .find(|c| c.local.eq_ignore_ascii_case(local))
            .and_then(|c| {
                let v = c.text.trim();
                if v.is_empty() {
                    None
                } else {
                    Some(v.to_owned())
                }
            })
    }

    /// Newznab attribute elements (`newznab:attr` in the attribute namespace).
    pub fn newznab_attrs(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.children
            .iter()
            .filter(|c| c.ns.as_deref() == Some(NEWZNAB_NS) && c.local == "attr")
            .filter_map(|c| match (c.attr("name"), c.attr("value")) {
                (Some(n), Some(v)) => Some((n, v)),
                _ => None,
            })
    }

    pub fn has_descendant_named_ci(&self, local: &str) -> bool {
        self.local.eq_ignore_ascii_case(local)
            || self
                .children
                .iter()
                .any(|c| c.has_descendant_named_ci(local))
    }
}

type Bindings = Vec<(Option<String>, String)>;

fn resolve_ns(prefix: Option<&str>, frames: &[Bindings]) -> Option<String> {
    let prefix = prefix.map(str::to_owned);
    for frame in frames.iter().rev() {
        for (p, uri) in frame {
            if *p == prefix {
                return Some(uri.clone());
            }
        }
    }
    None
}

fn split_name(raw: &str) -> (Option<&str>, &str) {
    match raw.bytes().position(|b| b == b':') {
        Some(i) => (Some(&raw[..i]), &raw[i + 1..]),
        None => (None, raw),
    }
}

/// Parses the body into the root element. Any XML error or a DOCTYPE fails with the
/// byte offset of the error in the body; no body content is ever included.
pub(crate) fn parse(body: &[u8]) -> Result<Element, u64> {
    let mut reader = Reader::from_reader(body);
    let mut stack: Vec<Element> = Vec::new();
    let mut frames: Vec<Bindings> = Vec::new();
    let mut root: Option<Element> = None;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|_| reader.error_position())?;
        match event {
            Event::Start(e) => {
                let qname = e.name();
                let name: &str = qname.as_ref();
                let (prefix, local) = split_name(name);
                let local = local.to_owned();
                let ns = resolve_ns(prefix, &frames);
                let mut el = Element {
                    local,
                    ns,
                    attrs: Vec::new(),
                    children: Vec::new(),
                    text: String::new(),
                };
                let mut bindings: Bindings = Vec::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|_| reader.error_position())?;
                    let key: &str = attr.key.as_ref();
                    let key = key.to_owned();
                    let Some(value) = normalize_attr_value(&attr.value) else {
                        return Err(reader.error_position());
                    };
                    let value = value.into_owned();
                    if key == "xmlns" {
                        bindings.push((None, value));
                    } else if let Some(p) = key.strip_prefix("xmlns:") {
                        bindings.push((Some(p.to_owned()), value));
                    } else {
                        el.attrs.push((key, value));
                    }
                }
                frames.push(bindings);
                stack.push(el);
            }
            Event::Empty(e) => {
                let qname = e.name();
                let name: &str = qname.as_ref();
                let (prefix, local) = split_name(name);
                let local = local.to_owned();
                let ns = resolve_ns(prefix, &frames);
                let mut el = Element {
                    local,
                    ns,
                    attrs: Vec::new(),
                    children: Vec::new(),
                    text: String::new(),
                };
                for attr in e.attributes() {
                    let attr = attr.map_err(|_| reader.error_position())?;
                    let key: &str = attr.key.as_ref();
                    let key = key.to_owned();
                    let Some(value) = normalize_attr_value(&attr.value) else {
                        return Err(reader.error_position());
                    };
                    let value = value.into_owned();
                    if key == "xmlns" || key.starts_with("xmlns:") {
                        // bindings on an empty element do not leak to siblings
                    } else {
                        el.attrs.push((key, value));
                    }
                }
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                } else if root.is_none() {
                    root = Some(el);
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    let text: &str = t.as_ref();
                    top.text.push_str(text);
                }
            }
            Event::End(_) => {
                frames.pop();
                if let Some(el) = stack.pop() {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(el);
                    } else {
                        root = Some(el);
                    }
                }
            }
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
            // A DOCTYPE is a format error: DTDs are prohibited and no external resolver
            // is ever touched. quick-xml splits every `&...;` in text into a GeneralRef
            // event, so references are resolved here (character references and the five
            // predefined entities are legal without a DTD, like in .NET; anything else is
            // an undeclared entity and a format error).
            Event::DocType(_) => return Err(reader.buffer_position()),
            Event::GeneralRef(r) => match resolve_ref(&r) {
                Some(text) => {
                    if let Some(top) = stack.last_mut() {
                        top.text.push_str(&text);
                    }
                }
                None => return Err(reader.buffer_position()),
            },
            Event::Eof => break,
        }
        buf.clear();
    }
    root.ok_or(body.len() as u64)
}

/// Resolves one entity/character reference name (the part between `&` and `;`) to its
/// text. `&#39;` / `&#x27;` and the predefined `amp`/`lt`/`gt`/`apos`/`quot` resolve;
/// anything else is an undeclared entity (DTDs prohibited) and returns `None`.
fn resolve_ref(name: &str) -> Option<String> {
    if let Some(num) = name.strip_prefix('#') {
        parse_char_ref(num).map(|ch| ch.to_string())
    } else {
        resolve_predefined_entity(name).map(str::to_owned)
    }
}

/// Parses the numeric part of `&#...;` / `&#x...;` and validates it as an XML 1.0
/// legal character (WFC: Legal Character). Only a lowercase `x` introduces hex,
/// matching quick-xml and the XML spec.
fn parse_char_ref(num: &str) -> Option<char> {
    let code = if let Some(hex) = num.strip_prefix('x') {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        num.parse::<u32>().ok()?
    };
    let ch = char::from_u32(code)?;
    is_legal_xml10_char(ch).then_some(ch)
}

/// XML 1.0 [WFC: Legal Character](https://www.w3.org/TR/xml/#wf-Legalchar).
fn is_legal_xml10_char(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
    )
}

/// XML 1.0 attribute-value normalization with entity resolution, matching .NET
/// `XmlReader`: literal tab/CR/LF become spaces, predefined entities and numeric
/// character references resolve, any other reference is an undeclared entity and
/// returns `None` (DTDs prohibited). Character references to whitespace keep the
/// referenced character (only literal whitespace is normalized).
fn normalize_attr_value(raw: &str) -> Option<Cow<'_, str>> {
    if !raw
        .as_bytes()
        .iter()
        .any(|&b| matches!(b, b'&' | b'\t' | b'\r' | b'\n'))
    {
        return Some(Cow::Borrowed(raw));
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find(['&', '\t', '\r', '\n']) {
        let (head, tail) = rest.split_at(i);
        push_attr_text(&mut out, head);
        if let Some(after) = tail.strip_prefix('&') {
            let j = after.find(';')?; // dangling '&' is ill-formed
            let name = &after[..j];
            out.push_str(&resolve_ref(name)?);
            rest = &after[j + 1..];
        } else {
            out.push(' ');
            rest = &tail[1..];
        }
    }
    push_attr_text(&mut out, rest);
    Some(Cow::Owned(out))
}

fn push_attr_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\t' | '\r' | '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
}
