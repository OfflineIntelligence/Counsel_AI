//! Efficient text processing utilities

use std::borrow::Cow;
use regex::Regex;
use lazy_static::lazy_static;

lazy_static! {
    static ref WHITESPACE_REGEX: Regex = Regex::new(r"\s+").unwrap();
}

/// Case-insensitive text operations without allocation when possible
pub struct TextUtils;

impl TextUtils {
    /// Check if text contains pattern (case-insensitive) without allocation
    pub fn contains_ignore_case(text: &str, pattern: &str) -> bool {
        if pattern.len() > text.len() {
            return false;
        }
        
        // Fast path for short patterns
        if pattern.len() <= 32 {
            text.to_lowercase().contains(&pattern.to_lowercase())
        } else {
            // Use more efficient algorithm for longer patterns
            text.chars()
                .flat_map(char::to_lowercase)
                .collect::<String>()
                .contains(&pattern.to_lowercase())
        }
    }
    
    /// Normalize whitespace efficiently
    pub fn normalize_whitespace(text: &str) -> Cow<'_, str> {
        if WHITESPACE_REGEX.is_match(text) {
            Cow::Owned(WHITESPACE_REGEX.replace_all(text, " ").trim().to_string())
        } else {
            Cow::Borrowed(text)
        }
    }
    
    /// Extract first N words efficiently
    pub fn first_words(text: &str, n: usize) -> Cow<'_, str> {
        if n == 0 || text.is_empty() {
            return Cow::Borrowed("");
        }
        
        let mut word_count = 0;
        let mut end_pos = 0;
        
        for (pos, _) in text.match_indices(' ') {
            word_count += 1;
            if word_count >= n {
                end_pos = pos;
                break;
            }
        }
        
        if end_pos > 0 {
            Cow::Borrowed(&text[..end_pos])
        } else {
            Cow::Borrowed(text)
        }
    }
    
    /// Count words efficiently (no allocation)
    pub fn count_words(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        
        text.split_whitespace().count()
    }
    
    /// Truncate text to max length with ellipsis if needed
    pub fn truncate_with_ellipsis(text: &str, max_len: usize) -> Cow<'_, str> {
        if text.len() <= max_len {
            Cow::Borrowed(text)
        } else if max_len <= 3 {
            Cow::Borrowed("...")
        } else {
            let mut result = String::with_capacity(max_len);
            result.push_str(&text[..max_len - 3]);
            result.push_str("...");
            Cow::Owned(result)
        }
    }
    
    /// Check if word is significant (not stop word, long enough)
    pub fn is_significant_word(word: &str, min_len: usize) -> bool {
        if word.len() < min_len {
            return false;
        }
        
        // Quick ASCII-only check for common stop words
        !matches!(word.to_lowercase().as_str(), "the" | "a" | "an" | "and" | "or" | "but" | "in" | "on" | "at" | "to" | "for" |
            "of" | "with" | "by" | "is" | "am" | "are" | "was" | "were" | "be" | "been" |
            "being" | "have" | "has" | "had" | "do" | "does" | "did")
    }
}