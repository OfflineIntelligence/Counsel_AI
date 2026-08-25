import React, { useState } from 'react';
import { getApiBaseSync } from '../api/backendUrl';
import { ArrowLeft } from 'lucide-react';

interface HelpFeedbackPanelProps {
  isOpen: boolean;
  onClose: () => void;
  isLoggedIn?: boolean;
}

const HelpFeedbackPanel: React.FC<HelpFeedbackPanelProps> = ({ isOpen, onClose, isLoggedIn = false }) => {
  const [feedback, setFeedback] = useState('');
  const [email, setEmail] = useState('');
  const [submitted, setSubmitted] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');

  const handleSubmitFeedback = async () => {
    if (!feedback.trim()) return;
    
    // Validate email format if provided
    if (email.trim() && !isValidEmail(email.trim())) {
      setError('Please enter a valid email address.');
      return;
    }
    
    // Require email if not logged in
    if (!isLoggedIn && !email.trim()) {
      setError('Please provide an email address so we can follow up with you.');
      return;
    }
    
    setSubmitting(true);
    setError('');
    try {
      const res = await fetch(`${getApiBaseSync()}/feedback`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ message: feedback, email }),
      });
      const data = await res.json();
      if (data.success) {
        setSubmitted(true);
        setFeedback('');
        setEmail('');
        setTimeout(() => setSubmitted(false), 4000);
      } else {
        setError(data.message || 'Failed to submit feedback.');
      }
    } catch {
      setError('Could not reach the server. Please try again later.');
    } finally {
      setSubmitting(false);
    }
  };

  const isValidEmail = (email: string): boolean => {
    const emailRegex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
    return emailRegex.test(email);
  };

  if (!isOpen) return null;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', width: '100%', backgroundColor: 'var(--bg-chat)' }}>
      {/* Header */}
      <div className="chat-header">
        <div className="chat-header-bar centered">
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
            <button className="header-icon-btn" onClick={onClose} title="Back to chat">
              <ArrowLeft size={18} />
            </button>
            <h1 className="chat-title" style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
              <svg width="20" height="20" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8.228 9c.549-1.165 2.03-2 3.772-2 2.21 0 4 1.343 4 3 0 1.4-1.278 2.575-3.006 2.907-.542.104-.994.54-.994 1.093m0 3h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
              </svg>
              Help & Feedback
            </h1>
          </div>
        </div>
      </div>

      {/* Content */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '24px 16px' }}>
        <div style={{ maxWidth: '600px', margin: '0 auto' }}>
          {/* FAQ Section */}
          <div className="settings-section">
            <h2 style={{ fontSize: '16px', fontWeight: 600, color: 'var(--text-primary)', marginBottom: '16px' }}>Frequently Asked Questions</h2>

            <div style={{ display: 'flex', flexDirection: 'column', gap: '12px' }}>
              <FaqItem
                q="How does Offline Counsel AI work?"
                a="Offline Counsel AI runs AI models locally on your device. Your conversations never leave your computer, ensuring complete privacy."
              />
              <FaqItem
                q="How do I install a new model?"
                a="Go to the Models page from the sidebar, switch to the Available tab, and click Install on any model you'd like to use."
              />
              <FaqItem
                q="Why are responses slow?"
                a="Response speed depends on your hardware (CPU, RAM, GPU). Smaller models (7B) are faster. Enable GPU acceleration for best performance."
              />
              <FaqItem
                q="Is my data private?"
                a="Yes. All conversations are stored locally on your device. We don't collect any personal data or conversation content."
              />
            </div>
          </div>

          {/* Contact Section */}
          <div className="settings-section" style={{ marginTop: '32px' }}>
            <h2 style={{ fontSize: '16px', fontWeight: 600, color: 'var(--text-primary)', marginBottom: '16px' }}>Send Feedback</h2>
            <p style={{ fontSize: '13px', color: 'var(--text-secondary)', marginBottom: '12px' }}>
              Help us improve Offline Counsel AI by sharing your thoughts, reporting bugs, or suggesting features.
            </p>
            {!isLoggedIn && (
              <p style={{ fontSize: '12px', color: 'var(--accent)', marginBottom: '12px', padding: '8px 12px', backgroundColor: 'var(--bg-tertiary)', borderRadius: '6px' }}>
                ℹ️ Please provide your email address so we can follow up with you.
              </p>
            )}
            <input
              type="email"
              placeholder={isLoggedIn ? "Your email (optional, for follow-up)" : "Your email (required)*"}
              value={email}
              onChange={e => setEmail(e.target.value)}
              required={!isLoggedIn}
              style={{
                width: '100%', padding: '10px 12px', marginBottom: '10px',
                borderRadius: '8px', border: '1px solid var(--border-primary)',
                backgroundColor: 'var(--bg-input)', color: 'var(--text-primary)',
                fontSize: '14px', outline: 'none', fontFamily: 'inherit',
                boxSizing: 'border-box',
              }}
            />
            <textarea
              placeholder="Type your feedback here..."
              value={feedback}
              onChange={e => setFeedback(e.target.value)}
              style={{
                width: '100%', minHeight: '120px', padding: '12px',
                borderRadius: '8px', border: '1px solid var(--border-primary)',
                backgroundColor: 'var(--bg-input)', color: 'var(--text-primary)',
                fontSize: '14px', resize: 'vertical', outline: 'none',
                fontFamily: 'inherit', boxSizing: 'border-box',
              }}
            />
            <div style={{ display: 'flex', alignItems: 'center', gap: '12px', marginTop: '12px' }}>
              <button
                onClick={handleSubmitFeedback}
                disabled={!feedback.trim() || submitting}
                style={{
                  padding: '8px 20px', borderRadius: '8px',
                  backgroundColor: feedback.trim() && !submitting ? 'var(--accent)' : 'var(--bg-tertiary)',
                  color: feedback.trim() && !submitting ? 'white' : 'var(--text-muted)', border: 'none',
                  fontSize: '14px', fontWeight: 500,
                  cursor: feedback.trim() && !submitting ? 'pointer' : 'not-allowed',
                }}
              >
                {submitting ? 'Submitting...' : 'Submit Feedback'}
              </button>
              {submitted && (
                <span style={{ fontSize: '13px', color: 'var(--accent)' }}>
                  Thank you for your feedback!
                </span>
              )}
              {error && (
                <span style={{ fontSize: '13px', color: 'var(--danger)' }}>
                  {error}
                </span>
              )}
            </div>
          </div>

          {/* Keyboard Shortcuts */}
          <div className="settings-section" style={{ marginTop: '32px' }}>
            <h2 style={{ fontSize: '16px', fontWeight: 600, color: 'var(--text-primary)', marginBottom: '16px' }}>Keyboard Shortcuts</h2>
            <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
              <ShortcutRow keys="Enter" desc="Send message" />
              <ShortcutRow keys="Ctrl + N" desc="New chat" />
              <ShortcutRow keys="Ctrl + K" desc="Search conversations" />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

const FaqItem: React.FC<{ q: string; a: string }> = ({ q, a }) => (
  <details style={{
    padding: '12px 14px', borderRadius: '8px',
    backgroundColor: 'var(--bg-tertiary)', cursor: 'pointer',
  }}>
    <summary style={{ fontWeight: 500, fontSize: '14px', color: 'var(--text-primary)' }}>{q}</summary>
    <p style={{ marginTop: '8px', fontSize: '13px', color: 'var(--text-secondary)', lineHeight: 1.6 }}>{a}</p>
  </details>
);

const ShortcutRow: React.FC<{ keys: string; desc: string }> = ({ keys, desc }) => (
  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '6px 0' }}>
    <span style={{ fontSize: '13px', color: 'var(--text-primary)' }}>{desc}</span>
    <kbd style={{
      fontSize: '12px', padding: '2px 8px', borderRadius: '4px',
      backgroundColor: 'var(--bg-tertiary)', border: '1px solid var(--border-primary)',
      color: 'var(--text-secondary)', fontFamily: 'monospace',
    }}>{keys}</kbd>
  </div>
);

export default HelpFeedbackPanel;
