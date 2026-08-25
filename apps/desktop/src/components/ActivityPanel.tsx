import React, { useState, useMemo } from 'react';

interface ActivityConversation {
  id: string;
  title: string;
  created_at: string;
}

interface ActivityPanelProps {
  isOpen: boolean;
  onClose: () => void;
  conversations: ActivityConversation[];
  onDeleteConversation: (id: string) => void;
  onDeleteByAge: (olderThanDays: number) => void;
  activityEnabled: boolean;
  onToggleActivity: (enabled: boolean) => void;
  autoDeletePeriod: string;
  onAutoDeletePeriodChange: (period: string) => void;
}

const DELETE_PERIOD_OPTIONS = [
  { value: 'never', label: 'Never' },
  { value: '1d', label: '1 day' },
  { value: '7d', label: '1 week' },
  { value: '14d', label: '2 weeks' },
  { value: '30d', label: '1 month' },
  { value: '90d', label: '3 months' },
  { value: '180d', label: '6 months' },
  { value: '365d', label: '1 year' },
  { value: '540d', label: '18 months' },
  { value: '730d', label: '2 years' },
];

const ActivityPanel: React.FC<ActivityPanelProps> = ({
  isOpen,
  onClose,
  conversations,
  onDeleteConversation,
  onDeleteByAge,
  activityEnabled,
  onToggleActivity,
  autoDeletePeriod,
  onAutoDeletePeriodChange,
}) => {
  const [expandedDay, setExpandedDay] = useState<string | null>(null);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState<string | null>(null);

  // Group all conversations by day (must be before early return to satisfy Rules of Hooks)
  const grouped = useMemo(() => {
    const groups: Record<string, ActivityConversation[]> = {};

    conversations.forEach(conv => {
      const day = conv.created_at.slice(0, 10);
      if (!groups[day]) groups[day] = [];
      groups[day].push(conv);
    });

    return Object.entries(groups)
      .filter(([, convs]) => convs.length > 0)
      .sort(([a], [b]) => b.localeCompare(a));
  }, [conversations]);

  if (!isOpen) return null;

  const formatDate = (dateStr: string) => {
    const date = new Date(dateStr);
    const today = new Date();
    const yesterday = new Date(today);
    yesterday.setDate(yesterday.getDate() - 1);

    if (dateStr === today.toISOString().slice(0, 10)) return 'Today';
    if (dateStr === yesterday.toISOString().slice(0, 10)) return 'Yesterday';
    return date.toLocaleDateString('en-US', { weekday: 'long', month: 'short', day: 'numeric' });
  };

  return (
    <div className="activity-overlay">
      <div className="activity-panel">
        {/* Header */}
        <div className="activity-header">
          <h2>Activity</h2>
          <button className="activity-close-btn" onClick={onClose}>
            <svg width="20" height="20" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>

        <div className="activity-content">
          {/* Privacy Notice */}
          <div className="activity-notice">
            <svg width="16" height="16" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 16h-1v-4h-1m1-4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            <p>We don't collect your information or data. Activity tracking is only used to help you find previous conversations and improve the system's ability to retrieve context. All data stays on your device.</p>
          </div>

          {/* Keep Activity Toggle */}
          <div className="activity-setting-row">
            <div className="activity-setting-info">
              <h3>Keep Activity</h3>
              <p>Store conversation history for context retrieval</p>
            </div>
            <button
              className={`activity-toggle ${activityEnabled ? 'active' : ''}`}
              onClick={() => onToggleActivity(!activityEnabled)}
              aria-label={activityEnabled ? 'Disable activity' : 'Enable activity'}
            >
              <div className="activity-toggle-thumb" />
            </button>
          </div>

          {/* Auto-Delete Period */}
          <div className="activity-setting-row">
            <div className="activity-setting-info">
              <h3>Auto-delete activity older than</h3>
              <p>Automatically remove old conversations</p>
            </div>
            <select
              className="activity-select"
              value={autoDeletePeriod}
              onChange={(e) => onAutoDeletePeriodChange(e.target.value)}
            >
              {DELETE_PERIOD_OPTIONS.map(opt => (
                <option key={opt.value} value={opt.value}>{opt.label}</option>
              ))}
            </select>
          </div>

          {/* Delete All Activity */}
          <div className="activity-setting-row">
            <div className="activity-setting-info">
              <h3>Delete activity</h3>
              <p>Remove all conversation history</p>
            </div>
            <button
              className="activity-delete-btn"
              onClick={() => setShowDeleteConfirm('all')}
            >
              Delete all activity
            </button>
          </div>

          {/* Activity History */}
          <div className="activity-recent">
            <h3>Activity ({conversations.length} conversation{conversations.length !== 1 ? 's' : ''})</h3>
            {grouped.length === 0 && (
              <p className="activity-empty">No activity yet. Start a conversation to see it here.</p>
            )}
            {grouped.map(([day, convs]) => (
              <div key={day} className="activity-day-group">
                <button
                  className="activity-day-header"
                  onClick={() => setExpandedDay(expandedDay === day ? null : day)}
                >
                  <span>{formatDate(day)}</span>
                  <span className="activity-day-count">{convs.length} conversation{convs.length !== 1 ? 's' : ''}</span>
                  <svg
                    width="16" height="16" fill="none" stroke="currentColor" viewBox="0 0 24 24"
                    className={`activity-chevron ${expandedDay === day ? 'expanded' : ''}`}
                  >
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 9l-7 7-7-7" />
                  </svg>
                </button>
                {expandedDay === day && (
                  <ul className="activity-day-list">
                    {convs.map(conv => (
                      <li key={conv.id} className="activity-conv-item">
                        <span className="activity-conv-title">{conv.title}</span>
                        <button
                          className="activity-conv-delete"
                          onClick={() => onDeleteConversation(conv.id)}
                          title="Delete conversation"
                        >
                          <svg width="14" height="14" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                          </svg>
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            ))}
          </div>
        </div>

        {/* Delete Confirmation */}
        {showDeleteConfirm && (
          <div className="activity-confirm-overlay">
            <div className="activity-confirm">
              <h3>Delete all activity?</h3>
              <p>This will permanently remove all conversation history. This action cannot be undone.</p>
              <div className="activity-confirm-buttons">
                <button className="activity-confirm-cancel" onClick={() => setShowDeleteConfirm(null)}>Cancel</button>
                <button className="activity-confirm-delete" onClick={() => { onDeleteByAge(0); setShowDeleteConfirm(null); }}>Delete</button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
};

export default ActivityPanel;
