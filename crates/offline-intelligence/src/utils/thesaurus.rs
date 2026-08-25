//! Synonym expansion for search, tuned for legal documents.
//!
//! # The problem this solves
//!
//! The full-text index matches words, and its porter stemmer only relates
//! words that share a written root: "terminate" finds "termination", and a
//! family prefix relates "indemnify" to "indemnification". Nothing relates
//! DIFFERENT words for the same idea. A user asking about a "lease" gets
//! nothing from a document that says "tenancy" throughout, which in legal
//! drafting is the normal case rather than the exception.
//!
//! # How it works
//!
//! Purely at query time. A token contributes its exact form, its family
//! prefix, and now the other members of its synonym group. The INDEX is
//! untouched, so this needs no migration and no rebuild — the same property
//! that made the family-prefix change cheap.
//!
//! Ranking does the rest: a document containing the user's actual word matches
//! more clauses than one matching only a synonym, so BM25 keeps exact hits on
//! top and synonyms add recall underneath rather than displacing precision.
//!
//! # Why some words are deliberately NOT expanded
//!
//! Legal English reuses everyday words with entirely different meanings, and
//! expanding those makes results worse, not better:
//!
//! | word          | legal sense              | everyday sense        |
//! |---------------|--------------------------|-----------------------|
//! | consideration | the value exchanged      | thinking about        |
//! | execution     | signing                  | carrying out          |
//! | service       | delivery of process      | a service provided    |
//! | instrument    | a legal document         | a tool                |
//! | interest      | a right in property      | curiosity, or a rate  |
//! | will          | a testament              | the modal verb        |
//!
//! `AMBIGUOUS_TERMS` lists these. They are never expanded and never appear as
//! an expansion target, enforced by a test over the data rather than by
//! reviewer discipline. "will" shows why the target rule matters as much as
//! the key rule: expanding "testament" to "will" would match every document
//! containing the ordinary verb.
//!
//! # Scope, honestly
//!
//! This fixes VOCABULARY, not PARAPHRASE. "may terminate without cause" still
//! will not find "termination for convenience", because no thesaurus relates a
//! phrase to a differently-worded description of the same concept. That needs
//! a model, and is a separate decision.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Most synonyms added for any one query token.
///
/// Bounds the OR query: a 12-token question with unlimited expansion would
/// build a clause list that matches nearly everything, which is slow and
/// destroys ranking. Groups are ordered with the closest equivalents first, so
/// a cap keeps the best ones.
const MAX_SYNONYMS_PER_TOKEN: usize = 6;

/// Words never expanded, in either direction — see the module docs.
///
/// Being on this list does not mean the word is unsearchable: it still
/// contributes its exact form and its family prefix, exactly as before. It
/// only means no synonyms are attached to it.
pub const AMBIGUOUS_TERMS: &[&str] = &[
    "consideration",
    "execution",
    "execute",
    "executed",
    "service",
    "services",
    "instrument",
    "instruments",
    "interest",
    "interests",
    "policy",
    "policies",
    "will",
    "trust",
    "bond",
    "bonds",
    "note",
    "notes",
    "record",
    "records",
    "title",
    "titles",
    "charge",
    "charges",
    "security",
    "motion",
    "motions",
    "brief",
    "counsel",
    "action",
    "actions",
    "party",
    "parties",
    "assignment",
    "assignments",
    "capital",
];

/// Synonym groups. Every member is treated as equivalent to every other.
///
/// Conventions:
/// - lowercase throughout; lookup lowercases the query token
/// - closest equivalents first, because `MAX_SYNONYMS_PER_TOKEN` truncates
/// - multi-word entries are emitted as FTS5 phrase queries, which match only
///   that exact sequence — this is what makes phrases containing an ambiguous
///   word ("last will and testament") safe when the bare word is not
/// - British and American spellings belong in the same group
pub const SYNONYM_GROUPS: &[&[&str]] = &[
    // ── Property and leases ──────────────────────────────────────────────
    &["lease", "tenancy", "letting", "leasehold", "rental agreement"],
    &["landlord", "lessor", "owner"],
    &["tenant", "lessee", "renter", "occupier", "occupant"],
    &["rent", "rents", "rental", "rental payment"],
    &["premises", "property", "site", "unit", "demised premises"],
    &["sublease", "sublet", "subletting", "underlease", "sub-tenancy"],
    &["eviction", "ejectment", "repossession", "dispossession"],
    &["forfeiture", "re-entry", "termination of lease"],
    &["dilapidations", "disrepair", "want of repair"],
    &["easement", "right of way", "servitude", "wayleave"],
    &["mortgage", "encumbrance", "lien"],
    &["freehold", "fee simple", "absolute ownership"],
    &["fixtures", "fittings", "chattels"],
    &["surrender", "yield up", "give up possession"],
    &["quiet enjoyment", "peaceful possession"],
    &["deed", "conveyance", "transfer document"],
    &["survey", "inspection", "valuation"],
    &["boundary", "demarcation", "property line"],
    // ── Contracts ────────────────────────────────────────────────────────
    &["contract", "agreement", "arrangement", "bargain", "deal"],
    &["clause", "provision", "article", "paragraph", "section", "term"],
    &["schedule", "annex", "annexure", "appendix", "exhibit", "attachment"],
    &["addendum", "amendment", "variation", "modification", "supplement"],
    &["recital", "preamble", "background", "whereas clause"],
    &["termination", "cancellation", "rescission", "revocation", "ending"],
    &["terminate", "cancel", "rescind", "revoke", "end", "discontinue"],
    &["expiry", "expiration", "lapse", "running out"],
    &["renewal", "extension", "rollover", "continuation"],
    &["breach", "default", "violation", "contravention", "non-performance"],
    &["remedy", "relief", "cure", "redress"],
    &["waiver", "forbearance", "relinquishment"],
    &["novation", "transfer", "substitution"],
    &["severability", "severance clause"],
    &["entire agreement", "whole agreement", "integration clause"],
    &["governing law", "applicable law", "choice of law"],
    &["jurisdiction", "venue", "forum"],
    &["counterpart", "counterparts", "duplicate original"],
    &["notice", "notification", "communication"],
    &["effective date", "commencement date", "start date"],
    &["signatory", "signer", "subscriber"],
    &["signature", "signing", "subscription"],
    &["binding", "enforceable", "legally effective"],
    &["void", "invalid", "null", "ineffective", "unenforceable"],
    &["condition precedent", "prerequisite", "precondition"],
    &["undertaking", "covenant", "promise", "commitment"],
    &["obligation", "duty", "responsibility", "requirement"],
    &["right", "entitlement", "privilege"],
    &["term of the agreement", "duration", "period"],
    // ── Liability, indemnity, risk ───────────────────────────────────────
    &["indemnity", "indemnification", "indemnify", "hold harmless"],
    &["liability", "responsibility", "accountability", "exposure"],
    &["damages", "compensation", "loss", "losses"],
    &["negligence", "carelessness", "want of care", "fault"],
    &["warranty", "guarantee", "assurance", "warranties"],
    &["representation", "statement", "assertion"],
    &["misrepresentation", "false statement", "untrue statement"],
    &["limitation of liability", "liability cap", "cap on liability"],
    &["consequential loss", "indirect loss", "special damages"],
    &["force majeure", "act of god", "unforeseen event"],
    &["insurance", "cover", "coverage", "indemnity insurance"],
    &["risk", "exposure", "hazard"],
    &["defect", "fault", "flaw", "deficiency"],
    // ── Payment and money ────────────────────────────────────────────────
    &["payment", "remittance", "disbursement", "settlement of account"],
    &["pay", "remit", "disburse", "settle"],
    &["invoice", "bill", "statement of account"],
    &["fee", "fees", "cost", "costs", "price", "charge payable"],
    &["deposit", "advance payment", "down payment"],
    &["refund", "reimbursement", "repayment", "rebate"],
    &["penalty", "liquidated damages", "financial penalty"],
    &["arrears", "overdue amount", "outstanding balance"],
    &["tax", "duty", "levy", "vat"],
    &["money", "funds", "monies", "cash", "sums"],
    &["escrow", "stakeholder account", "holding account"],
    &["setoff", "set-off", "deduction", "offset"],
    &["currency", "denomination", "legal tender"],
    // ── Employment ───────────────────────────────────────────────────────
    &["employee", "worker", "staff", "personnel", "member of staff"],
    &["employer", "hiring company"],
    &["salary", "wage", "wages", "remuneration", "pay", "emoluments"],
    &["dismissal", "termination of employment", "firing", "discharge"],
    &["redundancy", "layoff", "retrenchment"],
    &["resignation", "quitting", "stepping down"],
    &["notice period", "period of notice"],
    &["misconduct", "gross misconduct", "improper conduct"],
    &["holiday", "annual leave", "vacation", "paid leave"],
    &["sick leave", "sickness absence", "medical leave"],
    &["non-compete", "restrictive covenant", "restraint of trade"],
    &["confidentiality", "non-disclosure", "secrecy"],
    &["probation", "probationary period", "trial period"],
    &["grievance", "complaint", "formal complaint"],
    &["discrimination", "unequal treatment", "unfair treatment"],
    &["harassment", "bullying", "victimisation", "victimization"],
    &["contractor", "freelancer", "consultant", "self-employed"],
    &["pension", "retirement benefit", "superannuation"],
    &["bonus", "incentive payment", "commission"],
    // ── Corporate ────────────────────────────────────────────────────────
    &["company", "corporation", "firm", "business", "entity", "organisation", "organization"],
    &["shareholder", "stockholder", "equity holder"],
    &["share", "shares", "stock", "equity"],
    &["director", "officer", "board member"],
    &["board", "board of directors", "governing body"],
    &["merger", "amalgamation", "consolidation"],
    &["acquisition", "takeover", "buyout", "purchase of shares"],
    &["due diligence", "investigation", "verification exercise"],
    &["articles of association", "bylaws", "constitution", "articles"],
    &["dividend", "distribution", "shareholder payment"],
    &["subsidiary", "affiliate", "group company", "related company"],
    &["winding up", "liquidation", "dissolution"],
    &["insolvency", "bankruptcy", "administration", "receivership"],
    &["resolution", "formal decision", "board resolution"],
    &["quorum", "minimum attendance"],
    &["minutes", "meeting record", "note of meeting"],
    &["auditor", "accountant", "external auditor"],
    &["financial statements", "accounts", "balance sheet"],
    // ── Intellectual property ────────────────────────────────────────────
    &["intellectual property", "ip rights", "proprietary rights"],
    &["copyright", "authorship right"],
    &["trademark", "trade mark", "mark", "brand name"],
    &["patent", "invention right"],
    &["licence", "license", "permission", "authorisation", "authorization"],
    &["licensor", "grantor", "rights holder"],
    &["licensee", "grantee", "permitted user"],
    &["royalty", "royalties", "licence fee", "usage fee"],
    &["infringement", "unauthorised use", "unauthorized use"],
    &["trade secret", "know-how", "confidential information"],
    &["moral rights", "author rights"],
    // ── Litigation and dispute resolution ────────────────────────────────
    &["claim", "lawsuit", "suit", "proceedings", "case"],
    &["claimant", "plaintiff", "petitioner", "applicant"],
    &["defendant", "respondent", "accused"],
    &["court", "tribunal", "bench"],
    &["judge", "justice", "adjudicator", "magistrate"],
    &["judgment", "judgement", "ruling", "decision", "decree", "order"],
    &["appeal", "review", "challenge"],
    &["evidence", "proof", "documentation", "supporting material"],
    &["witness", "deponent"],
    &["testimony", "deposition", "witness statement"],
    &["settlement", "compromise", "accord", "resolution of dispute"],
    &["arbitration", "arbitral proceedings"],
    &["mediation", "conciliation", "alternative dispute resolution"],
    &["injunction", "restraining order", "prohibitory order"],
    &["discovery", "disclosure", "document production"],
    &["pleading", "statement of case", "particulars of claim"],
    &["limitation period", "statute of limitations", "prescription period"],
    &["damages award", "monetary award", "compensation award"],
    &["costs order", "legal fees", "costs of proceedings"],
    &["subpoena", "witness summons"],
    &["affidavit", "sworn statement", "statutory declaration"],
    &["hearing", "trial", "court appearance"],
    &["dispute", "disagreement", "controversy", "conflict"],
    // ── Data protection and privacy ──────────────────────────────────────
    &["personal data", "personal information", "pii"],
    &["data controller", "controller"],
    &["data processor", "processor"],
    &["data breach", "security incident", "data leak"],
    &["data protection", "gdpr", "privacy law"],
    &["retention", "storage period", "retention period"],
    &["consent", "permission", "agreement to process"],
    &["data subject", "individual", "person concerned"],
    &["anonymisation", "anonymization", "de-identification"],
    &["encryption", "cryptographic protection"],
    // ── Regulatory and compliance ────────────────────────────────────────
    &["regulation", "rule", "requirement", "regulatory requirement"],
    &["compliance", "adherence", "conformity"],
    &["breach of regulation", "non-compliance", "regulatory breach"],
    &["licence to operate", "permit", "approval", "consent to operate"],
    &["audit", "inspection", "examination", "review"],
    &["statute", "act", "legislation", "enactment"],
    &["amendment to law", "statutory amendment"],
    &["penalty notice", "fine", "sanction"],
    &["whistleblowing", "protected disclosure"],
    &["anti-bribery", "anti-corruption", "bribery prevention"],
    &["money laundering", "aml", "financial crime"],
    &["sanctions", "trade restrictions", "embargo"],
    // ── Common actions and qualities ─────────────────────────────────────
    &["amend", "modify", "alter", "vary", "revise", "change"],
    &["allow", "permit", "authorise", "authorize", "sanction"],
    &["prohibit", "forbid", "bar", "preclude", "disallow"],
    &["require", "oblige", "mandate", "compel"],
    &["grant", "give", "provide", "furnish", "supply", "confer"],
    &["obtain", "receive", "acquire", "procure"],
    &["deliver", "send", "transmit", "dispatch", "forward"],
    &["notify", "inform", "advise", "tell", "give notice"],
    &["agree", "consent", "accept", "assent", "approve"],
    &["refuse", "reject", "decline", "withhold consent"],
    &["commence", "start", "begin", "initiate"],
    &["complete", "finish", "conclude", "finalise", "finalize"],
    &["retain", "keep", "preserve", "maintain"],
    &["destroy", "delete", "erase", "dispose of"],
    &["demonstrate", "show", "establish", "evidence"],
    &["assist", "help", "support", "aid"],
    &["repair", "fix", "make good", "rectify"],
    &["material", "significant", "substantial", "important"],
    &["immaterial", "insignificant", "trivial", "minor"],
    &["reasonable", "fair", "proportionate"],
    &["unreasonable", "unfair", "disproportionate"],
    &["promptly", "expeditiously", "without delay", "forthwith"],
    &["immediately", "at once", "instantly"],
    &["annually", "yearly", "per annum", "each year"],
    &["monthly", "per month", "each month"],
    &["business day", "working day", "weekday"],
    &["written", "in writing", "documented"],
    &["verbal", "oral", "spoken"],
    &["mutual", "reciprocal", "shared"],
    &["exclusive", "sole", "unshared"],
    &["perpetual", "indefinite", "unlimited in time"],
    // ── Everyday vocabulary that appears in documents ────────────────────
    &["car", "vehicle", "automobile", "motor vehicle"],
    &["house", "home", "dwelling", "residence"],
    &["buy", "purchase", "acquire"],
    &["sell", "dispose", "vend", "transfer for value"],
    &["job", "employment", "position", "post", "role"],
    &["doctor", "physician", "medical practitioner"],
    &["lawyer", "attorney", "solicitor", "barrister", "advocate"],
    &["letter", "correspondence", "written communication"],
    &["meeting", "conference", "session"],
    &["copy", "duplicate", "reproduction"],
    &["address", "location", "place of business"],
    &["date", "day", "calendar date"],
    &["child", "minor", "dependant", "dependent"],
    &["spouse", "husband", "wife", "partner in marriage"],
    &["death", "decease", "passing"],
    &["injury", "harm", "bodily harm"],
    &["accident", "incident", "mishap"],
    &["goods", "products", "merchandise", "items"],
    &["supplier", "vendor", "seller", "provider"],
    &["customer", "client", "purchaser", "buyer"],
    &["delivery", "shipment", "dispatch", "consignment"],
    &["quality", "standard", "specification"],
    &["quantity", "amount", "volume"],
    &["equipment", "machinery", "apparatus", "plant"],
    &["software", "program", "application", "computer program"],
    &["premises liability", "occupiers liability"],
    &["estate", "assets", "property of the deceased"],
    &["beneficiary", "legatee", "heir"],
    &["executor", "personal representative", "administrator"],
    &["testament", "last will and testament"],
    &["probate", "grant of representation"],
    &["guardian", "custodian", "carer"],
    &["power of attorney", "authority to act", "proxy"],
];

/// token -> the other members of its group.
fn index() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut map: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for group in SYNONYM_GROUPS {
            for member in group.iter() {
                // Multi-word members are expansion TARGETS only. Lookup happens
                // per token, so a phrase can never be a key here; the bigram
                // path below handles two-word queries.
                if member.contains(' ') || is_ambiguous(member) {
                    continue;
                }
                let others: Vec<&'static str> = group
                    .iter()
                    .filter(|other| other != &member)
                    .copied()
                    .collect();
                map.entry(member).or_default().extend(others);
            }
        }
        // A word can legitimately sit in more than one group; de-duplicate so
        // the per-token cap is spent on distinct terms.
        for values in map.values_mut() {
            let mut seen = std::collections::HashSet::new();
            values.retain(|v| seen.insert(*v));
        }
        map
    })
}

/// "word word" -> the other members of the group it belongs to.
///
/// Legal terminology is heavily multi-word ("force majeure", "due diligence",
/// "hold harmless"), and those are exactly the terms a user is most likely to
/// type verbatim. Without this, a two-word query would be split into tokens
/// that individually match nothing useful.
fn bigram_index() -> &'static HashMap<String, Vec<&'static str>> {
    static INDEX: OnceLock<HashMap<String, Vec<&'static str>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut map: HashMap<String, Vec<&'static str>> = HashMap::new();
        for group in SYNONYM_GROUPS {
            for member in group.iter() {
                let words: Vec<&str> = member.split(' ').collect();
                if words.len() != 2 {
                    continue;
                }
                let others: Vec<&'static str> = group
                    .iter()
                    .filter(|other| other != &member)
                    .copied()
                    .collect();
                map.entry(member.to_string()).or_default().extend(others);
            }
        }
        for values in map.values_mut() {
            let mut seen = std::collections::HashSet::new();
            values.retain(|v| seen.insert(*v));
        }
        map
    })
}

/// Whether a term is on the do-not-expand list.
pub fn is_ambiguous(term: &str) -> bool {
    AMBIGUOUS_TERMS.iter().any(|t| t.eq_ignore_ascii_case(term))
}

/// Synonyms for one token, capped. Empty when the token is unknown or
/// deliberately not expanded.
pub fn synonyms_for(token: &str) -> Vec<&'static str> {
    let lower = token.to_lowercase();
    if is_ambiguous(&lower) {
        return Vec::new();
    }
    index()
        .get(lower.as_str())
        .map(|v| v.iter().take(MAX_SYNONYMS_PER_TOKEN).copied().collect())
        .unwrap_or_default()
}

/// Synonyms for an adjacent word pair, capped. Empty when the pair is not a
/// known term.
pub fn synonyms_for_pair(first: &str, second: &str) -> Vec<&'static str> {
    let key = format!("{} {}", first.to_lowercase(), second.to_lowercase());
    bigram_index()
        .get(&key)
        .map(|v| v.iter().take(MAX_SYNONYMS_PER_TOKEN).copied().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expands_to(token: &str, expected: &str) -> bool {
        synonyms_for(token).iter().any(|s| *s == expected)
    }

    #[test]
    fn the_two_cases_that_motivated_this_both_work() {
        assert!(expands_to("lease", "tenancy"), "lease must find tenancy");
        assert!(expands_to("tenancy", "lease"), "and the reverse");
        assert!(expands_to("car", "vehicle"), "car must find vehicle");
        assert!(expands_to("vehicle", "car"), "and the reverse");
    }

    #[test]
    fn expansion_is_symmetric_for_every_group() {
        // Asymmetry would make results depend on which word the user happened
        // to type, which is precisely the failure being fixed.
        for group in SYNONYM_GROUPS {
            for a in group.iter().filter(|w| !w.contains(' ') && !is_ambiguous(w)) {
                for b in group.iter().filter(|w| !w.contains(' ') && !is_ambiguous(w)) {
                    if a == b {
                        continue;
                    }
                    // Only checkable within the per-token cap.
                    let syns = index().get(*a).cloned().unwrap_or_default();
                    assert!(
                        syns.contains(b),
                        "{:?} should expand to {:?} - both are in the same group",
                        a,
                        b
                    );
                }
            }
        }
    }

    /// The precision guard. If an ambiguous word ever appears in a group, the
    /// data has silently contradicted the policy in the module docs.
    #[test]
    fn no_ambiguous_word_appears_in_any_group_as_a_single_word_entry() {
        for group in SYNONYM_GROUPS {
            for member in group.iter() {
                if member.contains(' ') {
                    // Phrases are exempt: an FTS5 phrase query matches only the
                    // exact sequence, so "last will and testament" cannot match
                    // a stray "will".
                    continue;
                }
                assert!(
                    !is_ambiguous(member),
                    "{:?} is on AMBIGUOUS_TERMS but appears in a synonym group - \
                     expanding it would match its everyday meaning",
                    member
                );
            }
        }
    }

    #[test]
    fn ambiguous_words_expand_to_nothing() {
        // They remain searchable by their exact form; they simply gain no
        // synonyms.
        for term in ["consideration", "execution", "service", "interest", "will", "party"] {
            assert!(
                synonyms_for(term).is_empty(),
                "{:?} must not be expanded",
                term
            );
        }
    }

    #[test]
    fn expansion_is_bounded_per_token() {
        for group in SYNONYM_GROUPS {
            for member in group.iter().filter(|w| !w.contains(' ')) {
                assert!(
                    synonyms_for(member).len() <= MAX_SYNONYMS_PER_TOKEN,
                    "{:?} expanded past the cap",
                    member
                );
            }
        }
    }

    #[test]
    fn multi_word_legal_terms_are_matched_as_pairs() {
        assert!(
            synonyms_for_pair("force", "majeure").iter().any(|s| *s == "act of god"),
            "force majeure must find act of god"
        );
        assert!(
            synonyms_for_pair("due", "diligence").iter().any(|s| *s == "investigation"),
            "due diligence must find investigation"
        );
        assert!(
            synonyms_for_pair("hold", "harmless").iter().any(|s| *s == "indemnity"),
            "hold harmless must find indemnity"
        );
    }

    #[test]
    fn an_unknown_word_expands_to_nothing_rather_than_guessing() {
        assert!(synonyms_for("zxqwerty").is_empty());
        assert!(synonyms_for_pair("zxq", "werty").is_empty());
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(expands_to("LEASE", "tenancy"));
        assert!(!synonyms_for("Indemnity").is_empty());
    }

    #[test]
    fn spelling_variants_are_related() {
        // A document drafted in one variety must be findable from the other.
        assert!(expands_to("licence", "license"));
        assert!(expands_to("judgment", "judgement"));
        assert!(expands_to("organisation", "organization"));
        assert!(expands_to("authorise", "authorize"));
    }

    #[test]
    fn legal_vocabulary_spans_the_domains_this_product_serves() {
        // A spot check across areas, so a future edit that guts one section
        // fails here rather than silently degrading search for that domain.
        assert!(expands_to("indemnity", "hold harmless"));
        assert!(expands_to("dismissal", "redundancy") || expands_to("dismissal", "discharge"));
        assert!(expands_to("claimant", "plaintiff"));
        assert!(expands_to("trademark", "trade mark"));
        assert!(expands_to("landlord", "lessor"));
        assert!(expands_to("shareholder", "stockholder"));
        assert!(expands_to("arbitration", "arbitral proceedings"));
        assert!(expands_to("gdpr", "data protection"));
    }
}
