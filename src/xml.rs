//! A small XML tree: enough for device descriptions, SOAP answers, GENA
//! events and DIDL-Lite. Names are kept without their namespace prefix.

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The trimmed text of a child, `None` when it is missing or empty.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim()).filter(|t| !t.is_empty())
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The first descendant with this name, depth first.
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(n) = c.find(name) {
                return Some(n);
            }
        }
        None
    }
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, l)) => l.to_string(),
        None => s.into_owned(),
    }
}

fn element(e: &quick_xml::events::BytesStart) -> Node {
    let attrs = e
        .attributes()
        .flatten()
        .map(|a| {
            let v = a.unescape_value().map(|v| v.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            (local(a.key.as_ref()), v)
        })
        .collect();
    Node { name: local(e.name().as_ref()), attrs, ..Default::default() }
}

fn parse_strict(s: &str) -> Option<Node> {
    let mut r = Reader::from_str(s);
    let mut stack: Vec<Node> = vec![Node::default()];
    loop {
        match r.read_event().ok()? {
            Event::Start(e) => stack.push(element(&e)),
            Event::Empty(e) => {
                let n = element(&e);
                stack.last_mut()?.children.push(n);
            }
            Event::End(_) => {
                let n = stack.pop()?;
                stack.last_mut()?.children.push(n);
            }
            Event::Text(t) => {
                let t = t.unescape().ok()?;
                stack.last_mut()?.text.push_str(&t);
            }
            Event::CData(t) => stack.last_mut()?.text.push_str(&String::from_utf8_lossy(&t.into_inner())),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return None;
    }
    stack.pop()?.children.into_iter().next()
}

/// The document element. Devices sometimes send a bare `&` in titles, so a
/// document that does not parse is tried again with those escaped.
pub fn parse(s: &str) -> Option<Node> {
    parse_strict(s).or_else(|| parse_strict(&escape_bare_amps(s)))
}

fn escape_bare_amps(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.char_indices() {
        if ch == '&' {
            let rest = &s[i + 1..];
            let entity = rest.find(';').is_some_and(|end| {
                let name = &rest[..end];
                end > 0 && end < 10 && (name.starts_with('#') || name.chars().all(|c| c.is_ascii_alphanumeric()))
            });
            out.push_str(if entity { "&" } else { "&amp;" });
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_without_prefix() {
        let n = parse(r#"<s:Envelope xmlns:s="x"><s:Body><u:R xmlns:u="y"><A>1 &amp; 2</A><B val="x"/></u:R></s:Body></s:Envelope>"#).unwrap();
        assert_eq!(n.name, "Envelope");
        let r = n.find("R").unwrap();
        assert_eq!(r.text_of("A"), Some("1 & 2"));
        assert_eq!(r.child("B").unwrap().attr("val"), Some("x"));
    }

    #[test]
    fn bare_ampersand() {
        let n = parse("<a><t>Tom & Jerry &amp; co &#38;</t></a>").unwrap();
        assert_eq!(n.text_of("t"), Some("Tom & Jerry & co &"));
    }
}
