/**
 * The feedback page: the same split layout, type and form style as the login
 * page. Sends to product@offlineintelligence.io through the backend's
 * /feedback endpoint (which also keeps a copy on this computer).
 */
import React, { useState } from 'react'
import { CheckCircle } from 'lucide-react'

import { useAuth } from '../contexts/AuthContext'
import { getApiBaseSync } from '../api/backendUrl'
import { BackRow, Showcase } from './LoginPage'
import './login-page.css'

const FeedbackPage: React.FC<{ onDone: () => void }> = ({ onDone }) => {
  const { user } = useAuth()
  const [name, setName] = useState(user?.name ?? '')
  const [email, setEmail] = useState(user?.email ?? '')
  const [message, setMessage] = useState('')
  const [busy, setBusy] = useState(false)
  const [sent, setSent] = useState(false)
  const [error, setError] = useState('')

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!message.trim()) return
    setBusy(true)
    setError('')
    try {
      const response = await fetch(`${getApiBaseSync()}/feedback`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, email, message }),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok || data.success === false) throw new Error(data.message || 'Could not send your feedback.')
      setSent(true)
      setMessage('')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not reach the app. Try again in a moment.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="login-page">
      <Showcase />
      <section className="login-form-side">
        <div className="login-card">
          <BackRow onClick={onDone} />
          {sent ? (
            <div className="login-form">
              <span className="login-done"><CheckCircle size={28} strokeWidth={1.75} /></span>
              <h1>Thank you</h1>
              <p className="login-sub">Your feedback has been sent to the team.</p>
              <button type="button" className="ui-btn ui-btn--primary ui-btn--pill login-submit" onClick={onDone}>
                Back to chat
              </button>
            </div>
          ) : (
            <form className="login-form" onSubmit={submit}>
              <h1>Give us feedback</h1>
              <p className="login-sub">Tell us what broke, what is missing, or what would make this better.</p>
              <div className="login-pair">
                <label className="login-field">
                  <span>Name</span>
                  <input value={name} onChange={e => setName(e.target.value)} autoComplete="name" />
                </label>
                <label className="login-field">
                  <span>E-mail</span>
                  <input type="email" value={email} onChange={e => setEmail(e.target.value)} autoComplete="email" />
                </label>
              </div>
              <label className="login-field">
                <span>Feedback</span>
                <textarea
                  className="login-textarea"
                  rows={7}
                  autoFocus
                  value={message}
                  onChange={e => setMessage(e.target.value)}
                  placeholder="Tell us what you think…"
                />
              </label>
              {error && <p className="login-error">{error}</p>}
              <button type="submit" className="ui-btn ui-btn--primary ui-btn--pill login-submit" disabled={busy || !message.trim()}>
                {busy ? 'Sending…' : 'Send feedback'}
              </button>
            </form>
          )}
        </div>
      </section>
    </div>
  )
}

export default FeedbackPage
