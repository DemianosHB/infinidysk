//! Minimal XML DOM parsing with quick-xml, ported from the XDocument-based parsing in the
//! v1 `NewznabClient`. DTDs are prohibited (a `DOCTYPE` is a format error; quick-xml never
//! resolves external entities), matching `XmlReaderSettings.DtdProcessing = Prohibit`.
//! Namespace prefixes are tracked manually so `newznab:attr` elements resolve to the
//! Newznab attribute namespace exactly like `XNamespace` matching in v1.

use quick_xml::Reader;
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
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| reader.error_position())?
                        .into_owned();
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
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| reader.error_position())?
                        .into_owned();
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
            // A DOCTYPE or an unresolved entity reference: with DTDs prohibited both are
            // format errors and no external resolver is ever touched.
            Event::DocType(_) | Event::GeneralRef(_) => return Err(reader.buffer_position()),
            Event::Eof => break,
        }
        buf.clear();
    }
    root.ok_or(body.len() as u64)
}
