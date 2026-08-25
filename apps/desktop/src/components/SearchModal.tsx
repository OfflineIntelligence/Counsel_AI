// Search Modal: Filters chat transcripts by keyword
// Displays matching chats grouped by time periods in a modal overlay
import React, { useState, useMemo, useEffect, useRef } from 'react';
import type { Chat } from './ChatWindow';

interface SearchModalProps {
  isOpen: boolean;
  chats: Chat[];
  onClose: () => void;
  onSelectChat: (chatId: string) => void;
}

interface SearchResult {
  chat: Chat;
  matchCount: number;
}

interface GroupedResults {
  today: SearchResult[];
  week: SearchResult[];
  month: SearchResult[];
  older: SearchResult[];
}

export const SearchModal: React.FC<SearchModalProps> = ({
  isOpen,
  chats,
  onClose,
  onSelectChat
}) => {
  const [searchQuery, setSearchQuery] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  // Focus input when modal opens
  useEffect(() => {
    if (isOpen) {
      inputRef.current?.focus();
    }
  }, [isOpen]);

  // Handle escape key to close modal
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && isOpen) {
        onClose();
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [isOpen, onClose]);

  // Search chats and group by time period
  const groupedResults = useMemo(() => {
    const now = new Date();
    const grouped: GroupedResults = {
      today: [],
      week: [],
      month: [],
      older: []
    };

    // If no search query, show all chats grouped by time
    if (!searchQuery.trim()) {
      chats.forEach(chat => {
        const chatDate = new Date(chat.createdAt);
        const daysAgo = Math.floor((now.getTime() - chatDate.getTime()) / (1000 * 60 * 60 * 24));

        const result: SearchResult = { chat, matchCount: 0 };

        if (daysAgo === 0) {
          grouped.today.push(result);
        } else if (daysAgo < 7) {
          grouped.week.push(result);
        } else if (daysAgo < 30) {
          grouped.month.push(result);
        } else {
          grouped.older.push(result);
        }
      });

      return grouped;
    }

    // Search functionality
    const query = searchQuery.toLowerCase();
    const results: SearchResult[] = [];

    chats.forEach(chat => {
      let matchCount = 0;

      // Search in chat title
      if (chat.title.toLowerCase().includes(query)) {
        matchCount += 2; // Weight title matches higher
      }

      // Search in all messages (case-insensitive, safe string matching)
      const lowerQuery = query.toLowerCase();
      chat.messages.forEach(msg => {
        const lowerContent = msg.content.toLowerCase();
        let pos = 0;
        while ((pos = lowerContent.indexOf(lowerQuery, pos)) !== -1) {
          matchCount++;
          pos += lowerQuery.length;
        }
      });

      if (matchCount > 0) {
        results.push({ chat, matchCount });
      }
    });

    // Sort by match count descending
    results.sort((a, b) => b.matchCount - a.matchCount);

    results.forEach(result => {
      const chatDate = new Date(result.chat.createdAt);
      const daysAgo = Math.floor((now.getTime() - chatDate.getTime()) / (1000 * 60 * 60 * 24));

      if (daysAgo === 0) {
        grouped.today.push(result);
      } else if (daysAgo < 7) {
        grouped.week.push(result);
      } else if (daysAgo < 30) {
        grouped.month.push(result);
      } else {
        grouped.older.push(result);
      }
    });

    return grouped;
  }, [searchQuery, chats]);

  const handleSelectChat = (chatId: string) => {
    onSelectChat(chatId);
    onClose();
  };

  if (!isOpen) return null;

  return (
    <div className="search-modal-overlay" onClick={onClose}>
      <div className="search-modal-container" onClick={e => e.stopPropagation()}>
        {/* Header with Search Input */}
        <div className="search-modal-header">
          <div className="search-modal-input-wrapper">
            <svg className="search-icon" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
            </svg>
            <input
              ref={inputRef}
              type="text"
              className="search-modal-input"
              placeholder="Search chats..."
              value={searchQuery}
              onChange={e => setSearchQuery(e.target.value)}
            />
            {searchQuery && (
              <button
                className="search-clear-button"
                onClick={() => setSearchQuery('')}
                aria-label="Clear search"
              >
                <svg className="search-clear-icon" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                </svg>
              </button>
            )}
          </div>
          <button
            className="search-modal-close-button"
            onClick={onClose}
            aria-label="Close search"
          >
            ✕
          </button>
        </div>

        {/* Results Container */}
        <div className="search-modal-results">
          {groupedResults.today.length === 0 &&
            groupedResults.week.length === 0 &&
            groupedResults.month.length === 0 &&
            groupedResults.older.length === 0 ? (
            <div className="search-empty-state">
              <p>No chats found</p>
            </div>
          ) : (
            <>
              {/* Today */}
              {groupedResults.today.length > 0 && (
                <div className="search-results-group">
                  <div className="search-group-title">Today</div>
                  {groupedResults.today.map(result => (
                    <button
                      key={result.chat.id}
                      className="search-result-item"
                      onClick={() => handleSelectChat(result.chat.id)}
                    >
                      <div className="search-result-icon">
                        <svg fill="none" stroke="currentColor" viewBox="0 0 24 24">
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                        </svg>
                      </div>
                      <div className="search-result-content">
                        <div className="search-result-title">{result.chat.title}</div>
                      </div>
                    </button>
                  ))}
                </div>
              )}

              {/* Previous 7 Days */}
              {groupedResults.week.length > 0 && (
                <div className="search-results-group">
                  <div className="search-group-title">Previous 7 Days</div>
                  {groupedResults.week.map(result => (
                    <button
                      key={result.chat.id}
                      className="search-result-item"
                      onClick={() => handleSelectChat(result.chat.id)}
                    >
                      <div className="search-result-icon">
                        <svg fill="none" stroke="currentColor" viewBox="0 0 24 24">
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                        </svg>
                      </div>
                      <div className="search-result-content">
                        <div className="search-result-title">{result.chat.title}</div>
                      </div>
                    </button>
                  ))}
                </div>
              )}

              {/* Previous 30 Days */}
              {groupedResults.month.length > 0 && (
                <div className="search-results-group">
                  <div className="search-group-title">Previous 30 Days</div>
                  {groupedResults.month.map(result => (
                    <button
                      key={result.chat.id}
                      className="search-result-item"
                      onClick={() => handleSelectChat(result.chat.id)}
                    >
                      <div className="search-result-icon">
                        <svg fill="none" stroke="currentColor" viewBox="0 0 24 24">
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                        </svg>
                      </div>
                      <div className="search-result-content">
                        <div className="search-result-title">{result.chat.title}</div>
                      </div>
                    </button>
                  ))}
                </div>
              )}

              {/* Older */}
              {groupedResults.older.length > 0 && (
                <div className="search-results-group">
                  <div className="search-group-title">Older</div>
                  {groupedResults.older.map(result => (
                    <button
                      key={result.chat.id}
                      className="search-result-item"
                      onClick={() => handleSelectChat(result.chat.id)}
                    >
                      <div className="search-result-icon">
                        <svg fill="none" stroke="currentColor" viewBox="0 0 24 24">
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                        </svg>
                      </div>
                      <div className="search-result-content">
                        <div className="search-result-title">{result.chat.title}</div>
                      </div>
                    </button>
                  ))}
                </div>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
};
