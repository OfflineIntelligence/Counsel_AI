//! Common topic extraction utilities

use crate::memory::Message;
use lazy_static::lazy_static;

lazy_static! {
    static ref STOP_WORDS: Vec<&'static str> = vec![
        "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for",
        "of", "with", "by", "is", "am", "are", "was", "were", "be", "been",
        "being", "have", "has", "had", "do", "does", "did", "will", "would",
        "shall", "should", "may", "might", "must", "can", "could", "i", "you",
        "he", "she", "it", "we", "they", "me", "him", "her", "us", "them",
        "my", "your", "his", "its", "our", "their", "mine", "yours", "hers",
        "ours", "theirs", "this", "that", "these", "those",
    ];
}

/// Extract topics from text with configurable parameters
pub struct TopicExtractor {
    max_topics: usize,
    min_word_length: usize,
}

impl Default for TopicExtractor {
    fn default() -> Self {
        Self {
            max_topics: 3,
            min_word_length: 3,
        }
    }
}

impl TopicExtractor {
    pub fn new(max_topics: usize, min_word_length: usize) -> Self {
        Self {
            max_topics,
            min_word_length,
        }
    }
    
    /// Extract topics from a single text
    pub fn extract_from_text(&self, text: &str) -> Vec<String> {
        let mut topics = Vec::new();
        let text_lower = text.to_lowercase();
        let words: Vec<&str> = text_lower.split_whitespace().collect();
        
        // Look for question patterns
        let question_words = ["what", "how", "why", "when", "where", "who", "which"];
        for i in 0..words.len().saturating_sub(1) {
            if question_words.contains(&words[i]) {
                let topic = self.extract_topic_phrase(&words, i + 1, 4);
                if !topic.is_empty() {
                    topics.push(topic);
                }
            }
            
            // Look for "about" pattern
            if words[i] == "about" || words[i] == "regarding" || words[i] == "discussing" {
                let topic = self.extract_topic_phrase(&words, i + 1, 3);
                if !topic.is_empty() {
                    topics.push(topic);
                }
            }
        }
        
        // Fallback: extract significant words
        if topics.is_empty() {
            let significant: Vec<&str> = words.iter()
                .filter(|&&word| {
                    word.len() >= self.min_word_length &&
                    !STOP_WORDS.contains(&word) &&
                    (word.ends_with("ing") || word.ends_with("tion") || 
                     word.starts_with("what") || word.starts_with("how"))
                })
                .take(self.max_topics * 2)
                .copied()
                .collect();
            
            if !significant.is_empty() {
                topics.push(significant.join(" "));
            }
        }
        
        // Deduplicate and limit
        topics.sort();
        topics.dedup();
        topics.truncate(self.max_topics);
        
        // Capitalize first letter
        topics.iter_mut().for_each(|topic| {
            if !topic.is_empty() {
                let mut chars: Vec<char> = topic.chars().collect();
                if !chars.is_empty() {
                    chars[0] = chars[0].to_uppercase().next().unwrap_or(chars[0]);
                    *topic = chars.into_iter().collect();
                }
            }
        });
        
        topics
    }
    
    /// Extract topics from messages
    pub fn extract_from_messages(&self, messages: &[Message], recent_count: usize) -> Vec<String> {
        let recent_messages: Vec<&Message> = messages.iter()
            .rev()
            .take(recent_count)
            .collect();
        
        let mut all_topics = Vec::new();
        for message in recent_messages {
            let topics = self.extract_from_text(&message.content);
            all_topics.extend(topics);
        }
        
        // Deduplicate and limit
        all_topics.sort();
        all_topics.dedup();
        all_topics.truncate(self.max_topics);
        
        all_topics
    }
    
    /// Helper to extract topic phrase starting from position
    fn extract_topic_phrase(&self, words: &[&str], start: usize, max_words: usize) -> String {
        let end = (start + max_words).min(words.len());
        if start >= end {
            return String::new();
        }
        
        let phrase_words: Vec<&str> = words[start..end].iter()
            .filter(|&&word| word.len() >= self.min_word_length && !STOP_WORDS.contains(&word))
            .copied()
            .collect();
        
        if phrase_words.is_empty() {
            String::new()
        } else {
            phrase_words.join(" ")
        }
    }
    
    /// Check if a word is a stop word
    pub fn is_stop_word(word: &str) -> bool {
        STOP_WORDS.contains(&word.to_lowercase().as_str())
    }
}