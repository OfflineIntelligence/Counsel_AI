/**
 * Help & Feedback — the same page furniture as Settings.
 *
 * It used to build its own chrome out of inline styles: a chat header, its
 * own font sizes, its own colours and its own input shapes. It now uses the
 * Settings shell and the Settings row/control classes, so the two pages read
 * as one product and a retune of either lands on both.
 */
import React, { useEffect, useState } from 'react';
import { ArrowLeft, CheckCircle } from 'lucide-react';

import { getApiBaseSync } from '../api/backendUrl';
import './SettingsPanel.css';

interface HelpFeedbackPanelProps {
  isOpen: boolean;
  onClose: () => void;
  isLoggedIn?: boolean;
  /** Render only the sections, for the Settings > Help & Support tab. */
  embedded?: boolean;
}

const FAQS: { q: string; a: string }[] = [
  {
    q: 'How does Offline Counsel AI work?',
    a: 'It runs AI models locally on your device. Your conversations never leave this computer.',
  },
  {
    q: 'How do I install a new model?',
    a: 'Open Models from the sidebar, find a model in All models, and choose Download. It is loaded for you once the download finishes.',
  },
  {
    q: 'Why are responses slow?',
    a: 'Speed depends on your hardware. Smaller models answer faster, and layers that fit in your GPU are much faster than the ones that run on the CPU. Settings › System shows how the current model is running.',
  },
  {
    q: 'Is my data private?',
    a: 'Yes. Conversations, documents and models are stored on this device. Nothing is collected or sent anywhere.',
  },
];

const SHORTCUTS: { keys: string; desc: string }[] = [
  { keys: 'Enter', desc: 'Send message' },
  { keys: 'Ctrl + N', desc: 'New chat' },
  { keys: 'Ctrl + K', desc: 'Search conversations' },
];

function isValidEmail(value: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
}

const HelpFeedbackPanel: React.FC<HelpFeedbackPanelProps> = ({ isOpen, onClose, isLoggedIn = false, embedded = false }) => {
  const [feedback, setFeedback] = useState('');
  const [email, setEmail] = useState('');
  const [submitted, setSubmitted] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');

  // Escape leaves the page, as it does in Settings. Backspace does too, as
  // long as the user is not typing in a field — deleting an email character
  // must never navigate away.
  useEffect(() => {
    if (!isOpen || embedded) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
        return;
      }
      if (e.key === 'Backspace') {
        const t = e.target as HTMLElement | null;
        const typing =
          !!t &&
          (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
        if (!typing) onClose();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [isOpen, onClose, embedded]);

  const handleSubmitFeedback = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!feedback.trim()) return;

    if (email.trim() && !isValidEmail(email.trim())) {
      setError('Please enter a valid email address.');
      return;
    }
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
        setError(data.message || 'Could not send your feedback.');
      }
    } catch {
      setError('Could not reach the server. Please try again later.');
    } finally {
      setSubmitting(false);
    }
  };

  if (!isOpen) return null;

  const body = (
<div className="help-columns">
            <div className="help-col">
            <section className="settings-section">
              {FAQS.map(({ q, a }) => (
                <details key={q} className="settings-faq">
                  <summary className="settings-faq__q">{q}</summary>
                  <p className="settings-faq__a">{a}</p>
                </details>
              ))}
            </section>

            <section className="settings-section">
              <h3 className="settings-section__title">Keyboard shortcuts</h3>
              {SHORTCUTS.map(({ keys, desc }) => (
                <div key={keys} className="settings-row">
                  <div className="settings-row__text">
                    <span className="settings-row__title">{desc}</span>
                  </div>
                  <div className="settings-row__control">
                    <kbd className="settings-kbd">{keys}</kbd>
                  </div>
                </div>
              ))}
            </section>
            </div>

            <div className="help-col">
            <section className="settings-section">
              {submitted ? (
                <div className="settings-sent">
                  <span className="settings-sent__icon">
                    <CheckCircle size={22} strokeWidth={1.75} />
                  </span>
                  <span className="settings-sent__title">Thank you</span>
                  <span className="settings-sent__hint">Your feedback has been sent.</span>
                </div>
              ) : (
                <form className="settings-form" onSubmit={handleSubmitFeedback}>
                  <p className="settings-footnote settings-footnote--top">
                    Tell us what broke, what is missing, or what would make this better.
                    {!isLoggedIn && ' Your email lets us follow up.'}
                  </p>

                  <div className="settings-field">
                    <label className="settings-field__label" htmlFor="help-email">
                      Email{isLoggedIn ? ' (optional)' : ''}
                    </label>
                    <input
                      id="help-email"
                      className="settings-input"
                      type="email"
                      value={email}
                      onChange={(e) => setEmail(e.target.value)}
                      placeholder="you@example.com"
                      required={!isLoggedIn}
                    />
                  </div>

                  <div className="settings-field">
                    <label className="settings-field__label" htmlFor="help-feedback">
                      Feedback
                    </label>
                    <textarea
                      id="help-feedback"
                      className={error ? 'settings-textarea settings-textarea--invalid' : 'settings-textarea'}
                      value={feedback}
                      onChange={(e) => setFeedback(e.target.value)}
                      placeholder="Tell us what you think…"
                      rows={5}
                      required
                    />
                    {error && <span className="settings-note settings-note--danger">{error}</span>}
                  </div>

                  <div className="settings-form__actions">
                    <button
                      type="submit"
                      className="settings-btn settings-btn--primary"
                      disabled={!feedback.trim() || submitting}
                    >
                      {submitting ? 'Sending…' : 'Send'}
                    </button>
                  </div>
                </form>
              )}
            </section>

            </div>
            </div>
  );

  // Inside Settings it is just the sections; the page frame is Settings' own.
  if (embedded) return body;

  return (
    <div className="app-view settings-page help-page">
      <div className="settings-shell settings-shell--single">
        <div className="settings-content">
          <div className="settings-inner ui-anim ui-anim--rise">
            <button type="button" className="settings-back" onClick={onClose} title="Back to chat">
              <ArrowLeft size={15} strokeWidth={1.75} aria-hidden="true" />
              <span>Back to chat</span>
            </button>
            <header className="settings-head">
              <h1 className="settings-head__title">Help &amp; Support</h1>
            </header>

            {/* Two columns rather than one tall stack.

                Stacked, the page ran well past the window: four questions, a
                form and a shortcut table in a 720px column, so the one thing
                anyone comes here to DO — send feedback — was below the fold,
                reached by scrolling past material they were not reading. Side
                by side it all fits, and the split says which is which: what to
                read on the left, what to do on the right. */}
            {body}
          </div>
        </div>
      </div>
    </div>
  );
};

export default HelpFeedbackPanel;
