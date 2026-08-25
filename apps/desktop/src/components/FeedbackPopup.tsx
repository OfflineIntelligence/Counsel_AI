import React, { useState } from 'react';
import { useAuth } from '../contexts/AuthContext';
import { getApiBaseSync } from '../api/backendUrl';
import { X, Send, MessageSquare } from 'lucide-react';

interface FeedbackPopupProps {
  isOpen: boolean;
  onClose: () => void;
}

const FeedbackPopup: React.FC<FeedbackPopupProps> = ({ isOpen, onClose }) => {
  const { user } = useAuth();
  const [name, setName] = useState(user?.name || '');
  const [email, setEmail] = useState(user?.email || '');
  const [feedback, setFeedback] = useState('');
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [isSubmitted, setIsSubmitted] = useState(false);
  const [error, setError] = useState('');

  if (!isOpen) return null;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    
    if (!feedback.trim()) {
      setError('Please enter your feedback');
      return;
    }

    setIsSubmitting(true);
    setError('');

    try {
      const response = await fetch(`${getApiBaseSync()}/feedback`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          message: `Name: ${name}\nEmail: ${email}\n\nFeedback:\n${feedback}`,
          email: email,
        }),
      });

      if (response.ok) {
        setIsSubmitted(true);
        setTimeout(() => {
          onClose();
          // Reset form after closing
          setTimeout(() => {
            setIsSubmitted(false);
            setFeedback('');
          }, 300);
        }, 2000);
      } else {
        throw new Error('Failed to submit feedback');
      }
    } catch (err) {
      setError('Failed to send feedback. Please try again.');
    } finally {
      setIsSubmitting(false);
    }
  };

  return (
    <div
      style={{
        position: 'fixed',
        bottom: '24px',
        right: '24px',
        zIndex: 9999,
        maxWidth: '380px',
        width: '100%',
      }}
    >
      <div
        // Token surface, not glass. This popup appears in the same bottom-right
        // corner as the toasts, so it has to belong to the same family — and its
        // previous background was a hardcoded `rgba(47,47,47,0.85)`, i.e. dark in
        // BOTH themes, sitting under text coloured with --text-primary. In light
        // mode that was near-black text on a dark slab. The 20px backdrop blur
        // also cost a full-viewport composite on every repaint behind it, which
        // is not free on the integrated GPUs this app targets.
        style={{
          background: 'var(--bg-raised)',
          borderRadius: 'var(--r-xl)',
          border: '1px solid var(--border)',
          boxShadow: 'var(--shadow-lg)',
          overflow: 'hidden',
          animation: 'slideIn var(--dur-3) var(--ease-entrance)',
        }}
      >
        {/* Header */}
        <div
          style={{
            padding: '16px 20px',
            borderBottom: '1px solid var(--border-subtle)',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            background: 'var(--bg-surface-alt)',
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <div
              style={{
                width: '32px',
                height: '32px',
                borderRadius: '10px',
                background: 'var(--accent-quiet)',
                color: 'var(--accent-fg)',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
              }}
            >
              <MessageSquare size={16} />
            </div>
            <div>
              <h3
                style={{
                  fontSize: '15px',
                  fontWeight: 600,
                  color: 'var(--text-primary)',
                  margin: 0,
                }}
              >
                We value your feedback!
              </h3>
              <p
                style={{
                  fontSize: '12px',
                  color: 'var(--text-secondary)',
                  margin: '2px 0 0 0',
                }}
              >
                Help us improve Offline Counsel AI
              </p>
            </div>
          </div>
          <button
            onClick={onClose}
            style={{
              background: 'transparent',
              border: 'none',
              borderRadius: '8px',
              width: '28px',
              height: '28px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              cursor: 'pointer',
              color: 'var(--text-muted)',
              transition: 'all 0.2s',
            }}
            onMouseEnter={(e) => {
              e.currentTarget.style.background = 'var(--bg-hover)';
              e.currentTarget.style.color = 'var(--text-primary)';
            }}
            onMouseLeave={(e) => {
              e.currentTarget.style.background = 'transparent';
              e.currentTarget.style.color = 'var(--text-muted)';
            }}
          >
            <X size={16} />
          </button>
        </div>

        {/* Content */}
        <div style={{ padding: '20px' }}>
          {isSubmitted ? (
            <div style={{ textAlign: 'center', padding: '20px 0' }}>
              <div
                style={{
                  width: '48px',
                  height: '48px',
                  borderRadius: '50%',
                  background: 'var(--success-quiet)',
                  color: 'var(--success-fg)',
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  margin: '0 auto 16px',
                }}
              >
                <Send size={24} />
              </div>
              <h4
                style={{
                  fontSize: '16px',
                  fontWeight: 600,
                  color: 'var(--text-primary)',
                  marginBottom: '6px',
                }}
              >
                Thank you!
              </h4>
              <p style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>
                Your feedback has been sent successfully.
              </p>
            </div>
          ) : (
            <form onSubmit={handleSubmit}>
              {/* Name Field */}
              <div style={{ marginBottom: '12px' }}>
                <label
                  style={{
                    display: 'block',
                    fontSize: '12px',
                    fontWeight: 500,
                    color: 'var(--text-secondary)',
                    marginBottom: '4px',
                  }}
                >
                  Name
                </label>
                <input
                  type="text"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="Your name"
                  style={{
                    width: '100%',
                    padding: '10px 12px',
                    borderRadius: '10px',
                    border: '1px solid var(--border)',
                    background: 'var(--bg-field)',
                    color: 'var(--text-primary)',
                    fontSize: '13px',
                    outline: 'none',
                    transition: 'all 0.2s',
                  }}
                  onFocus={(e) => {
                    e.currentTarget.style.borderColor = 'var(--accent)';
                    e.currentTarget.style.boxShadow = '0 0 0 3px var(--ring)';
                  }}
                  onBlur={(e) => {
                    e.currentTarget.style.borderColor = 'var(--border)';
                    e.currentTarget.style.boxShadow = 'none';
                  }}
                />
              </div>

              {/* Email Field */}
              <div style={{ marginBottom: '12px' }}>
                <label
                  style={{
                    display: 'block',
                    fontSize: '12px',
                    fontWeight: 500,
                    color: 'var(--text-secondary)',
                    marginBottom: '4px',
                  }}
                >
                  Email
                </label>
                <input
                  type="email"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  placeholder="you@example.com"
                  style={{
                    width: '100%',
                    padding: '10px 12px',
                    borderRadius: '10px',
                    border: '1px solid var(--border)',
                    background: 'var(--bg-field)',
                    color: 'var(--text-primary)',
                    fontSize: '13px',
                    outline: 'none',
                    transition: 'all 0.2s',
                  }}
                  onFocus={(e) => {
                    e.currentTarget.style.borderColor = 'var(--accent)';
                    e.currentTarget.style.boxShadow = '0 0 0 3px var(--ring)';
                  }}
                  onBlur={(e) => {
                    e.currentTarget.style.borderColor = 'var(--border)';
                    e.currentTarget.style.boxShadow = 'none';
                  }}
                />
              </div>

              {/* Feedback Field */}
              <div style={{ marginBottom: '16px' }}>
                <label
                  style={{
                    display: 'block',
                    fontSize: '12px',
                    fontWeight: 500,
                    color: 'var(--text-secondary)',
                    marginBottom: '4px',
                  }}
                >
                  Your Feedback *
                </label>
                <textarea
                  value={feedback}
                  onChange={(e) => setFeedback(e.target.value)}
                  placeholder="Tell us what you think..."
                  rows={3}
                  style={{
                    width: '100%',
                    padding: '10px 12px',
                    borderRadius: '10px',
                    border: `1px solid ${error ? 'var(--danger)' : 'var(--border)'}`,
                    background: 'var(--bg-field)',
                    color: 'var(--text-primary)',
                    fontSize: '13px',
                    outline: 'none',
                    resize: 'none',
                    transition: 'all 0.2s',
                    fontFamily: 'inherit',
                  }}
                  onFocus={(e) => {
                    if (!error) {
                      e.currentTarget.style.borderColor = 'var(--accent)';
                      e.currentTarget.style.boxShadow = '0 0 0 3px var(--ring)';
                    }
                  }}
                  onBlur={(e) => {
                    e.currentTarget.style.borderColor = error ? 'var(--danger)' : 'var(--border)';
                    e.currentTarget.style.boxShadow = 'none';
                  }}
                />
                {error && (
                  <span
                    style={{
                      fontSize: '11px',
                      color: 'var(--danger-fg)',
                      marginTop: '4px',
                      display: 'block',
                    }}
                  >
                    {error}
                  </span>
                )}
              </div>

              {/* Submit Button */}
              <button
                type="submit"
                disabled={isSubmitting}
                style={{
                  width: '100%',
                  padding: '12px',
                  borderRadius: '12px',
                  border: 'none',
                  background: isSubmitting
                    ? 'var(--bg-active)'
                    : 'var(--accent)',
                  color: 'var(--text-on-accent)',
                  fontSize: '14px',
                  fontWeight: 600,
                  cursor: isSubmitting ? 'not-allowed' : 'pointer',
                  transition: 'all 0.2s',
                  opacity: isSubmitting ? 0.7 : 1,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  gap: '8px',
                  boxShadow: isSubmitting ? 'none' : 'var(--shadow-xs)',
                }}
                onMouseEnter={(e) => {
                  if (!isSubmitting) {
                    e.currentTarget.style.transform = 'translateY(-1px)';
                    e.currentTarget.style.boxShadow = 'var(--shadow-sm)';
                  }
                }}
                onMouseLeave={(e) => {
                  e.currentTarget.style.transform = 'translateY(0)';
                  e.currentTarget.style.boxShadow = isSubmitting
                    ? 'none'
                    : 'var(--shadow-xs)';
                }}
              >
                {isSubmitting ? (
                  <>
                    <div
                      style={{
                        width: '16px',
                        height: '16px',
                        border: '2px solid var(--text-on-accent)',
                        borderTop: '2px solid white',
                        borderRadius: '50%',
                        animation: 'spin 0.8s linear infinite',
                      }}
                    />
                    Sending...
                  </>
                ) : (
                  <>
                    <Send size={16} />
                    Send Feedback
                  </>
                )}
              </button>
            </form>
          )}
        </div>
      </div>

      <style>{`
        @keyframes slideIn {
          from {
            opacity: 0;
            transform: translateY(20px) scale(0.95);
          }
          to {
            opacity: 1;
            transform: translateY(0) scale(1);
          }
        }
        @keyframes spin {
          to {
            transform: rotate(360deg);
          }
        }
      `}</style>
    </div>
  );
};

export default FeedbackPopup;
