import React, { createContext, useContext, useState, useEffect } from 'react';

type Theme = 'light' | 'dark';
/** What the person chose: a fixed theme, or whatever the OS is using. */
export type ThemePreference = 'system' | Theme;

interface ThemeContextType {
  /** The theme on screen now. */
  theme: Theme;
  preference: ThemePreference;
  setPreference: (p: ThemePreference) => void;
  toggleTheme: () => void;
}

const ThemeContext = createContext<ThemeContextType | undefined>(undefined);

// Key must match the pre-paint script in index.html.
const STORAGE_KEY = 'offline-intelligence-theme';
const DARK_QUERY = '(prefers-color-scheme: dark)';

const systemTheme = (): Theme =>
  window.matchMedia?.(DARK_QUERY).matches ? 'dark' : 'light';

export const ThemeProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [preference, setPreference] = useState<ThemePreference>(() => {
    // The default follows the OS appearance (Apple HIG: respect the system
    // setting). A saved fixed choice still wins.
    try {
      const saved = localStorage.getItem(STORAGE_KEY);
      return saved === 'dark' || saved === 'light' ? saved : 'system';
    } catch {
      return 'system';
    }
  });
  const [system, setSystem] = useState<Theme>(systemTheme);

  // Follow the OS live, so switching appearance there re-themes the app at once.
  useEffect(() => {
    const query = window.matchMedia?.(DARK_QUERY);
    if (!query) return;
    const onChange = () => setSystem(query.matches ? 'dark' : 'light');
    query.addEventListener('change', onChange);
    return () => query.removeEventListener('change', onChange);
  }, []);

  const theme = preference === 'system' ? system : preference;

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, preference);
    } catch {
      /* Storage unavailable: the choice lasts for this session only. */
    }
    document.documentElement.setAttribute('data-theme', theme);
  }, [preference, theme]);

  const toggleTheme = () => setPreference(theme === 'dark' ? 'light' : 'dark');

  return (
    <ThemeContext.Provider value={{ theme, preference, setPreference, toggleTheme }}>
      {children}
    </ThemeContext.Provider>
  );
};

// eslint-disable-next-line react-refresh/only-export-components
export const useTheme = (): ThemeContextType => {
  const context = useContext(ThemeContext);
  if (!context) throw new Error('useTheme must be used within a ThemeProvider');
  return context;
};
