/**
 * Render a component inside the providers the real app mounts.
 *
 * `main.tsx` wraps <App /> in AuthProvider → ThemeProvider →
 * NotificationProvider, and components freely call the hooks those expose.
 * Rendering such a component bare therefore throws on the first hook call —
 * `useAuth must be used within an AuthProvider` — before a single assertion
 * runs.
 *
 * That is not hypothetical: it was the cause of every failure in
 * ChatWindow.test.tsx and Sidebar.test.tsx. Both suites called
 * `render(<ChatWindow ... />)` directly, so all of their cases failed on mount
 * regardless of what they asserted. Any component that later reached for a
 * second context — as ChatWindow did when its attachment limits moved from
 * `alert()` to the toast system — simply failed the same way for one more
 * reason.
 *
 * Use this instead of RTL's `render` for anything that consumes app context.
 */
import React from 'react';
import { render, type RenderOptions, type RenderResult } from '@testing-library/react';
import { AuthProvider } from '../contexts/AuthContext';
import { ThemeProvider } from '../contexts/ThemeContext';
import { NotificationProvider } from '../contexts/NotificationContext';

/** Same nesting order as main.tsx, so context resolution matches production. */
export const AppProviders: React.FC<{ children: React.ReactNode }> = ({ children }) => (
  <AuthProvider>
    <ThemeProvider>
      <NotificationProvider>{children}</NotificationProvider>
    </ThemeProvider>
  </AuthProvider>
);

export function renderWithProviders(
  ui: React.ReactElement,
  options?: Omit<RenderOptions, 'wrapper'>,
): RenderResult {
  return render(ui, { wrapper: AppProviders, ...options });
}

export * from '@testing-library/react';
