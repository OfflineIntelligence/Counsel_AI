import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { AuthProvider } from './contexts/AuthContext.tsx'
import { NotificationProvider } from './contexts/NotificationContext.tsx'
import { ThemeProvider } from './contexts/ThemeContext.tsx'
import { LoadingScreen } from './components/LoadingScreen.tsx'
import { EngineSetupGate } from './components/EngineSetupGate.tsx'

console.log('[main.tsx] Starting React app...')
console.log('[main.tsx] BACKEND_PORT_OVERRIDE:', (window as any).BACKEND_PORT_OVERRIDE)

window.onerror = (message, source, lineno, colno, error) => {
  console.error('[main.tsx] Global error:', { message, source, lineno, colno, error });
};

window.onunhandledrejection = (event) => {
  console.error('[main.tsx] Unhandled promise rejection:', event.reason);
};

try {
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <LoadingScreen>
        <EngineSetupGate>
          <AuthProvider>
            <ThemeProvider>
              <NotificationProvider>
                <App />
              </NotificationProvider>
            </ThemeProvider>
          </AuthProvider>
        </EngineSetupGate>
      </LoadingScreen>
    </StrictMode>,
  )
  console.log('[main.tsx] React app rendered successfully');
} catch (error) {
  console.error('[main.tsx] Failed to render React app:', error);
  document.getElementById('root')!.innerHTML = `
    <div style="padding: 20px; color: red; background: #1a1a2e; min-height: 100vh;">
      <h1>Error Loading Application</h1>
      <pre>${error}</pre>
    </div>
  `;
}
