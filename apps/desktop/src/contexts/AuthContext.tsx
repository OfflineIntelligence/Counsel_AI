// Auth Context - Manages user authentication state
import React, { createContext, useContext, useState, useEffect, useCallback } from 'react';
import { getApiBaseSync, getStoredToken, setStoredToken, clearStoredAuth, getCurrentUser, type AuthResponse } from '../api/auth';

export interface User {
  id: number;
  name: string;
  email: string;
  isAuthenticated: boolean;
  emailVerified: boolean;
  apiKeys: {
    huggingface?: string;
  };
}

interface AuthContextType {
  user: User | null;
  isLoggedIn: boolean;
  isLoading: boolean;
  login: (name: string, email: string, huggingfaceKey?: string) => void;
  logout: () => void;
  updateUser: (updates: Partial<User>) => void;
  setApiKey: (provider: 'huggingface', key: string) => void;
  setToken: (token: string) => void;
}

const AuthContext = createContext<AuthContextType | undefined>(undefined);

const STORAGE_KEY = 'offline-intelligence-user-profile';

export const AuthProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [user, setUser] = useState<User | null>(null);
  const [isLoaded, setIsLoaded] = useState(false);
  const [isLoading, setIsLoading] = useState(true);

  const sendLoginNotification = async (userName: string, userEmail: string) => {
    try {
      await fetch(`${getApiBaseSync()}/notify-login`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          user_name: userName,
          user_email: userEmail,
        }),
      });
    } catch (error) {
      console.log('Login notification not sent (backend may not support it yet)');
    }
  };

  const login = useCallback((name: string, email: string, huggingfaceKey?: string) => {
    const newUser: User = {
      id: 0,
      name: name.trim(),
      email: email.trim(),
      isAuthenticated: true,
      emailVerified: true,
      apiKeys: {
        huggingface: huggingfaceKey?.trim(),
      },
    };
    setUser(newUser);
    sendLoginNotification(newUser.name, newUser.email);
  }, []);

  const logout = useCallback(() => {
    setUser(null);
    clearStoredAuth();
    localStorage.removeItem(STORAGE_KEY);
  }, []);

  const updateUser = useCallback((updates: Partial<User>) => {
    setUser(prev => prev ? { ...prev, ...updates } : null);
  }, []);

  const setApiKey = useCallback((provider: 'huggingface', key: string) => {
    setUser(prev => {
      if (!prev) return null;
      return {
        ...prev,
        apiKeys: {
          ...prev.apiKeys,
          [provider]: key.trim() || undefined,
        },
      };
    });
  }, []);

  const setToken = useCallback((token: string) => {
    setStoredToken(token);
  }, []);

  useEffect(() => {
    const initializeAuth = async () => {
      setIsLoading(true);
      
      const stored = localStorage.getItem(STORAGE_KEY);
      const token = getStoredToken();
      
      if (stored && token) {
        try {
          const parsed = JSON.parse(stored);
          
          const tokenResponse = await getCurrentUser(token);
          if (tokenResponse && tokenResponse.success && tokenResponse.user) {
            setUser({
              id: tokenResponse.user.id,
              name: tokenResponse.user.name,
              email: tokenResponse.user.email,
              isAuthenticated: true,
              emailVerified: tokenResponse.user.email_verified,
              apiKeys: parsed.apiKeys || {},
            });
          } else {
            localStorage.removeItem(STORAGE_KEY);
            clearStoredAuth();
          }
        } catch (e) {
          console.error('Failed to parse stored user:', e);
          localStorage.removeItem(STORAGE_KEY);
          clearStoredAuth();
        }
      } else if (stored) {
        try {
          const parsed = JSON.parse(stored);
          setUser(parsed);
        } catch (e) {
          console.error('Failed to parse stored user:', e);
          localStorage.removeItem(STORAGE_KEY);
        }
      }
      
      setIsLoaded(true);
      setIsLoading(false);
    };

    initializeAuth();
  }, []);

  useEffect(() => {
    if (isLoaded && user) {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(user));
    } else if (isLoaded && !user) {
      localStorage.removeItem(STORAGE_KEY);
    }
  }, [user, isLoaded]);

  const value: AuthContextType = {
    user,
    isLoggedIn: !!user?.isAuthenticated,
    isLoading,
    login,
    logout,
    updateUser,
    setApiKey,
    setToken,
  };

  if (!isLoaded) {
    return (
      <div className="init-loading">
        <div style={{ textAlign: 'center' }}>
          <div style={{
            width: '40px',
            height: '40px',
            border: '3px solid rgba(255,255,255,0.1)',
            borderTopColor: '#00d4aa',
            borderRadius: '50%',
            animation: 'spin 1s linear infinite',
            margin: '0 auto 1rem'
          }}></div>
          <span style={{ color: 'rgba(255,255,255,0.8)' }}>Loading...</span>
        </div>
        <style>{`@keyframes spin { to { transform: rotate(360deg); } }`}</style>
      </div>
    );
  }

  return (
    <AuthContext.Provider value={value}>
      {children}
    </AuthContext.Provider>
  );
};

export const useAuth = (): AuthContextType => {
  const context = useContext(AuthContext);
  if (context === undefined) {
    throw new Error('useAuth must be used within an AuthProvider');
  }
  return context;
};
