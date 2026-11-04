//! Generates interface tables and opcode constants from `protocols/*.xml`.

use std::fmt::Write as _;
use std::path::Path;

struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Element>,
}

impl Element {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
}

/// Parses the XML subset used by Wayland protocol files: elements, attributes,
/// comments and processing instructions. Text content is skipped.
fn parse_xml(src: &str) -> Element {
    let bytes = src.as_bytes();
    let mut stack = vec![Element { name: String::new(), attrs: Vec::new(), children: Vec::new() }];
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").expect("unterminated comment") + 3;
        } else if rest.starts_with("<?") {
            i += rest.find("?>").expect("unterminated processing instruction") + 2;
        } else if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>").expect("unterminated CDATA") + 3;
        } else if rest.starts_with("</") {
            i += rest.find('>').expect("unterminated closing tag") + 1;
            let done = stack.pop().expect("unbalanced XML");
            stack.last_mut().expect("unbalanced XML").children.push(done);
        } else {
            let end = rest.find('>').expect("unterminated tag");
            let mut tag = &rest[1..end];
            let self_closing = tag.ends_with('/');
            if self_closing {
                tag = &tag[..tag.len() - 1];
            }
            let name_end = tag.find(char::is_whitespace).unwrap_or(tag.len());
            let mut el = Element { name: tag[..name_end].to_string(), attrs: Vec::new(), children: Vec::new() };
            let mut a = tag[name_end..].trim_start();
            while let Some(eq) = a.find('=') {
                let key = a[..eq].trim().to_string();
                let after = a[eq + 1..].trim_start();
                let quote = after.chars().next().expect("attribute without value");
                let close = after[1..].find(quote).expect("unterminated attribute") + 1;
                el.attrs.push((key, after[1..close].to_string()));
                a = after[close + 1..].trim_start();
            }
            if self_closing {
                stack.last_mut().unwrap().children.push(el);
            } else {
                stack.push(el);
            }
            i += end + 1;
        }
    }
    assert_eq!(stack.len(), 1, "unbalanced XML");
    stack.pop().unwrap()
}

fn const_name(name: &str) -> String {
    let upper = name.to_uppercase();
    if upper.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{upper}")
    } else {
        upper
    }
}

fn write_messages(out: &mut String, messages: &[&Element]) {
    out.push_str("&[");
    for m in messages {
        let since = m.attr("since").unwrap_or("1");
        let destructor = m.attr("type") == Some("destructor");
        write!(out, "MessageDesc {{ name: {:?}, since: {since}, destructor: {destructor}, args: &[", m.attr("name").unwrap()).unwrap();
        for a in m.children("arg") {
            let kind = match a.attr("type").unwrap() {
                "int" => "Int",
                "uint" => "Uint",
                "fixed" => "Fixed",
                "string" => "Str",
                "object" => "Object",
                "new_id" => "NewId",
                "array" => "Array",
                "fd" => "Fd",
                t => panic!("unknown arg type {t}"),
            };
            let iface = match a.attr("interface") {
                Some(i) => format!("Some({i:?})"),
                None => "None".to_string(),
            };
            let nullable = a.attr("allow-null") == Some("true");
            write!(out, "ArgDesc {{ name: {:?}, kind: ArgKind::{kind}, interface: {iface}, nullable: {nullable} }}, ", a.attr("name").unwrap()).unwrap();
        }
        out.push_str("] }, ");
    }
    out.push(']');
}

fn write_opcodes(out: &mut String, module: &str, messages: &[&Element]) {
    writeln!(out, "    pub mod {module} {{").unwrap();
    for (i, m) in messages.iter().enumerate() {
        writeln!(out, "        pub const {}: u16 = {i};", const_name(m.attr("name").unwrap())).unwrap();
    }
    out.push_str("    }\n");
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocols");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();

    let mut out = String::new();
    let mut names = Vec::new();
    for file in files.iter().filter(|f| f.extension().is_some_and(|e| e == "xml")) {
        println!("cargo:rerun-if-changed={}", file.display());
        let root = parse_xml(&std::fs::read_to_string(file).unwrap());
        for protocol in root.children("protocol") {
            for iface in protocol.children("interface") {
                let name = iface.attr("name").unwrap();
                let requests: Vec<_> = iface.children("request").collect();
                let events: Vec<_> = iface.children("event").collect();
                writeln!(out, "pub mod {name} {{\n    use crate::*;").unwrap();
                write!(out, "    pub static INTERFACE: Interface = Interface {{ name: {name:?}, version: {}, requests: ", iface.attr("version").unwrap()).unwrap();
                write_messages(&mut out, &requests);
                out.push_str(", events: ");
                write_messages(&mut out, &events);
                out.push_str(" };\n");
                write_opcodes(&mut out, "req", &requests);
                write_opcodes(&mut out, "evt", &events);
                for e in iface.children("enum") {
                    writeln!(out, "    pub mod {} {{", e.attr("name").unwrap()).unwrap();
                    for entry in e.children("entry") {
                        let value = entry.attr("value").unwrap();
                        writeln!(out, "        pub const {}: u32 = {value};", const_name(entry.attr("name").unwrap())).unwrap();
                    }
                    out.push_str("    }\n");
                }
                out.push_str("}\n");
                names.push(name.to_string());
            }
        }
    }
    out.push_str("pub static ALL: &[&Interface] = &[");
    for n in &names {
        write!(out, "&{n}::INTERFACE, ").unwrap();
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("protocols.rs");
    std::fs::write(dest, out).unwrap();
}
