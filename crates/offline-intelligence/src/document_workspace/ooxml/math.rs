//! Equations: a small linear notation to Office Math (OMML) and back.
//!
//! # The notation
//!
//! A LaTeX-flavoured subset, because it is what people already type:
//!
//! - `x^{2}` / `x^2` superscript, `a_{i}` / `a_i` subscript, `x_{i}^{2}` both
//! - `\frac{a}{b}` fraction, `\sqrt{x}` and `\sqrt[3]{x}` radicals
//! - `\alpha`, `\sum`, `\int`, `\pm`, `\le`, `\infty`… as their symbols
//! - `{…}` groups
//!
//! Anything else is kept as literal math text, so nothing a user types is lost.
//!
//! # Reading back
//!
//! `to_linear` walks the OMML written by Word or by us and produces the same
//! notation, so an equation can be re-edited. Structures we do not model
//! (matrices, n-ary with limits, accents) fall back to their text content.

use super::spans::{escape, scan, unescape, XmlScan};

pub const MATH_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"), ("beta", "β"), ("gamma", "γ"), ("delta", "δ"), ("epsilon", "ε"),
    ("zeta", "ζ"), ("eta", "η"), ("theta", "θ"), ("iota", "ι"), ("kappa", "κ"),
    ("lambda", "λ"), ("mu", "μ"), ("nu", "ν"), ("xi", "ξ"), ("pi", "π"), ("rho", "ρ"),
    ("sigma", "σ"), ("tau", "τ"), ("upsilon", "υ"), ("phi", "φ"), ("chi", "χ"),
    ("psi", "ψ"), ("omega", "ω"), ("Gamma", "Γ"), ("Delta", "Δ"), ("Theta", "Θ"),
    ("Lambda", "Λ"), ("Xi", "Ξ"), ("Pi", "Π"), ("Sigma", "Σ"), ("Phi", "Φ"),
    ("Psi", "Ψ"), ("Omega", "Ω"), ("sum", "∑"), ("prod", "∏"), ("int", "∫"),
    ("oint", "∮"), ("partial", "∂"), ("nabla", "∇"), ("infty", "∞"), ("pm", "±"),
    ("mp", "∓"), ("times", "×"), ("div", "÷"), ("cdot", "·"), ("le", "≤"),
    ("leq", "≤"), ("ge", "≥"), ("geq", "≥"), ("ne", "≠"), ("neq", "≠"),
    ("approx", "≈"), ("equiv", "≡"), ("propto", "∝"), ("in", "∈"), ("notin", "∉"),
    ("subset", "⊂"), ("supset", "⊃"), ("cup", "∪"), ("cap", "∩"), ("forall", "∀"),
    ("exists", "∃"), ("rightarrow", "→"), ("leftarrow", "←"), ("Rightarrow", "⇒"),
    ("Leftrightarrow", "⇔"), ("degree", "°"), ("therefore", "∴"),
];

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Sup(Vec<Node>, Vec<Node>),
    Sub(Vec<Node>, Vec<Node>),
    SubSup(Vec<Node>, Vec<Node>, Vec<Node>),
    Frac(Vec<Node>, Vec<Node>),
    Rad(Option<Vec<Node>>, Vec<Node>),
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    _src: &'a str,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { chars: src.chars().collect(), pos: 0, _src: src }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// A sequence until `}` or `]` (when `stop_bracket`) or the end.
    fn sequence(&mut self, stop_bracket: bool) -> Vec<Node> {
        let mut out: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            if c == '}' || (stop_bracket && c == ']') {
                break;
            }
            match c {
                '^' | '_' => {
                    self.pos += 1;
                    let script = self.argument();
                    let base = out.pop().map(|n| vec![n]).unwrap_or_default();
                    out.push(attach(base, c, script));
                }
                '{' => {
                    self.pos += 1;
                    let group = self.sequence(false);
                    if self.peek() == Some('}') {
                        self.pos += 1;
                    }
                    // A group becomes one base for a following script.
                    out.push(group_node(group));
                }
                '\\' => {
                    self.pos += 1;
                    let name: String = self.take_word();
                    match name.as_str() {
                        "frac" => {
                            let num = self.argument();
                            let den = self.argument();
                            out.push(Node::Frac(num, den));
                        }
                        "sqrt" => {
                            let degree = if self.peek() == Some('[') {
                                self.pos += 1;
                                let d = self.sequence(true);
                                if self.peek() == Some(']') {
                                    self.pos += 1;
                                }
                                Some(d)
                            } else {
                                None
                            };
                            let body = self.argument();
                            out.push(Node::Rad(degree, body));
                        }
                        "" => {
                            // `\` followed by a non-letter is that literal char.
                            if let Some(ch) = self.peek() {
                                self.pos += 1;
                                push_text(&mut out, &ch.to_string());
                            }
                        }
                        other => {
                            let sym = SYMBOLS
                                .iter()
                                .find(|(k, _)| *k == other)
                                .map(|(_, v)| v.to_string())
                                .unwrap_or_else(|| format!("\\{}", other));
                            // Symbols stand alone so a script binds to them.
                            out.push(Node::Text(sym));
                        }
                    }
                }
                _ => {
                    self.pos += 1;
                    // Letters and digits group into one run; operators stand
                    // alone, so `x^2` scripts only the `x` in `ax^2`.
                    if c.is_alphanumeric() || c == '.' {
                        match out.last_mut() {
                            Some(Node::Text(t))
                                if t.chars().last().map(|l| l.is_alphanumeric() || l == '.').unwrap_or(false)
                                    && !(c.is_alphabetic()) =>
                            {
                                t.push(c)
                            }
                            _ => out.push(Node::Text(c.to_string())),
                        }
                    } else {
                        out.push(Node::Text(c.to_string()));
                    }
                }
            }
        }
        out
    }

    /// A script or macro argument: a `{group}` or a single character.
    fn argument(&mut self) -> Vec<Node> {
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                let g = self.sequence(false);
                if self.peek() == Some('}') {
                    self.pos += 1;
                }
                g
            }
            Some('\\') => {
                // A single macro, e.g. `x^\alpha`.
                let start = self.pos;
                self.pos += 1;
                let name = self.take_word();
                let sym = SYMBOLS.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
                match sym {
                    Some(s) => vec![Node::Text(s)],
                    None => {
                        self.pos = start + 1;
                        vec![Node::Text("\\".into())]
                    }
                }
            }
            Some(c) => {
                self.pos += 1;
                vec![Node::Text(c.to_string())]
            }
            None => Vec::new(),
        }
    }

    fn take_word(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                name.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        name
    }
}

fn push_text(out: &mut Vec<Node>, s: &str) {
    match out.last_mut() {
        Some(Node::Text(t)) => t.push_str(s),
        _ => out.push(Node::Text(s.to_string())),
    }
}

/// Wrap a group so it can act as one base. A single node needs no wrapper.
fn group_node(mut group: Vec<Node>) -> Node {
    if group.len() == 1 {
        return group.pop().unwrap();
    }
    // Represent a multi-node group as a subscript-less "sup" would lose it, so
    // flatten text-only groups and otherwise keep the first structure.
    let all_text = group.iter().all(|n| matches!(n, Node::Text(_)));
    if all_text {
        Node::Text(group.into_iter().map(|n| if let Node::Text(t) = n { t } else { String::new() }).collect())
    } else {
        // Frac/Rad inside a group: wrap as a radical-free grouping by using a
        // sup with an empty script is wrong; instead keep a frac-of-one. The
        // simplest faithful representation is a Sub with an empty subscript,
        // which OMML renders as just the base.
        Node::Sub(group, Vec::new())
    }
}

fn attach(base: Vec<Node>, op: char, script: Vec<Node>) -> Node {
    // `x_{i}^{2}`: a sub followed by a sup becomes one sub-sup.
    if op == '^' {
        if let [Node::Sub(b, s)] = base.as_slice() {
            if !s.is_empty() {
                return Node::SubSup(b.clone(), s.clone(), script);
            }
        }
        Node::Sup(base, script)
    } else {
        Node::Sub(base, script)
    }
}

fn omml_nodes(nodes: &[Node]) -> String {
    nodes.iter().map(omml_node).collect()
}

fn omml_node(node: &Node) -> String {
    match node {
        Node::Text(t) => format!("<m:r><m:t xml:space=\"preserve\">{}</m:t></m:r>", escape(t)),
        Node::Sup(b, s) => format!(
            "<m:sSup><m:e>{}</m:e><m:sup>{}</m:sup></m:sSup>",
            omml_nodes(b),
            omml_nodes(s)
        ),
        Node::Sub(b, s) if s.is_empty() => omml_nodes(b),
        Node::Sub(b, s) => format!(
            "<m:sSub><m:e>{}</m:e><m:sub>{}</m:sub></m:sSub>",
            omml_nodes(b),
            omml_nodes(s)
        ),
        Node::SubSup(b, sub, sup) => format!(
            "<m:sSubSup><m:e>{}</m:e><m:sub>{}</m:sub><m:sup>{}</m:sup></m:sSubSup>",
            omml_nodes(b),
            omml_nodes(sub),
            omml_nodes(sup)
        ),
        Node::Frac(n, d) => format!(
            "<m:f><m:num>{}</m:num><m:den>{}</m:den></m:f>",
            omml_nodes(n),
            omml_nodes(d)
        ),
        Node::Rad(None, body) => format!(
            "<m:rad><m:radPr><m:degHide m:val=\"1\"/></m:radPr><m:deg/><m:e>{}</m:e></m:rad>",
            omml_nodes(body)
        ),
        Node::Rad(Some(deg), body) => format!(
            "<m:rad><m:deg>{}</m:deg><m:e>{}</m:e></m:rad>",
            omml_nodes(deg),
            omml_nodes(body)
        ),
    }
}

/// The content of an `m:oMath` element for a linear expression.
pub fn to_omml(linear: &str) -> String {
    let mut parser = Parser::new(linear);
    let mut nodes = Vec::new();
    loop {
        nodes.extend(parser.sequence(false));
        // A stray closing brace is literal text rather than the end of input.
        match parser.peek() {
            Some(c) => {
                parser.pos += 1;
                push_text(&mut nodes, &c.to_string());
            }
            None => break,
        }
    }
    omml_nodes(&nodes)
}

/// A whole display-equation paragraph body, declaring its own namespace so it
/// is valid in documents whose root does not declare `m`.
pub fn display_equation(linear: &str) -> String {
    format!(
        "<m:oMathPara xmlns:m=\"{}\"><m:oMath>{}</m:oMath></m:oMathPara>",
        MATH_NS,
        to_omml(linear)
    )
}

// ------------------------------------------------------------------ reading

/// Linear notation for an `oMath` (or `oMathPara`) element in `source`.
pub fn to_linear(source: &str, s: &XmlScan, root: usize) -> String {
    let mut out = String::new();
    walk(source, s, root, &mut out);
    out.trim().to_string()
}

fn children(s: &XmlScan, parent: usize) -> Vec<usize> {
    s.elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.parent == Some(parent))
        .map(|(i, _)| i)
        .collect()
}

fn child(s: &XmlScan, parent: usize, local: &str) -> Option<usize> {
    children(s, parent).into_iter().find(|&i| s.elements[i].local == local)
}

fn group(source: &str, s: &XmlScan, el: Option<usize>) -> String {
    let mut inner = String::new();
    if let Some(e) = el {
        walk(source, s, e, &mut inner);
    }
    let symbolic = inner.chars().count() == 1 || inner.chars().all(|c| c.is_alphanumeric());
    if symbolic && !inner.is_empty() {
        inner
    } else {
        format!("{{{}}}", inner)
    }
}

fn walk(source: &str, s: &XmlScan, el: usize, out: &mut String) {
    let e = &s.elements[el];
    // Only math-namespace structure is interpreted; `w:rPr` and friends inside
    // an equation carry no text.
    match e.local.as_str() {
        "t" => {
            let text = unescape(e.inner.of(source));
            for ch in text.chars() {
                match SYMBOLS.iter().find(|(_, v)| v.chars().next() == Some(ch) && v.chars().count() == 1) {
                    // Keep Greek letters and operators readable as typed.
                    Some((k, _)) if !ch.is_ascii() => {
                        out.push('\\');
                        out.push_str(k);
                        out.push(' ');
                    }
                    _ => out.push(ch),
                }
            }
        }
        "sSup" => {
            out.push_str(&group(source, s, child(s, el, "e")));
            out.push('^');
            out.push_str(&group(source, s, child(s, el, "sup")));
        }
        "sSub" => {
            out.push_str(&group(source, s, child(s, el, "e")));
            out.push('_');
            out.push_str(&group(source, s, child(s, el, "sub")));
        }
        "sSubSup" => {
            out.push_str(&group(source, s, child(s, el, "e")));
            out.push('_');
            out.push_str(&group(source, s, child(s, el, "sub")));
            out.push('^');
            out.push_str(&group(source, s, child(s, el, "sup")));
        }
        "f" => {
            let mut num = String::new();
            let mut den = String::new();
            if let Some(n) = child(s, el, "num") {
                walk(source, s, n, &mut num);
            }
            if let Some(d) = child(s, el, "den") {
                walk(source, s, d, &mut den);
            }
            out.push_str(&format!("\\frac{{{}}}{{{}}}", num, den));
        }
        "rad" => {
            let mut deg = String::new();
            if let Some(d) = child(s, el, "deg") {
                walk(source, s, d, &mut deg);
            }
            let mut body = String::new();
            if let Some(b) = child(s, el, "e") {
                walk(source, s, b, &mut body);
            }
            if deg.is_empty() {
                out.push_str(&format!("\\sqrt{{{}}}", body));
            } else {
                out.push_str(&format!("\\sqrt[{}]{{{}}}", deg, body));
            }
        }
        // Property blocks hold no content.
        "rPr" | "ctrlPr" | "sSupPr" | "sSubPr" | "sSubSupPr" | "fPr" | "radPr" | "oMathParaPr" => {}
        _ => {
            for c in children(s, el) {
                walk(source, s, c, out);
            }
        }
    }
}

/// Convenience for tests and callers holding only a string.
pub fn linear_of(omml_fragment: &str) -> String {
    let wrapped = format!("<m:oMath xmlns:m=\"{}\">{}</m:oMath>", MATH_NS, omml_fragment);
    let Ok(s) = scan(&wrapped) else { return String::new() };
    to_linear(&wrapped, &s, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superscripts_fractions_and_roots_become_structure() {
        let x = to_omml("x^{2}+\\frac{a}{b}-\\sqrt{y}");
        assert!(x.contains("<m:sSup>"), "{}", x);
        assert!(x.contains("<m:f>"), "{}", x);
        assert!(x.contains("<m:degHide"), "{}", x);
    }

    #[test]
    fn symbols_are_written_as_their_characters() {
        assert!(to_omml("\\alpha+\\beta").contains("α"));
    }

    #[test]
    fn an_equation_reads_back_as_the_notation_it_was_written_in() {
        assert_eq!(linear_of(&to_omml("\\frac{a}{b}")), "\\frac{a}{b}");
        assert_eq!(linear_of(&to_omml("x^{2}")), "x^2");
        assert_eq!(linear_of(&to_omml("\\sqrt[3]{x}")), "\\sqrt[3]{x}");
    }

    #[test]
    fn unbalanced_input_is_kept_not_dropped() {
        let x = to_omml("a}b");
        assert!(x.contains("a}") && x.contains(">b<"), "{}", x);
    }
}
