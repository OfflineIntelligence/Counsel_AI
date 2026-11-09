/**
 * The login page: a space scene on the left, the form on the right.
 *
 * Steps on the right:
 *   sign in  — Continue with Google, Continue with Microsoft (Outlook,
 *              Hotmail, Microsoft 365), or any e-mail address: we e-mail a
 *              six-digit code. A password still works ("Use a password
 *              instead");
 *   code     — the sign-in code that was e-mailed;
 *   password — user name (or e-mail) and password;
 *   create   — first and last name, e-mail, user name, password, age and date
 *              of birth, for an address with no account on this computer;
 *   verify   — the code that confirms the new account's e-mail;
 *   age      — date of birth, for a sign-in that has none on record
 *              (Google / Microsoft sign-ups, older accounts);
 *   blocked  — the neutral notice after a failed age check.
 *
 * Age: the app is for adults. The date of birth is asked neutrally (the
 * form never says where the line is) and the backend decides
 * (api/age_gate.rs). A failed check locks this computer for 24 hours, here
 * as well as in the backend, so the date cannot just be changed and resent.
 *
 * `gated`: shown at launch on a computer that has an account; there is no
 * way back into the app until someone signs in.
 */
import React, { useEffect, useRef, useState } from 'react'
import { ArrowLeft } from 'lucide-react'
import { open as openExternal } from '@tauri-apps/plugin-shell'

import { useAuth } from '../contexts/AuthContext'
import {
  AgeBlockedError,
  confirmAge,
  getSignInMethods,
  needsDob,
  signIn,
  signInWithProvider,
  startCodeSignIn,
  startRegistration,
  verifyCodeSignIn,
  verifyRegistration,
  type SignInMethods,
  type SignInResult,
} from '../api/account'
import AppMark from './AppMark'
import './login-page.css'

type Step = 'signin' | 'code' | 'password' | 'create' | 'verify' | 'age' | 'blocked'

const AGE_LOCK_KEY = 'oca.ageLockedUntil'
const AGE_LOCK_MS = 24 * 60 * 60 * 1000

function ageLocked(): boolean {
  try {
    return Number(localStorage.getItem(AGE_LOCK_KEY) ?? 0) > Date.now()
  } catch {
    return false
  }
}

function ageFrom(dob: string): number | null {
  const d = new Date(dob)
  if (Number.isNaN(d.getTime())) return null
  const now = new Date()
  let age = now.getFullYear() - d.getFullYear()
  if (now.getMonth() < d.getMonth() || (now.getMonth() === d.getMonth() && now.getDate() < d.getDate())) age -= 1
  return age
}

const LoginPage: React.FC<{ onDone: () => void; gated?: boolean }> = ({ onDone, gated = false }) => {
  const { user, login, setToken, logout } = useAuth()
  const [step, setStep] = useState<Step>(() => (ageLocked() ? 'blocked' : 'signin'))
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [info, setInfo] = useState('')

  const [identifier, setIdentifier] = useState('')
  const [password, setPassword] = useState('')

  const [first, setFirst] = useState('')
  const [last, setLast] = useState('')
  const [email, setEmail] = useState('')
  const [username, setUsername] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [dob, setDob] = useState('')
  const [attest, setAttest] = useState(false)
  const [ticket, setTicket] = useState('')
  const [code, setCode] = useState('')
  const [signInCode, setSignInCode] = useState('')

  const [methods, setMethods] = useState<SignInMethods | null>(null)
  const [waitingFor, setWaitingFor] = useState<'google' | 'microsoft' | null>(null)
  const ssoAbort = useRef<AbortController | null>(null)

  useEffect(() => {
    void getSignInMethods().then(setMethods)
    return () => ssoAbort.current?.abort()
  }, [])

  const go = (next: Step) => {
    setStep(next)
    setError('')
  }

  const finish = (result: SignInResult) => {
    // No date of birth on record yet: ask for it before the session.
    if (needsDob(result)) {
      setTicket(result.ticket)
      setDob('')
      setAttest(false)
      go('age')
      return
    }
    setToken(result.token)
    login(result.user.name, result.user.email)
    onDone()
  }

  const blocked = () => {
    try {
      localStorage.setItem(AGE_LOCK_KEY, String(Date.now() + AGE_LOCK_MS))
    } catch {
      /* the backend keeps its own lock */
    }
    setStep('blocked')
  }

  const run = async (work: () => Promise<void>) => {
    setBusy(true)
    setError('')
    try {
      await work()
    } catch (e) {
      if (e instanceof AgeBlockedError) blocked()
      else setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const submitAge = (e: React.FormEvent) => {
    e.preventDefault()
    void run(async () => finish(await confirmAge(ticket, dob, attest)))
  }

  // Google / Microsoft: the provider opens in the browser; this waits for it.
  const sso = (provider: 'google' | 'microsoft') => {
    ssoAbort.current?.abort()
    const ctrl = new AbortController()
    ssoAbort.current = ctrl
    setWaitingFor(provider)
    void run(async () => {
      try {
        const session = await signInWithProvider(
          provider,
          url => ('__TAURI_INTERNALS__' in window ? openExternal(url) : void window.open(url, '_blank', 'noopener')),
          ctrl.signal,
        )
        finish(session)
      } finally {
        setWaitingFor(null)
      }
    })
  }

  const cancelSso = () => {
    ssoAbort.current?.abort()
    setWaitingFor(null)
    setBusy(false)
  }

  // Any e-mail address: a code by e-mail.
  const sendSignInCode = (e?: React.FormEvent) => {
    e?.preventDefault()
    setBusy(true)
    setError('')
    startCodeSignIn(email)
      .then(() => {
        setInfo(`We sent a 6-digit code to ${email.trim()}.`)
        setSignInCode('')
        setStep('code')
      })
      .catch((err: Error & { needsAccount?: boolean }) => {
        if (err.needsAccount) {
          // New here: the same address carries over to Create account.
          setStep('create')
          setError('There is no account for this e-mail on this computer yet. Create one below.')
        } else {
          setError(err.message)
        }
      })
      .finally(() => setBusy(false))
  }

  const submitSignInCode = (e: React.FormEvent) => {
    e.preventDefault()
    void run(async () => finish(await verifyCodeSignIn(email, signInCode)))
  }

  const submitPassword = (e: React.FormEvent) => {
    e.preventDefault()
    void run(async () => finish(await signIn(identifier, password)))
  }

  const sendCode = (e?: React.FormEvent) => {
    e?.preventDefault()
    void run(async () => {
      await startRegistration({
        first_name: first,
        last_name: last,
        email,
        username,
        password: newPassword,
        // The backend computes the age from the date of birth.
        age: ageFrom(dob) ?? 0,
        dob,
      })
      setInfo(`We sent a 6-digit code to ${email.trim()}.`)
      setStep('verify')
    })
  }

  const submitCode = (e: React.FormEvent) => {
    e.preventDefault()
    void run(async () => finish(await verifyRegistration(email, code)))
  }

  // Already signed in: the account, and the way out.
  if (user) {
    return (
      <div className="login-page">
        <Showcase />
        <section className="login-form-side">
          <div className="login-card">
            <h1>{user.name}</h1>
            <p className="login-sub">{user.email}</p>
            <div className="login-actions">
              <button type="button" className="ui-btn ui-btn--secondary ui-btn--pill" onClick={onDone}>
                Back to chat
              </button>
              <button type="button" className="ui-btn ui-btn--primary ui-btn--pill" onClick={logout}>
                Sign out
              </button>
            </div>
          </div>
        </section>
      </div>
    )
  }

  return (
    <div className="login-page">
      <Showcase />
      <section className="login-form-side">
        <div className="login-card">

          {step === 'signin' && (
            <div className="login-form">
              <p className="login-sub login-welcome">Welcome to Offline Counsel AI.</p>
              <div className="login-sso">
                <button
                  type="button"
                  className="login-sso__btn"
                  onClick={() => sso('google')}
                  disabled={busy || !methods?.google}
                  title={methods?.google ? undefined : 'Google sign-in is not set up on this computer'}
                >
                  <GoogleMark />
                  {waitingFor === 'google' ? 'Waiting for Google…' : 'Continue with Google'}
                </button>
                <button
                  type="button"
                  className="login-sso__btn"
                  onClick={() => sso('microsoft')}
                  disabled={busy || !methods?.microsoft}
                  title={methods?.microsoft ? undefined : 'Microsoft sign-in is not set up on this computer'}
                >
                  <MicrosoftMark />
                  {waitingFor === 'microsoft' ? 'Waiting for Microsoft…' : 'Continue with Microsoft'}
                </button>
              </div>
              {waitingFor && (
                <p className="login-hint">
                  Finish signing in in your browser, then come back here.{' '}
                  <button type="button" className="login-link" onClick={cancelSso}>Cancel</button>
                </p>
              )}
              <div className="login-or"><span>or</span></div>
              <form onSubmit={sendSignInCode} className="login-form login-form--inner">
                <Field label="E-mail">
                  <input type="email" value={email} onChange={e => setEmail(e.target.value)} autoComplete="email" placeholder="you@firm.com" />
                </Field>
                {error && <p className="login-error">{error}</p>}
                <button
                  type="submit"
                  className="ui-btn ui-btn--primary ui-btn--pill login-submit"
                  disabled={busy || !email.includes('@') || methods?.email_code === false}
                  title={methods?.email_code === false ? 'E-mail is not set up on this computer' : undefined}
                >
                  {busy && !waitingFor ? 'Sending code…' : 'E-mail me a sign-in code'}
                </button>
              </form>
              <p className="login-switch">
                <button type="button" onClick={() => go('password')}>Use a password instead</button>
                {' · '}
                <button type="button" onClick={() => go('create')}>Create account</button>
              </p>
            </div>
          )}

          {step === 'code' && (
            <form onSubmit={submitSignInCode} className="login-form">
              <h1>Check your e-mail</h1>
              <p className="login-sub">{info}</p>
              <Field label="6-digit code">
                <input
                  autoFocus
                  inputMode="numeric"
                  maxLength={6}
                  className="login-code"
                  value={signInCode}
                  onChange={e => setSignInCode(e.target.value.replace(/\D/g, ''))}
                  autoComplete="one-time-code"
                />
              </Field>
              {error && <p className="login-error">{error}</p>}
              <button type="submit" className="ui-btn ui-btn--primary ui-btn--pill login-submit" disabled={busy || signInCode.length !== 6}>
                {busy ? 'Signing in…' : 'Sign in'}
              </button>
              <p className="login-switch">
                <button type="button" disabled={busy} onClick={() => sendSignInCode()}>Send a new code</button>
                {' · '}
                <button type="button" onClick={() => go('signin')}>Use another e-mail</button>
              </p>
            </form>
          )}

          {step === 'password' && (
            <form onSubmit={submitPassword} className="login-form">
              <h1>Sign in with a password</h1>
              <p className="login-sub">Your user name or e-mail, and your password.</p>
              <Field label="User name or e-mail">
                <input autoFocus value={identifier} onChange={e => setIdentifier(e.target.value)} autoComplete="username" />
              </Field>
              <Field label="Password">
                <input type="password" value={password} onChange={e => setPassword(e.target.value)} autoComplete="current-password" />
              </Field>
              {error && <p className="login-error">{error}</p>}
              <button type="submit" className="ui-btn ui-btn--primary ui-btn--pill login-submit" disabled={busy || !identifier || !password}>
                {busy ? 'Signing in…' : 'Sign in'}
              </button>
              <p className="login-switch">
                <button type="button" onClick={() => go('signin')}>Other ways to sign in</button>
              </p>
            </form>
          )}

          {step === 'create' && (
            <form onSubmit={sendCode} className="login-form">
              <h1>Create your account</h1>
              <p className="login-sub">We verify your e-mail with a one-time code.</p>
              <div className="login-pair">
                <Field label="First name">
                  <input autoFocus value={first} onChange={e => setFirst(e.target.value)} autoComplete="given-name" />
                </Field>
                <Field label="Last name">
                  <input value={last} onChange={e => setLast(e.target.value)} autoComplete="family-name" />
                </Field>
              </div>
              <Field label="E-mail">
                <input type="email" value={email} onChange={e => setEmail(e.target.value)} autoComplete="email" />
              </Field>
              <div className="login-pair">
                <Field label="User name">
                  <input value={username} onChange={e => setUsername(e.target.value)} autoComplete="username" />
                </Field>
                <Field label="Password">
                  <input type="password" value={newPassword} onChange={e => setNewPassword(e.target.value)} autoComplete="new-password" />
                </Field>
              </div>
              <Field label="Date of birth">
                <DobInput value={dob} onChange={setDob} />
              </Field>
              <Attest checked={attest} onChange={setAttest} />
              {error && <p className="login-error">{error}</p>}
              <button
                type="submit"
                className="ui-btn ui-btn--primary ui-btn--pill login-submit"
                disabled={busy || !first || !last || !email || !username || !newPassword || !dob || !attest}
              >
                {busy ? 'Sending code…' : 'Send code'}
              </button>
              <p className="login-switch">
                Already have an account here?{' '}
                <button type="button" onClick={() => go('signin')}>Sign in</button>
              </p>
            </form>
          )}

          {step === 'age' && (
            <form onSubmit={submitAge} className="login-form">
              <h1>One more thing</h1>
              <p className="login-sub">Enter your date of birth to finish signing in.</p>
              <Field label="Date of birth">
                <DobInput value={dob} onChange={setDob} autoFocus />
              </Field>
              <Attest checked={attest} onChange={setAttest} />
              {error && <p className="login-error">{error}</p>}
              <button type="submit" className="ui-btn ui-btn--primary ui-btn--pill login-submit" disabled={busy || !dob || !attest}>
                {busy ? 'Checking…' : 'Continue'}
              </button>
              <p className="login-switch">
                <button type="button" onClick={() => go('signin')}>Cancel</button>
              </p>
            </form>
          )}

          {step === 'blocked' && (
            <div className="login-form">
              <h1>Sorry</h1>
              <p className="login-sub">
                You’re not eligible to use Offline Counsel AI. If you think this is a mistake, contact
                product@offlineintelligence.io.
              </p>
            </div>
          )}

          {step === 'verify' && (
            <form onSubmit={submitCode} className="login-form">
              <h1>Check your e-mail</h1>
              <p className="login-sub">{info}</p>
              <Field label="6-digit code">
                <input
                  autoFocus
                  inputMode="numeric"
                  maxLength={6}
                  className="login-code"
                  value={code}
                  onChange={e => setCode(e.target.value.replace(/\D/g, ''))}
                  autoComplete="one-time-code"
                />
              </Field>
              {error && <p className="login-error">{error}</p>}
              <button type="submit" className="ui-btn ui-btn--primary ui-btn--pill login-submit" disabled={busy || code.length !== 6}>
                {busy ? 'Verifying…' : 'Verify and continue'}
              </button>
              <p className="login-switch">
                <button type="button" disabled={busy} onClick={() => sendCode()}>Send a new code</button>
                {' · '}
                <button type="button" onClick={() => go('create')}>Change details</button>
              </p>
            </form>
          )}
        </div>
      </section>
    </div>
  )
}

/** Back to Chat window: the arrow is the button, the words sit beside it. */
export const BackRow: React.FC<{ onClick: () => void }> = ({ onClick }) => (
  <div className="login-back">
    <button type="button" className="ws-back" onClick={onClick} aria-label="Back to Chat window" title="Back to Chat window">
      <ArrowLeft size={15} strokeWidth={1.75} aria-hidden="true" />
    </button>
    <span>Back to Chat window</span>
  </div>
)

/** Date of birth, asked neutrally: empty, no hint of the age line, and no
 *  date later than today. */
const DobInput: React.FC<{ value: string; onChange: (v: string) => void; autoFocus?: boolean }> = ({ value, onChange, autoFocus }) => (
  <input
    type="date"
    value={value}
    autoFocus={autoFocus}
    min="1900-01-01"
    max={new Date().toISOString().slice(0, 10)}
    onChange={e => onChange(e.target.value)}
  />
)

/** The user's own statement that the date is true, and the terms. */
const Attest: React.FC<{ checked: boolean; onChange: (v: boolean) => void }> = ({ checked, onChange }) => (
  <label className="login-attest">
    <input type="checkbox" checked={checked} onChange={e => onChange(e.target.checked)} />
    <span>I confirm my date of birth is accurate, and I agree to the Terms of Use and Privacy Policy.</span>
  </label>
)

const Field: React.FC<{ label: string; children: React.ReactNode }> = ({ label, children }) => (
  <label className="login-field">
    <span>{label}</span>
    {children}
  </label>
)

/** Google's "G", as its sign-in branding asks buttons to show it. */
const GoogleMark: React.FC = () => (
  <svg width="18" height="18" viewBox="0 0 48 48" aria-hidden="true">
    <path fill="#EA4335" d="M24 9.5c3.5 0 6.6 1.2 9.1 3.6l6.8-6.8C35.8 2.4 30.3 0 24 0 14.6 0 6.6 5.4 2.7 13.3l7.9 6.1C12.5 13.6 17.8 9.5 24 9.5z" />
    <path fill="#4285F4" d="M46.1 24.5c0-1.6-.1-3.1-.4-4.5H24v9h12.4c-.5 2.9-2.2 5.3-4.6 6.9l7.4 5.7c4.3-4 6.9-9.9 6.9-17.1z" />
    <path fill="#FBBC05" d="M10.6 28.6c-.5-1.4-.8-3-.8-4.6s.3-3.2.8-4.6l-7.9-6.1C1 16.6 0 20.2 0 24s1 7.4 2.7 10.7l7.9-6.1z" />
    <path fill="#34A853" d="M24 48c6.5 0 11.9-2.1 15.9-5.8l-7.4-5.7c-2.1 1.4-4.8 2.3-8.5 2.3-6.2 0-11.5-4.1-13.4-9.9l-7.9 6.1C6.6 42.6 14.6 48 24 48z" />
  </svg>
)

/** Microsoft's four squares, as its sign-in branding asks. */
const MicrosoftMark: React.FC = () => (
  <svg width="16" height="16" viewBox="0 0 21 21" aria-hidden="true">
    <rect x="1" y="1" width="9" height="9" fill="#F25022" />
    <rect x="11" y="1" width="9" height="9" fill="#7FBA00" />
    <rect x="1" y="11" width="9" height="9" fill="#00A4EF" />
    <rect x="11" y="11" width="9" height="9" fill="#FFB900" />
  </svg>
)

/** The left side: deep space, a slow star field, and nine bodies orbiting
 *  the app's mark on invisible paths. */
export const Showcase: React.FC = () => (
  <aside className="login-showcase" aria-hidden="true">
    <span className="login-stars login-stars--far" />
    <span className="login-stars login-stars--near" />
    <span className="login-nebula" />
    <div className="login-orbits">
      <span className="login-ring login-ring--1"><i /></span>
      <span className="login-ring login-ring--2"><i /></span>
      <span className="login-ring login-ring--3"><i /></span>
      <span className="login-ring login-ring--4"><i /></span>
      <span className="login-ring login-ring--5"><i /></span>
      <span className="login-ring login-ring--6"><i /></span>
      <span className="login-ring login-ring--7"><i /></span>
      <span className="login-ring login-ring--8"><i /></span>
      <span className="login-ring login-ring--9"><i /></span>
      <span className="login-mark"><AppMark size={46} /></span>
    </div>
  </aside>
)

export default LoginPage
