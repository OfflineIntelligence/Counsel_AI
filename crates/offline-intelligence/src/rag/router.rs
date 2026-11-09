//! Classifies a question so exact lookups (a page, a clause, a defined term)
//! never depend on search ranking.

use lazy_static::lazy_static;
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Pages `from..=to`, optionally in a named file.
    Page { from: i32, to: i32, file_hint: Option<String> },
    /// A numbered section/clause/article, e.g. "12.3(b)".
    Section { number_key: String, file_hint: Option<String> },
    /// A defined term.
    Definition { term: String },
    /// Every relevant document should be represented.
    Coverage,
    /// Hybrid semantic + keyword search.
    Semantic,
}

lazy_static! {
    static ref PAGE: Regex = Regex::new(
        r"(?i)(?:\bpages?|\bpp?\.|\bpgs?\.?|\bpágina?s?|\bpagina?s?|\bseiten?|\bстраниц[аеуы]|\bстр\.|صفحة)\s*(\d{1,5})(?:\s*(?:-|–|—|to|a|bis|al|à|до|through)\s*(\d{1,5}))?\s*(?:页|頁)?"
    ).unwrap();
    static ref PAGE_CJK_SUFFIX: Regex = Regex::new(r"(\d{1,5})\s*(?:页|頁|ページ)").unwrap();
    static ref SECTION: Regex = Regex::new(
        r"(?i)(?:\b(?:section|sections|clause|clauses|article|articles|art\.|paragraph|para\.|sección|seccion|cláusula|clausula|artículo|articulo|abschnitt|klausel|artikel|articolo|clausola|artigo|schedule|annex|anexo|anlage|статья|пункт|खंड|धारा|अनुच्छेद)|§)\s*([ivxlcdm\d]+(?:\.[ivxlcdm\d]+)*(?:\s*\([a-z0-9]{1,4}\))*)"
    ).unwrap();
    static ref SECTION_CJK: Regex = Regex::new(r"第([一二三四五六七八九十百千零〇0-9]+)[条章款]").unwrap();
    static ref DEFINITION: Regex = Regex::new(
        r#"(?i)(?:what\s+(?:does|do|is)\s+(?:the\s+(?:term|phrase)\s+)?["“']?([^"”'?]{2,60}?)["”']?\s+(?:mean|means|refer\s+to|defined\s+as)|(?:definition|meaning)\s+of\s+(?:the\s+term\s+)?["“']?([^"”'?]{2,60}?)["”']?(?:\s+in\b|\?|$)|\bdefine\s+["“']?([^"”'?]{2,60}?)["”']?(?:\s+in\b|\?|$)|qu[ée]\s+significa\s+["“']?([^"”'?]{2,60}?)["”']?(?:\?|$)|que\s+veut\s+dire\s+["“']?([^"”'?]{2,60}?)["”']?(?:\?|$))"#
    ).unwrap();
    static ref COVERAGE: Regex = Regex::new(
        r"(?i)\b(summari[sz]e|summary|overview|outline|compare|comparison|contrast|all\s+(?:the\s+)?(?:documents|files|agreements|contracts)|each\s+(?:document|file|agreement|contract)|every\s+(?:document|file|agreement|contract)|both\s+(?:documents|files|agreements|contracts)|resum(?:e|ir|en)|zusammenfass|comparar|r[ée]sum[ée])"
    ).unwrap();
    static ref FILE_HINT: Regex =
        Regex::new(r"(?i)([\w][\w\-.]{0,120}?\.(?:pdf|docx?|xlsx?|pptx?|txt|rtf|odt|html?|md|csv))\b").unwrap();
}

pub fn file_hint(query: &str) -> Option<String> {
    FILE_HINT.captures(query).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string())
}

pub fn classify(query: &str, known_terms: &[String]) -> Route {
    let hint = file_hint(query);

    if let Some(caps) = PAGE.captures(query) {
        if let Some(from) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            let to = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()).unwrap_or(from);
            let (from, to) = if to < from { (to, from) } else { (from, to) };
            // "page 1 to 400" is not a lookup; keep ranges readable.
            if from >= 1 && to - from <= 10 {
                return Route::Page { from, to, file_hint: hint };
            }
        }
    }
    if let Some(caps) = PAGE_CJK_SUFFIX.captures(query) {
        if let Some(p) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return Route::Page { from: p, to: p, file_hint: hint };
        }
    }

    if let Some(caps) = SECTION.captures(query) {
        if let Some(num) = caps.get(1) {
            let key = super::chunker::section_number_key(num.as_str());
            if !key.is_empty() && key.chars().any(|c| c.is_ascii_digit()) {
                return Route::Section { number_key: key, file_hint: hint };
            }
        }
    }
    if let Some(caps) = SECTION_CJK.captures(query) {
        if let Some(n) = caps.get(1).and_then(|m| super::chunker::cjk_number(m.as_str())) {
            return Route::Section { number_key: n.to_string(), file_hint: hint };
        }
    }

    if let Some(caps) = DEFINITION.captures(query) {
        if let Some(term) = (1..=5).filter_map(|i| caps.get(i)).next() {
            let t = term.as_str().trim().trim_start_matches("the ").trim();
            if !t.is_empty() {
                return Route::Definition { term: t.to_string() };
            }
        }
    }
    let lower = query.to_lowercase();
    if let Some(term) = known_terms
        .iter()
        .filter(|t| t.chars().count() >= 4)
        .find(|t| lower.contains(&format!("\"{}\"", t)) || lower.contains(&format!("“{}”", t)))
    {
        if lower.contains("mean") || lower.contains("defin") {
            return Route::Definition { term: term.clone() };
        }
    }

    if COVERAGE.is_match(query) {
        return Route::Coverage;
    }
    Route::Semantic
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_in_several_languages() {
        for (q, from, to) in [
            ("What is on page 47?", 47, 47),
            ("show pp. 3-5 of the lease", 3, 5),
            ("¿Qué dice la página 12?", 12, 12),
            ("Was steht auf Seite 8?", 8, 8),
            ("第12页写了什么", 12, 12),
            ("p. 102 of SPA.pdf", 102, 102),
        ] {
            match classify(q, &[]) {
                Route::Page { from: f, to: t, .. } => assert_eq!((f, t), (from, to), "{}", q),
                other => panic!("{} -> {:?}", q, other),
            }
        }
    }

    #[test]
    fn file_hint_is_captured() {
        match classify("what does page 4 of Master_Lease_2021.pdf say", &[]) {
            Route::Page { file_hint, .. } => assert_eq!(file_hint.as_deref(), Some("Master_Lease_2021.pdf")),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn sections_and_clauses() {
        for (q, key) in [
            ("Show clause 12.3(b)", "12.3.(b)"),
            ("what does Section 4 say about notice", "4"),
            ("Artículo 7 del contrato", "7"),
            ("§ 15 please", "15"),
            ("第十二条是什么", "12"),
        ] {
            match classify(q, &[]) {
                Route::Section { number_key, .. } => assert_eq!(number_key, key, "{}", q),
                other => panic!("{} -> {:?}", q, other),
            }
        }
    }

    #[test]
    fn definitions() {
        for (q, term) in [
            ("What does \"Material Adverse Effect\" mean?", "Material Adverse Effect"),
            ("definition of Business Day", "Business Day"),
            ("define Closing Date in the SPA", "Closing Date"),
        ] {
            match classify(q, &[]) {
                Route::Definition { term: t } => assert_eq!(t, term, "{}", q),
                other => panic!("{} -> {:?}", q, other),
            }
        }
    }

    #[test]
    fn coverage_and_semantic() {
        assert_eq!(classify("Summarise all the agreements", &[]), Route::Coverage);
        assert_eq!(classify("Can the buyer walk away if revenue drops?", &[]), Route::Semantic);
        assert_eq!(classify("Who pays for repairs to the roof?", &[]), Route::Semantic);
    }
}
