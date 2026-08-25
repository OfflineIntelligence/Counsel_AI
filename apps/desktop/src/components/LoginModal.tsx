import React, { useState, useEffect } from 'react';
import { useAuth } from '../contexts/AuthContext';
import { loginUser, signupUser, verifyEmail, getStoredToken } from '../api/auth';

interface LoginModalProps {
  isOpen: boolean;
  onClose: () => void;
}

const LoginModal: React.FC<LoginModalProps> = ({ isOpen, onClose }) => {
  const { login, setToken } = useAuth();
  const [isLogin, setIsLogin] = useState(true);
  const [name, setName] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [successMessage, setSuccessMessage] = useState<string | null>(null);
  const [verificationToken, setVerificationToken] = useState<string | null>(null);

  useEffect(() => {
    const urlParams = new URLSearchParams(window.location.search);
    const token = urlParams.get('token');
    if (token && isOpen) {
      handleVerifyEmail(token);
    }
  }, [isOpen]);

  useEffect(() => {
    if (!isOpen) {
      setError(null);
      setSuccessMessage(null);
      setVerificationToken(null);
    }
  }, [isOpen]);

  const handleVerifyEmail = async (token: string) => {
    setIsLoading(true);
    setError(null);
    
    const result = await verifyEmail(token);
    
    if (result.success && result.user && result.token) {
      setToken(result.token);
      login(result.user.name, result.user.email);
      setSuccessMessage('Email verified successfully!');
      setTimeout(() => {
        onClose();
      }, 1500);
    } else {
      setError(result.message || 'Verification failed');
    }
    
    setIsLoading(false);
  };

  if (!isOpen) return null;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setSuccessMessage(null);
    setIsLoading(true);
    
    try {
      if (isLogin) {
        const result = await loginUser(email, password);
        
        if (result.success && result.user && result.token) {
          setToken(result.token);
          login(result.user.name, result.user.email);
          onClose();
        } else if (result.requires_verification) {
          setError('Please verify your email address before logging in. Check your inbox for the verification link.');
        } else {
          setError(result.message || 'Login failed. Please try again.');
        }
      } else {
        if (password !== confirmPassword) {
          setError("Passwords don't match");
          setIsLoading(false);
          return;
        }
        
        const result = await signupUser(name, email, password);
        
        if (result.success) {
          if (result.requires_verification) {
            setSuccessMessage(result.message || 'Account created! Please check your email to verify your account.');
          } else {
            setToken(result.token!);
            login(result.user!.name, result.user!.email);
            onClose();
          }
        } else {
          setError(result.message || 'Signup failed. Please try again.');
        }
      }
    } catch (err) {
      setError('An unexpected error occurred. Please try again.');
      console.error('Authentication error:', err);
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <div className="login-modal-overlay" style={{
      position: 'fixed',
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      backgroundColor: 'rgba(0, 0, 0, 0.5)',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      zIndex: 1000,
    }}>
      <div className="login-modal" style={{
        backgroundColor: 'var(--bg-modal)',
        borderRadius: '12px',
        padding: '24px',
        width: '90%',
        maxWidth: '400px',
        boxShadow: '0 10px 25px rgba(0, 0, 0, 0.2)',
        position: 'relative',
      }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '20px' }}>
          <h2 style={{ margin: 0, fontSize: '20px', color: 'var(--text-primary)' }}>
            {isLogin ? 'Log In' : 'Sign Up'}
          </h2>
          <button 
            onClick={onClose}
            style={{
              background: 'none',
              border: 'none',
              color: 'var(--text-muted)',
              fontSize: '18px',
              cursor: 'pointer',
              padding: '4px',
              borderRadius: '4px',
            }}
            onMouseEnter={(e) => e.currentTarget.style.backgroundColor = 'var(--bg-hover)'}
            onMouseLeave={(e) => e.currentTarget.style.backgroundColor = 'transparent'}
          >
            ×
          </button>
        </div>

        {error && (
          <div style={{
            padding: '12px',
            marginBottom: '16px',
            backgroundColor: 'var(--danger-quiet)',
            borderRadius: '8px',
            color: 'var(--danger-fg)',
            fontSize: '14px',
          }}>
            {error}
          </div>
        )}

        {successMessage && (
          <div style={{
            padding: '12px',
            marginBottom: '16px',
            backgroundColor: 'var(--success-quiet)',
            borderRadius: '8px',
            color: 'var(--success-fg)',
            fontSize: '14px',
          }}>
            {successMessage}
          </div>
        )}

        {verificationToken ? (
          <div style={{ textAlign: 'center', padding: '20px' }}>
            <p style={{ color: 'var(--text-secondary)', marginBottom: '16px' }}>
              Verifying your email address...
            </p>
          </div>
        ) : (
          <form onSubmit={handleSubmit}>
            {!isLogin && (
              <div style={{ marginBottom: '16px' }}>
                <label style={{ display: 'block', marginBottom: '6px', fontSize: '14px', color: 'var(--text-secondary)' }}>
                  Full Name
                </label>
                <input
                  type="text"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  required={!isLogin}
                  placeholder="Enter your full name"
                  disabled={isLoading}
                  style={{
                    width: '100%',
                    padding: '10px 12px',
                    borderRadius: '8px',
                    border: '1px solid var(--border-primary)',
                    backgroundColor: 'var(--bg-input)',
                    color: 'var(--text-primary)',
                    fontSize: '14px',
                    opacity: isLoading ? 0.6 : 1,
                  }}
                />
              </div>
            )}

            <div style={{ marginBottom: '16px' }}>
              <label style={{ display: 'block', marginBottom: '6px', fontSize: '14px', color: 'var(--text-secondary)' }}>
                Email Address
              </label>
              <input
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
                placeholder="your@email.com"
                disabled={isLoading}
                style={{
                  width: '100%',
                  padding: '10px 12px',
                  borderRadius: '8px',
                  border: '1px solid var(--border-primary)',
                  backgroundColor: 'var(--bg-input)',
                  color: 'var(--text-primary)',
                  fontSize: '14px',
                  opacity: isLoading ? 0.6 : 1,
                }}
              />
            </div>

            <div style={{ marginBottom: '16px' }}>
              <label style={{ display: 'block', marginBottom: '6px', fontSize: '14px', color: 'var(--text-secondary)' }}>
                Password
              </label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
                placeholder={isLogin ? "Enter your password" : "Create a password"}
                disabled={isLoading}
                style={{
                  width: '100%',
                  padding: '10px 12px',
                  borderRadius: '8px',
                  border: '1px solid var(--border-primary)',
                  backgroundColor: 'var(--bg-input)',
                  color: 'var(--text-primary)',
                  fontSize: '14px',
                  opacity: isLoading ? 0.6 : 1,
                }}
              />
            </div>

            {!isLogin && (
              <div style={{ marginBottom: '20px' }}>
                <label style={{ display: 'block', marginBottom: '6px', fontSize: '14px', color: 'var(--text-secondary)' }}>
                  Confirm Password
                </label>
                <input
                  type="password"
                  value={confirmPassword}
                  onChange={(e) => setConfirmPassword(e.target.value)}
                  required={!isLogin}
                  placeholder="Confirm your password"
                  disabled={isLoading}
                  style={{
                    width: '100%',
                    padding: '10px 12px',
                    borderRadius: '8px',
                    border: '1px solid var(--border-primary)',
                    backgroundColor: 'var(--bg-input)',
                    color: 'var(--text-primary)',
                    fontSize: '14px',
                    opacity: isLoading ? 0.6 : 1,
                  }}
                />
              </div>
            )}

            <button
              type="submit"
              disabled={isLoading}
              style={{
                width: '100%',
                padding: '12px',
                borderRadius: '8px',
                backgroundColor: 'var(--accent)',
                color: 'var(--text-on-accent)',
                border: 'none',
                fontSize: '15px',
                fontWeight: 500,
                cursor: isLoading ? 'not-allowed' : 'pointer',
                marginBottom: '16px',
                opacity: isLoading ? 0.6 : 1,
              }}
            >
              {isLoading ? (isLogin ? 'Logging in...' : 'Signing up...') : (isLogin ? 'Log In' : 'Sign Up')}
            </button>
          </form>
        )}

        <div style={{ textAlign: 'center', marginTop: '16px', fontSize: '14px' }}>
          {isLogin ? (
            <span>
              Don't have an account?{' '}
              <button
                onClick={() => { setIsLogin(false); setError(null); setSuccessMessage(null); }}
                disabled={isLoading}
                style={{
                  background: 'none',
                  border: 'none',
                  color: 'var(--accent)',
                  textDecoration: 'underline',
                  cursor: isLoading ? 'not-allowed' : 'pointer',
                  fontSize: '14px',
                  opacity: isLoading ? 0.6 : 1,
                }}
              >
                Sign up
              </button>
            </span>
          ) : (
            <span>
              Already have an account?{' '}
              <button
                onClick={() => { setIsLogin(true); setError(null); setSuccessMessage(null); }}
                disabled={isLoading}
                style={{
                  background: 'none',
                  border: 'none',
                  color: 'var(--accent)',
                  textDecoration: 'underline',
                  cursor: isLoading ? 'not-allowed' : 'pointer',
                  fontSize: '14px',
                  opacity: isLoading ? 0.6 : 1,
                }}
              >
                Log in
              </button>
            </span>
          )}
        </div>
      </div>
    </div>
  );
};

export default LoginModal;
