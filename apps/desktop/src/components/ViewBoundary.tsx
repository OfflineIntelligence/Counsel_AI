// An error boundary around the page switch in App.tsx.
//
// ── Why this exists ─────────────────────────────────────────────────────────
// The six pages that are not the chat are `React.lazy` chunks now, so that
// starting the app does not parse the Workspace, its editors, Models, the
// Vault, the Library, Settings and Help before drawing the view the user is
// actually looking at.
//
// That trade introduces a failure mode the eager version did not have: a
// chunk that cannot be read — a truncated install, a file the antivirus has
// quarantined, a half-applied update — rejects inside render. React's
// response to an uncaught render error is to unmount the whole tree, so
// without a boundary a single unreadable page file turns the entire app into
// a white window with nothing to click, and the user has no way to tell that
// the chat they were using a moment ago is still fine.
//
// With this boundary the failure is confined to the page: the rail, the window
// band and every other destination keep working, and the error says which page
// failed and offers to try again. `resetKey` is the active view, so simply
// navigating elsewhere clears the error without anyone having to press
// anything.
//
// It is a class component because `componentDidCatch` has no hook equivalent.

import React from 'react';
import { RefreshCw } from 'lucide-react';

interface Props {
    children: React.ReactNode;
    /** Changing this clears a caught error — pass the active view. */
    resetKey?: string;
}

interface State {
    error: Error | null;
}

export class ViewBoundary extends React.Component<Props, State> {
    state: State = { error: null };

    static getDerivedStateFromError(error: Error): State {
        return { error };
    }

    componentDidCatch(error: Error, info: React.ErrorInfo) {
        // Logged rather than reported: the app is offline by design, so there
        // is nowhere to send this. main.tsx installs a window.onerror that
        // writes to the same console.
        console.error('[ViewBoundary] page failed to render', error, info.componentStack);
    }

    componentDidUpdate(prev: Props) {
        if (this.state.error && prev.resetKey !== this.props.resetKey) {
            this.setState({ error: null });
        }
    }

    render() {
        if (!this.state.error) return this.props.children;
        return (
            <div className="app-view view-error" role="alert">
                <div className="view-error__card">
                    <h2 className="view-error__title">This page could not be opened</h2>
                    <p className="view-error__body">
                        Everything else still works — pick another destination in the sidebar, or try
                        again.
                    </p>
                    <p className="view-error__detail">{this.state.error.message}</p>
                    <button
                        type="button"
                        className="ui-btn ui-btn--primary"
                        onClick={() => this.setState({ error: null })}
                    >
                        <RefreshCw size={14} strokeWidth={1.75} />
                        Try again
                    </button>
                </div>
            </div>
        );
    }
}

export default ViewBoundary;
