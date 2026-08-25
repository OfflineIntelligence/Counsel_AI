//! Decides whether a question refers to something the user has ALREADY seen —
//! an earlier conversation, or a document attached at some point in the past.
//!
//! This is the gate in front of retrieval. When it says no, the turn is treated
//! as a new topic and the database is not searched; when it says yes, the search
//! text and any narrowing it found (named files, a date window) are handed to
//! the FTS layer. Getting the NO case right matters as much as the yes: a new
//! question that drags in unrelated old material produces worse answers than one
//! that ignores history entirely.
//!
//! # What this is, precisely
//!
//! A curated set of LEXICAL patterns. It matches the words people actually use
//! to point backwards. It is not semantic understanding, and cannot be: there is
//! no embedding model in this system, so nothing here can recognise a phrasing
//! by meaning alone. A way of referring to the past that nobody anticipated will
//! not be detected, however many patterns are listed below.
//!
//! Two things keep that honest rather than fragile:
//!
//! 1. **Retrieval is ranked and thresholded downstream.** A false positive here
//!    costs a cheap indexed query that returns nothing above threshold, so it
//!    injects nothing. That asymmetry is why the patterns below can afford to be
//!    generous rather than conservative.
//! 2. **An explicit filename or date always wins.** Those are unambiguous
//!    signals that do not depend on guessing intent from phrasing.
//!
//! # Confidence tiers
//!
//! Some words point backwards reliably ("as we discussed"); others only
//! sometimes ("before"). `before we begin` and `also explain X` are ordinary
//! phrasings in a brand-new question, and treating either as a reference to
//! history would make every first message trigger a search. So weak signals do
//! not fire on their own — see [`ReferenceIntent::refers_to_past`].

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use lazy_static::lazy_static;
use regex::Regex;

/// Why this query was judged to refer to the past. Carried through so the
/// decision can be logged and explained rather than being an opaque boolean —
/// "retrieved because you named contract.pdf" is debuggable; "retrieved" is not.
#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceSignal {
    /// A filename was named outright, e.g. "what does lease.pdf say".
    ExplicitFilename(String),
    /// A known document was described rather than named, e.g. "the merger
    /// agreement" resolving to "merger-agreement.pdf".
    DescribedDocument { query_text: String, filename: String },
    /// An unambiguous backward reference, e.g. "as we discussed".
    StrongPhrase(&'static str),
    /// A word that often but not always points backwards, e.g. "before".
    /// Never sufficient on its own.
    WeakPhrase(&'static str),
    /// A date or time expression, with the window it resolved to.
    Temporal {
        matched: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceIntent {
    /// Every signal found, in detection order.
    pub signals: Vec<ReferenceSignal>,
    /// Filenames to prioritise, from explicit mentions and resolved
    /// descriptions. Deduplicated, original order preserved.
    pub named_files: Vec<String>,
    /// The window a date expression resolved to, if any. Narrows retrieval to
    /// documents and messages from that period.
    pub time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl ReferenceIntent {
    /// Whether this query should trigger a search of past conversations and
    /// documents.
    ///
    /// True when there is at least one signal that is meaningful alone — a named
    /// or described file, an unambiguous phrase, or a date — OR when two
    /// independent weak signals agree. The two-weak rule exists because "and
    /// also, what about before?" is a real backward reference built entirely
    /// from words that are individually inconclusive, while a single "before"
    /// most often opens a fresh question.
    pub fn refers_to_past(&self) -> bool {
        let mut weak = 0usize;
        for signal in &self.signals {
            match signal {
                ReferenceSignal::ExplicitFilename(_)
                | ReferenceSignal::DescribedDocument { .. }
                | ReferenceSignal::StrongPhrase(_)
                | ReferenceSignal::Temporal { .. } => return true,
                ReferenceSignal::WeakPhrase(_) => weak += 1,
            }
        }
        weak >= 2
    }

    /// Human-readable reason, for logs and for telling the user why old material
    /// appeared in an answer.
    pub fn explain(&self) -> String {
        if self.signals.is_empty() {
            return "no reference to earlier material detected".to_string();
        }
        let parts: Vec<String> = self
            .signals
            .iter()
            .map(|s| match s {
                ReferenceSignal::ExplicitFilename(f) => format!("named file '{}'", f),
                ReferenceSignal::DescribedDocument { query_text, filename } => {
                    format!("'{}' matches stored document '{}'", query_text, filename)
                }
                ReferenceSignal::StrongPhrase(p) => format!("phrase '{}'", p),
                ReferenceSignal::WeakPhrase(p) => format!("weak phrase '{}'", p),
                ReferenceSignal::Temporal { matched, .. } => format!("time reference '{}'", matched),
            })
            .collect();
        parts.join("; ")
    }
}

/// Unambiguous backward references.
///
/// Every entry here is a phrase that makes little sense in a genuinely new
/// question. The originals from `RetrievalPlanner::has_past_references_in_text`
/// are all retained (that set shipped and is known to work); the additions cover
/// second-person recall ("you said"), deictic references to prior artifacts
/// ("that document"), resumption ("going back to"), and the interrogative forms
/// people actually type ("didn't we", "what was that").
const STRONG_PHRASES: &[&str] = &[
    // --- original 16, kept verbatim ---
    "earlier",
    "last time",
    "yesterday",
    "we discussed",
    "we talked about",
    "remember",
    "recall",
    "did we talk",
    "have we discussed",
    "what did we say",
    "what was said",
    "mentioned earlier",
    "previously mentioned",
    // ("before", "previous" and "previously" moved to WEAK_PHRASES or given
    //  tighter patterns below - see PRECISE_STRONG.)
    // --- conversational recall, first and second person ---
    "we agreed",
    "we covered",
    "we established",
    "we reviewed",
    "we went over",
    "you said",
    "you mentioned",
    "you told me",
    "you explained",
    "you wrote",
    "i said",
    "i mentioned",
    "i told you",
    "i asked",
    "as discussed",
    "as mentioned",
    "as noted",
    "as agreed",
    "as established",
    "as we said",
    "per our",
    "from our conversation",
    "in our conversation",
    "our earlier",
    "our previous",
    "our last",
    // --- interrogative recall ---
    "did we discuss",
    "did we cover",
    "did we agree",
    "did i ask",
    "did i say",
    "did you say",
    "did you mention",
    "didn't we",
    "didnt we",
    "wasn't there",
    "wasnt there",
    "what was that",
    "what were those",
    "which one did",
    "remind me",
    "refresh my memory",
    // --- deictic references to prior artifacts ---
    "that document",
    "that file",
    "that contract",
    "that agreement",
    "the document i",
    "the file i",
    "the contract i",
    "the one i",
    "those documents",
    "those files",
    "the attachment",
    "the attached",
    "same document",
    "same file",
    // --- resumption / continuation ---
    "going back to",
    "back to the",
    "returning to",
    "revisit",
    "follow up on",
    "following up on",
    "as i was saying",
    "like i said",
    "like you said",
    "the other day",
    "back then",
    "at the time",
    "up until now",
    "so far",
];

/// Words that point backwards only sometimes. Never sufficient alone — see
/// [`ReferenceIntent::refers_to_past`].
///
/// `before`, `previous(ly)`, `prior`, `again`, `also` and `still` all appear
/// naturally in brand-new questions ("before you answer, note that…", "also
/// explain X", "is this still standard?"). They are recorded because two of
/// them together usually IS a backward reference, but one is not evidence.
const WEAK_PHRASES: &[&str] = &[
    "before",
    "previous",
    "previously",
    "prior",
    "again",
    "also",
    "still",
    "already",
    "another",
    "that time",
];

lazy_static! {
    /// Strong forms of otherwise-weak words: `before` is inconclusive, but
    /// "said before" / "discussed before" is not. Kept as regexes so word
    /// boundaries are respected and "beforehand" does not match "before".
    static ref PRECISE_STRONG: Vec<(Regex, &'static str)> = vec![
        (Regex::new(r"\b(said|mentioned|discussed|talked|asked|agreed|covered|noted|sent|shared|attached|uploaded)\s+(it\s+)?before\b").unwrap(), "... before"),
        (Regex::new(r"\bpreviously\s+(said|mentioned|discussed|asked|agreed|noted|sent|shared|attached|uploaded)\b").unwrap(), "previously ..."),
        (Regex::new(r"\bprevious\s+(chat|conversation|message|question|answer|document|file|contract|agreement|session|discussion)\b").unwrap(), "previous <thing>"),
        (Regex::new(r"\bprior\s+(chat|conversation|message|document|file|contract|agreement|discussion)\b").unwrap(), "prior <thing>"),
        (Regex::new(r"\bearlier\s+(chat|conversation|message|question|document|file|today|you|we|i)\b").unwrap(), "earlier <thing>"),
    ];

    /// A filename with one of the product's supported extensions, optionally
    /// preceded by the "@" the picker inserts.
    ///
    /// Spaces are deliberately EXCLUDED from the name. Allowing them (to support
    /// "my contract.pdf") made the match swallow preceding prose: "What does
    /// contract.pdf say" captured the filename as "What does contract.pdf",
    /// because every word before it is valid filename material once a space is
    /// permitted. There is no way to tell where such a name begins in free text.
    ///
    /// The cost is that a file whose real name contains a space resolves to its
    /// last word ("contract.pdf" from "my contract.pdf"), which still matches the
    /// right document via the FTS filename column. The alternative - swallowing
    /// the sentence - matches nothing at all.
    static ref EXPLICIT_FILENAME: Regex = Regex::new(
        r"(?i)@?([\w][\w\-.]{0,120}?\.(?:pdf|docx?|xlsx?|pptx?|txt|png|jpe?g))\b"
    ).unwrap();

    static ref N_UNITS_AGO: Regex =
        Regex::new(r"(?i)\b(\d{1,3})\s+(day|week|month|year)s?\s+ago\b").unwrap();
    static ref ISO_DATE: Regex = Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").unwrap();
    /// "July 20", "20 July", "Jul 20th", "20th of July", with an optional year.
    static ref MONTH_DAY: Regex = Regex::new(
        r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s+(\d{4}))?\b"
    ).unwrap();
    static ref DAY_MONTH: Regex = Regex::new(
        r"(?i)\b(\d{1,2})(?:st|nd|rd|th)?\s+(?:of\s+)?(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?(?:,?\s+(\d{4}))?\b"
    ).unwrap();
    static ref WEEKDAY: Regex = Regex::new(
        r"(?i)\b(?:last\s+|this\s+past\s+|on\s+)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    ).unwrap();
}

/// Detect whether `query` refers to earlier material.
///
/// `known_filenames` are the documents this system already holds; they let a
/// DESCRIPTION resolve to a file ("the merger agreement" -> merger-agreement.pdf)
/// rather than requiring the user to type an exact name. Pass an empty slice to
/// skip that (the cheap, purely lexical path).
///
/// `now` is injected rather than read from the clock so date resolution is
/// deterministic and testable — "yesterday" must be assertable.
pub fn detect_reference(
    query: &str,
    known_filenames: &[String],
    now: DateTime<Utc>,
) -> ReferenceIntent {
    let lower = query.to_lowercase();
    let mut intent = ReferenceIntent::default();

    // 1. Explicit filenames. Strongest and cheapest signal.
    for caps in EXPLICIT_FILENAME.captures_iter(query) {
        if let Some(m) = caps.get(1) {
            let name = m.as_str().trim().to_string();
            push_file(&mut intent, name.clone());
            intent.signals.push(ReferenceSignal::ExplicitFilename(name));
        }
    }

    // 2. Described documents: a stored filename's words appear in the query.
    for filename in known_filenames {
        if intent.named_files.iter().any(|f| f.eq_ignore_ascii_case(filename)) {
            continue;
        }
        if let Some(matched_text) = describes_filename(&lower, filename) {
            push_file(&mut intent, filename.clone());
            intent.signals.push(ReferenceSignal::DescribedDocument {
                query_text: matched_text,
                filename: filename.clone(),
            });
        }
    }

    // 2b. Definite reference to a stored document: "the agreement", "that
    //     contract". Only consulted when nothing was resolved above, and only
    //     the FIRST match is taken.
    //
    //     This exists because requiring EVERY filename word (step 2) misses the
    //     way people actually refer back. "Software_Agreement_Akhil_Mansoor.docx"
    //     needs all four of software/agreement/akhil/mansoor; a user who has been
    //     discussing it for ten minutes says "the agreement". That question
    //     resolved to nothing at all in a real session, and the model, given no
    //     document, invented a reason it could not show one.
    //
    //     DEFINITENESS is what makes this safe rather than eager. "the
    //     agreement" presupposes a specific agreement both sides already know;
    //     "draft a new agreement" introduces one. Only the former fires, so the
    //     precision the all-words rule was protecting is preserved by a
    //     different and more accurate means.
    //
    //     `known_filenames` arrives most-recently-referenced first
    //     (`all_document_filenames` orders by last_referenced_at), so taking the
    //     first match resolves "the agreement" to the agreement most recently
    //     in play - which is what the phrase means.
    if intent.named_files.is_empty() {
        for filename in known_filenames {
            if let Some(matched_text) = definite_reference_to_filename(&lower, filename) {
                push_file(&mut intent, filename.clone());
                intent.signals.push(ReferenceSignal::DescribedDocument {
                    query_text: matched_text,
                    filename: filename.clone(),
                });
                break;
            }
        }
    }

    // 3. Tightened forms of the otherwise-weak words, checked BEFORE the weak
    //    list so "discussed it before" registers as strong rather than weak.
    let mut precise_hit = false;
    for (re, label) in PRECISE_STRONG.iter() {
        if re.is_match(&lower) {
            intent.signals.push(ReferenceSignal::StrongPhrase(label));
            precise_hit = true;
        }
    }

    // 4. Unambiguous phrases.
    for phrase in STRONG_PHRASES {
        if contains_phrase(&lower, phrase) {
            intent.signals.push(ReferenceSignal::StrongPhrase(phrase));
        }
    }

    // 5. Weak words, recorded but not decisive. Skipped entirely when a precise
    //    form already fired, so "discussed before" is not counted twice.
    if !precise_hit {
        for phrase in WEAK_PHRASES {
            if contains_phrase(&lower, phrase) {
                intent.signals.push(ReferenceSignal::WeakPhrase(phrase));
            }
        }
    }

    // 6. Date and time expressions.
    if let Some((matched, start, end)) = parse_temporal(&lower, now) {
        intent.time_range = Some((start, end));
        intent.signals.push(ReferenceSignal::Temporal { matched, start, end });
    }

    intent
}

fn push_file(intent: &mut ReferenceIntent, name: String) {
    if !intent.named_files.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
        intent.named_files.push(name);
    }
}

/// Whole-phrase containment. Guards the boundaries so "prior" does not match
/// "priority" and "also" does not match "although" — the class of false positive
/// that would make a plain substring search fire on almost any question.
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let mut from = 0usize;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        let after_ok = end >= haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len().max(1);
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// Determiners that mark a noun as ALREADY KNOWN to both speaker and listener.
///
/// This is the whole basis of step 2b. English marks the difference between
/// introducing a thing and referring back to one, and that distinction is
/// exactly what separates "draft a new agreement" from "show me the agreement".
/// Indefinite determiners (a, an, another, some, any) are deliberately absent.
const DEFINITE_DETERMINERS: &[&str] = &[
    "the", "that", "this", "those", "these", "our", "your", "my", "its",
    "their", "said", "aforementioned", "same", "above",
];

/// Words in a filename distinctive enough to be referred to by one of them.
///
/// Three characters rather than four because legal filenames are full of
/// meaningful short acronyms - nda, sow, msa, loi - and "the NDA" is one of the
/// most natural definite references there is. Purely numeric fragments ("v2",
/// "2026") are dropped: nobody says "the 2026".
fn distinctive_filename_words(filename: &str) -> Vec<String> {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename)
        .to_lowercase();
    stem.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .filter(|w| !w.chars().all(|c| c.is_numeric()))
        .map(|w| w.to_string())
        .collect()
}

/// Does the query refer to `filename` by a definite noun phrase?
///
/// Matches a definite determiner followed, within two intervening words, by a
/// distinctive word of the filename - so "the agreement", "that software
/// agreement" and "the signed lease schedule" all resolve, while "a new
/// agreement" does not. Returns the matched phrase for the explanation.
fn definite_reference_to_filename(lower_query: &str, filename: &str) -> Option<String> {
    let words = distinctive_filename_words(filename);
    if words.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = lower_query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }

    for (i, token) in tokens.iter().enumerate() {
        if !DEFINITE_DETERMINERS.contains(token) {
            continue;
        }
        let last = (i + 3).min(tokens.len() - 1);
        for j in (i + 1)..=last {
            if words.iter().any(|w| w == tokens[j]) {
                return Some(tokens[i..=j].join(" "));
            }
        }
    }
    None
}

/// Does `lower_query` describe `filename` without naming it?
///
/// Splits the filename's stem into words and requires EVERY word of 3+
/// characters to appear in the query as a whole word. Returns the matched words
/// joined, for the explanation.
///
/// Two deliberate limits keep this precise rather than eager:
///
/// * Stems shorter than 5 characters are skipped. "a.pdf" or "x.txt" carry no
///   describable content, and a 2-3 character stem would match constantly.
/// * ALL significant words must be present, not any. "merger-agreement.pdf"
///   needs both "merger" and "agreement"; matching on "agreement" alone would
///   pull in every contract in the library on any question mentioning one.
fn describes_filename(lower_query: &str, filename: &str) -> Option<String> {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename)
        .to_lowercase();
    if stem.chars().count() < 5 {
        return None;
    }
    let words: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .collect();
    if words.is_empty() {
        return None;
    }
    if words.iter().all(|w| contains_phrase(lower_query, w)) {
        Some(words.join(" "))
    } else {
        None
    }
}

fn day_bounds(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
    let end = Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59).unwrap());
    (start, end)
}

/// Month number from a name or 3+ letter abbreviation, or None.
///
/// The length guard is load-bearing: this is also used to ASK "is this capture
/// group the month or the day?" when disambiguating "July 20" from "20 July",
/// so it is routinely handed a numeric string like "20". Slicing `[..3]` of that
/// panicked on a 2-character input.
fn month_number(abbrev: &str) -> Option<u32> {
    let lower = abbrev.to_lowercase();
    let prefix: String = lower.chars().take(3).collect();
    if prefix.chars().count() < 3 {
        return None;
    }
    Some(match prefix.as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

fn weekday_number(name: &str) -> Option<u32> {
    Some(match name.to_lowercase().as_str() {
        "monday" => 0,
        "tuesday" => 1,
        "wednesday" => 2,
        "thursday" => 3,
        "friday" => 4,
        "saturday" => 5,
        "sunday" => 6,
        _ => return None,
    })
}

/// Resolve a date or time expression to an absolute window.
///
/// Checked most-specific first, so "20 July 2026" is not consumed by a looser
/// pattern. All windows are in the PAST or contain today: this detector exists
/// to find things already said, so a future date is not a useful reference and
/// an explicit date that resolves ahead of `now` is rolled back a year (someone
/// writing "July 20" in January means last July).
fn parse_temporal(lower: &str, now: DateTime<Utc>) -> Option<(String, DateTime<Utc>, DateTime<Utc>)> {
    let today = now.date_naive();

    // ISO date: unambiguous, highest precedence.
    if let Some(c) = ISO_DATE.captures(lower) {
        let (y, m, d) = (
            c[1].parse::<i32>().ok()?,
            c[2].parse::<u32>().ok()?,
            c[3].parse::<u32>().ok()?,
        );
        if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
            let (s, e) = day_bounds(date);
            return Some((c[0].to_string(), s, e));
        }
    }

    // "N days/weeks/months/years ago"
    if let Some(c) = N_UNITS_AGO.captures(lower) {
        let n = c[1].parse::<i64>().ok()?;
        let unit = c[2].to_lowercase();
        let days = match unit.as_str() {
            "day" => n,
            "week" => n * 7,
            "month" => n * 30,
            "year" => n * 365,
            _ => return None,
        };
        let start_date = today - Duration::days(days);
        // A window rather than a point: "3 weeks ago" is approximate, so the
        // day either side is included for day/week units and a wider band for
        // month/year units where the arithmetic is already an approximation.
        let pad = if unit == "day" || unit == "week" { 1 } else { 15 };
        let (s, _) = day_bounds(start_date - Duration::days(pad));
        let (_, e) = day_bounds(start_date + Duration::days(pad));
        return Some((c[0].to_string(), s, e));
    }

    // Named relative expressions, longest/most specific first.
    let relative: &[(&str, i64, i64)] = &[
        // (phrase, days back for start, days back for end)
        ("day before yesterday", 2, 2),
        ("last night", 1, 1),
        ("yesterday", 1, 1),
        ("this morning", 0, 0),
        ("earlier today", 0, 0),
        ("today", 0, 0),
        ("last week", 14, 7),
        ("past week", 7, 0),
        ("this week", 7, 0),
        ("last month", 60, 30),
        ("past month", 30, 0),
        ("this month", 30, 0),
        ("last year", 730, 365),
        ("past year", 365, 0),
    ];
    for (phrase, start_back, end_back) in relative {
        if contains_phrase(lower, phrase) {
            let (s, _) = day_bounds(today - Duration::days(*start_back));
            let (_, e) = day_bounds(today - Duration::days(*end_back));
            return Some((phrase.to_string(), s, e));
        }
    }

    // "July 20" / "20 July", with optional year.
    for caps in [MONTH_DAY.captures(lower), DAY_MONTH.captures(lower)]
        .into_iter()
        .flatten()
    {
        // MONTH_DAY captures (month, day, year?); DAY_MONTH captures (day, month, year?).
        let (m_str, d_str) = if month_number(&caps[1]).is_some() {
            (caps[1].to_string(), caps[2].to_string())
        } else {
            (caps[2].to_string(), caps[1].to_string())
        };
        let month = month_number(&m_str)?;
        let day = d_str.parse::<u32>().ok()?;
        let year = caps
            .get(3)
            .and_then(|y| y.as_str().parse::<i32>().ok())
            .unwrap_or_else(|| today.year());
        if let Some(mut date) = NaiveDate::from_ymd_opt(year, month, day) {
            // No explicit year and the date is in the future: they meant last year.
            if caps.get(3).is_none() && date > today {
                date = NaiveDate::from_ymd_opt(year - 1, month, day)?;
            }
            let (s, e) = day_bounds(date);
            return Some((caps[0].to_string(), s, e));
        }
    }

    // Weekday name -> the most recent occurrence at or before today.
    if let Some(c) = WEEKDAY.captures(lower) {
        let target = weekday_number(&c[1])?;
        let current = today.weekday().num_days_from_monday();
        let mut back = (current + 7 - target) % 7;
        if back == 0 {
            // "on Monday" spoken on a Monday most often means the previous one.
            back = 7;
        }
        let date = today - Duration::days(back as i64);
        let (s, e) = day_bounds(date);
        return Some((c[0].trim().to_string(), s, e));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        // A Wednesday, so weekday arithmetic is exercised meaningfully.
        Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 7, 29)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
        )
    }

    fn detect(q: &str) -> ReferenceIntent {
        detect_reference(q, &[], now())
    }

    // ---------------------------------------------------------------------
    // The NO case. Every one of these is a brand-new question, and a search
    // triggered here would drag unrelated history into a fresh topic - the
    // failure that makes retrieval feel worse than no retrieval.
    // ---------------------------------------------------------------------

    #[test]
    fn brand_new_questions_do_not_trigger_retrieval() {
        let fresh = [
            "What is a force majeure clause?",
            "Draft a mutual NDA for a software vendor.",
            "Explain the difference between indemnity and warranty.",
            "Summarise this document.",
            "What are the key risks here?",
            "Before you answer, note that I am not a lawyer.",
            "Also explain the tax treatment.",
            "Is this still standard practice?",
            "Can you walk through it again, step by step?",
            "Please review and give me another option.",
            "How do I terminate an agreement?",
        ];
        for q in fresh {
            let intent = detect(q);
            assert!(
                !intent.refers_to_past(),
                "{:?} must NOT trigger retrieval, but matched: {}",
                q,
                intent.explain()
            );
        }
    }

    /// The precise reason weak words exist as a separate tier: each of these
    /// contains exactly one, and one is not evidence.
    #[test]
    fn a_single_weak_word_is_not_enough() {
        for q in [
            "Before you answer, check the maths.",
            "Also, what is the cap?",
            "Is it still valid?",
            "Give me another example.",
        ] {
            let intent = detect(q);
            assert!(
                !intent.signals.is_empty(),
                "{:?} should record a weak signal for diagnostics",
                q
            );
            assert!(
                !intent.refers_to_past(),
                "{:?} must not be treated as a backward reference: {}",
                q,
                intent.explain()
            );
        }
    }

    #[test]
    fn two_weak_words_together_do_count() {
        let intent = detect("Also, what did you say about that before?");
        assert!(
            intent.refers_to_past(),
            "two independent weak signals should count: {}",
            intent.explain()
        );
    }

    // ---------------------------------------------------------------------
    // The YES case.
    // ---------------------------------------------------------------------

    #[test]
    fn all_sixteen_original_phrases_are_still_detected() {
        // The set that shipped in RetrievalPlanner. Regression guard: widening
        // the list must never drop what already worked.
        for q in [
            "What did we say earlier about the cap?",
            "What happened last time?",
            "What did we agree yesterday?",
            "We discussed the indemnity, what was the outcome?",
            "We talked about termination rights.",
            "Do you remember the fee schedule?",
            "Can you recall the notice period?",
            "Did we talk about assignment?",
            "Have we discussed governing law?",
            "What did we say about arbitration?",
            "What was said about the deposit?",
            "As mentioned earlier, the term is three years.",
            "The previously mentioned clause applies.",
        ] {
            assert!(
                detect(q).refers_to_past(),
                "original phrase must still be detected: {:?}",
                q
            );
        }
    }

    #[test]
    fn widened_phrases_are_detected() {
        for q in [
            "You said the cap was five million.",
            "You mentioned an exclusivity period.",
            "I told you about the side letter.",
            "As discussed, the deadline is Friday.",
            "Per our conversation, please revise clause 4.",
            "Didn't we cover this?",
            "Wasn't there a carve-out for affiliates?",
            "Remind me what the deposit was.",
            "Refresh my memory on the termination fee.",
            "Going back to the earlier point about liability.",
            "What did that document say about notice?",
            "The file I attached mentions arbitration.",
            "Following up on the indemnity question.",
            "What was that clause number again?",
        ] {
            assert!(
                detect(q).refers_to_past(),
                "widened phrase must be detected: {:?}",
                q
            );
        }
    }

    /// Weak words become strong in context. "discussed it before" is
    /// unambiguous; bare "before" is not.
    #[test]
    fn weak_words_are_promoted_by_context() {
        for q in [
            "What did you say about this before?",
            "We discussed it before, what was the answer?",
            "I previously asked about the escrow.",
            "In our previous conversation you gave a figure.",
            "Check the prior agreement for that term.",
            "Our earlier conversation covered this.",
        ] {
            let intent = detect(q);
            assert!(
                intent.refers_to_past(),
                "{:?} must be detected as a backward reference: {}",
                q,
                intent.explain()
            );
        }
    }

    #[test]
    fn a_bare_before_is_not_promoted() {
        // The guard on the promotion rule: the tightened patterns must require
        // their verb, not fire on any sentence containing "before".
        let intent = detect("Read this before you reply.");
        assert!(!intent.refers_to_past(), "matched: {}", intent.explain());
    }

    // ---------------------------------------------------------------------
    // Filenames
    // ---------------------------------------------------------------------

    #[test]
    fn explicit_filenames_are_detected_for_every_supported_type() {
        for name in [
            "contract.pdf",
            "notes.txt",
            "report.docx",
            "old.doc",
            "budget.xlsx",
            "legacy.xls",
            "deck.pptx",
            "deck2.ppt",
            "scan.png",
            "photo.jpg",
            "photo2.jpeg",
        ] {
            let intent = detect(&format!("What does {} say about fees?", name));
            assert!(
                intent.refers_to_past(),
                "naming {} must trigger retrieval",
                name
            );
            assert_eq!(intent.named_files, vec![name.to_string()], "for {}", name);
        }
    }

    #[test]
    fn at_prefixed_and_multiple_filenames_are_captured() {
        let intent = detect("Compare @merger.pdf with @nda.docx please");
        assert!(intent.refers_to_past());
        assert_eq!(
            intent.named_files,
            vec!["merger.pdf".to_string(), "nda.docx".to_string()]
        );
    }

    #[test]
    fn unsupported_extensions_are_not_treated_as_filenames() {
        let intent = detect("Unzip archive.zip and check config.yaml");
        assert!(
            intent.named_files.is_empty(),
            "only supported types are filenames: {:?}",
            intent.named_files
        );
        assert!(!intent.refers_to_past());
    }

    #[test]
    fn a_described_document_resolves_to_a_stored_file() {
        let known = vec![
            "merger-agreement.pdf".to_string(),
            "lease_schedule.docx".to_string(),
            "nda.pdf".to_string(),
        ];
        let intent = detect_reference("what did the merger agreement say about escrow?", &known, now());
        assert!(intent.refers_to_past(), "{}", intent.explain());
        assert_eq!(intent.named_files, vec!["merger-agreement.pdf".to_string()]);

        let intent = detect_reference("check the lease schedule for the rent", &known, now());
        assert_eq!(intent.named_files, vec!["lease_schedule.docx".to_string()]);
    }

    /// The reported failure, verbatim. In a real session this question
    /// resolved to NOTHING - detection did not fire, no document was
    /// retrieved, and the model invented a reason it could not show the
    /// content.
    #[test]
    fn a_definite_reference_resolves_a_document_the_user_never_fully_named() {
        let known = vec!["Software_Agreement_Akhil_Mansoor.docx".to_string()];
        let intent = detect_reference(
            "can you display me the exact content of the agreement if possible?",
            &known,
            now(),
        );

        assert!(
            intent.refers_to_past(),
            "this question must be recognised as referring back: {}",
            intent.explain()
        );
        assert_eq!(
            intent.named_files,
            vec!["Software_Agreement_Akhil_Mansoor.docx".to_string()],
            "'the agreement' must resolve to the stored file: {}",
            intent.explain()
        );
    }

    /// The guarantee the all-words rule was protecting must survive. An
    /// INDEFINITE noun phrase introduces a new thing and must not retrieve.
    #[test]
    fn an_indefinite_reference_still_resolves_nothing() {
        let known = vec!["Software_Agreement_Akhil_Mansoor.docx".to_string()];
        for q in [
            "draft a new agreement for me",
            "what should an agreement include",
            "write me another agreement",
            "can you produce some agreement templates",
        ] {
            let intent = detect_reference(q, &known, now());
            assert!(
                intent.named_files.is_empty(),
                "{:?} must not resolve a stored document, got {:?}",
                q,
                intent.named_files
            );
        }
    }

    /// "the agreement" means the one most recently in play. `known_filenames`
    /// arrives most-recently-referenced first, so the first match is correct -
    /// and it must be the ONLY match, not every agreement in the library.
    #[test]
    fn a_definite_reference_picks_the_most_recent_matching_document_only() {
        let known = vec![
            "Software_Agreement_Akhil_Mansoor.docx".to_string(),
            "old-supply-agreement.pdf".to_string(),
            "ancient_agreement_2019.docx".to_string(),
        ];
        let intent = detect_reference("summarise the agreement again", &known, now());
        assert_eq!(
            intent.named_files,
            vec!["Software_Agreement_Akhil_Mansoor.docx".to_string()],
            "only the most recently referenced match may be taken"
        );
    }

    /// A full description is a stronger statement than a definite one and must
    /// win when both could apply.
    #[test]
    fn a_full_description_takes_precedence_over_a_definite_one() {
        let known = vec![
            "Software_Agreement_Akhil_Mansoor.docx".to_string(),
            "merger-agreement.pdf".to_string(),
        ];
        let intent = detect_reference("what did the merger agreement say?", &known, now());
        assert!(
            intent.named_files.contains(&"merger-agreement.pdf".to_string()),
            "the fully described file must win, got {:?}",
            intent.named_files
        );
    }

    /// Short legal acronyms are among the most natural definite references.
    #[test]
    fn a_definite_reference_to_a_short_acronym_resolves() {
        let known = vec!["nda.pdf".to_string()];
        let intent = detect_reference("what does the nda say about disclosure?", &known, now());
        assert_eq!(intent.named_files, vec!["nda.pdf".to_string()]);
    }

    /// Intervening adjectives are normal in legal speech.
    #[test]
    fn a_definite_reference_survives_intervening_words() {
        let known = vec!["lease_schedule.docx".to_string()];
        let intent = detect_reference("check the signed lease schedule for me", &known, now());
        assert_eq!(intent.named_files, vec!["lease_schedule.docx".to_string()]);
    }

    /// Numeric fragments are not referable nouns.
    #[test]
    fn version_numbers_in_filenames_are_not_referable() {
        let known = vec!["contract_v2_2026.pdf".to_string()];
        let intent = detect_reference("what happened in the 2026 budget?", &known, now());
        assert!(
            intent.named_files.is_empty(),
            "a bare year must not resolve a filename, got {:?}",
            intent.named_files
        );
    }

    /// ALL significant words must match, not any - otherwise one question
    /// mentioning "agreement" would pull in every contract in the library.
    #[test]
    fn a_partial_description_does_not_resolve_a_document() {
        let known = vec!["merger-agreement.pdf".to_string()];
        let intent = detect_reference("draft a new agreement for me", &known, now());
        assert!(
            intent.named_files.is_empty(),
            "one shared word must not resolve a document: {:?}",
            intent.named_files
        );
        assert!(!intent.refers_to_past(), "{}", intent.explain());
    }

    #[test]
    fn very_short_stems_are_never_matched_by_description() {
        let known = vec!["a.pdf".to_string(), "x.txt".to_string(), "q1.xlsx".to_string()];
        let intent = detect_reference("what is a q1 x metric", &known, now());
        assert!(
            intent.named_files.is_empty(),
            "short stems match constantly and must be skipped: {:?}",
            intent.named_files
        );
    }

    #[test]
    fn an_explicitly_named_file_is_not_duplicated_by_description() {
        let known = vec!["merger-agreement.pdf".to_string()];
        let intent =
            detect_reference("does merger-agreement.pdf cover the merger agreement scope?", &known, now());
        assert_eq!(
            intent.named_files,
            vec!["merger-agreement.pdf".to_string()],
            "the same file must appear once"
        );
    }

    // ---------------------------------------------------------------------
    // Dates and times
    // ---------------------------------------------------------------------

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn yesterday_and_today_resolve_to_the_right_day() {
        let intent = detect("what did we decide yesterday?");
        let (s, e) = intent.time_range.expect("yesterday must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 28));
        assert_eq!(e.date_naive(), ymd(2026, 7, 28));

        let intent = detect("what did I ask earlier today?");
        let (s, _) = intent.time_range.expect("today must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 29));
    }

    #[test]
    fn n_units_ago_resolves_to_a_window_around_that_point() {
        let intent = detect("we talked about this 3 days ago");
        let (s, e) = intent.time_range.expect("must resolve");
        // 2026-07-29 minus 3 days = 07-26, padded one day either side.
        assert_eq!(s.date_naive(), ymd(2026, 7, 25));
        assert_eq!(e.date_naive(), ymd(2026, 7, 27));

        let intent = detect("the contract I sent 2 weeks ago");
        let (s, e) = intent.time_range.expect("must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 14));
        assert_eq!(e.date_naive(), ymd(2026, 7, 16));
    }

    #[test]
    fn last_week_and_last_month_resolve_to_ranges_in_the_past() {
        let intent = detect("what did we discuss last week?");
        let (s, e) = intent.time_range.expect("last week must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 15));
        assert_eq!(e.date_naive(), ymd(2026, 7, 22));
        assert!(s < e);

        let intent = detect("the file from last month");
        let (s, e) = intent.time_range.expect("last month must resolve");
        assert!(s < e && e < now(), "the window must be in the past");
    }

    #[test]
    fn explicit_dates_resolve_in_both_orders_and_iso_form() {
        for q in [
            "what did we say on 2026-07-20?",
            "the document from July 20",
            "the document from 20 July",
            "the document from 20th of July, 2026",
            "the note dated Jul 20th",
        ] {
            let intent = detect(q);
            let (s, _) = intent
                .time_range
                .unwrap_or_else(|| panic!("{:?} must resolve to a date", q));
            assert_eq!(s.date_naive(), ymd(2026, 7, 20), "for {:?}", q);
            assert!(intent.refers_to_past(), "a date is a strong signal: {:?}", q);
        }
    }

    /// A month/day with no year that would land in the future means LAST year -
    /// someone writing "December 20" in July means the December just gone.
    #[test]
    fn a_yearless_future_date_rolls_back_to_last_year() {
        let intent = detect("what did we file on December 20?");
        let (s, _) = intent.time_range.expect("must resolve");
        assert_eq!(s.date_naive(), ymd(2025, 12, 20));
    }

    #[test]
    fn weekday_names_resolve_to_the_most_recent_past_occurrence() {
        // now() is Wednesday 2026-07-29.
        let intent = detect("what did we agree on Monday?");
        let (s, _) = intent.time_range.expect("Monday must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 27), "the Monday just gone");

        let intent = detect("the document from last Friday");
        let (s, _) = intent.time_range.expect("Friday must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 24), "the Friday before today");

        // Spoken ON a Wednesday, "on Wednesday" means the previous one.
        let intent = detect("what did I ask on Wednesday?");
        let (s, _) = intent.time_range.expect("Wednesday must resolve");
        assert_eq!(s.date_naive(), ymd(2026, 7, 22));
    }

    #[test]
    fn a_question_with_no_time_reference_has_no_range() {
        assert!(detect("what is an indemnity?").time_range.is_none());
        assert!(detect("draft a contract").time_range.is_none());
    }

    // ---------------------------------------------------------------------
    // Explanation
    // ---------------------------------------------------------------------

    #[test]
    fn the_decision_is_explainable() {
        let intent = detect_reference(
            "what did we discuss about merger-agreement.pdf yesterday?",
            &[],
            now(),
        );
        let why = intent.explain();
        assert!(why.contains("merger-agreement.pdf"), "{}", why);
        assert!(why.contains("we discussed") || why.contains("phrase"), "{}", why);
        assert!(why.contains("yesterday"), "{}", why);

        assert_eq!(
            detect("what is a warranty?").explain(),
            "no reference to earlier material detected"
        );
    }
}
