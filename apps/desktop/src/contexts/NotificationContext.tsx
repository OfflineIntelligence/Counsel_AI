// Notification Context
// Provides global notification state management for the application

import React, { createContext, useContext, useReducer, useCallback } from 'react';

// Notification types
export type NotificationType = 'success' | 'error' | 'warning' | 'info' | 'download';

// Notification interface
export interface Notification {
  id: string;
  type: NotificationType;
  title: string;
  message: string;
  duration?: number; // milliseconds, 0 for persistent
  timestamp: Date;
}

// Action types
type NotificationAction =
  | { type: 'ADD_NOTIFICATION'; payload: Notification }
  | { type: 'REMOVE_NOTIFICATION'; payload: string }
  | { type: 'CLEAR_ALL' };

// State interface
interface NotificationState {
  notifications: Notification[];
}

// Initial state
const initialState: NotificationState = {
  notifications: [],
};

// Reducer
function notificationReducer(state: NotificationState, action: NotificationAction): NotificationState {
  switch (action.type) {
    case 'ADD_NOTIFICATION': {
      return {
        ...state,
        notifications: [...state.notifications, action.payload],
      };
    }
    case 'REMOVE_NOTIFICATION':
      return {
        ...state,
        notifications: state.notifications.filter(n => n.id !== action.payload),
      };
    case 'CLEAR_ALL':
      return {
        ...state,
        notifications: [],
      };
    default:
      return state;
  }
}

// Context interface
interface NotificationContextType {
  notifications: Notification[];
  addNotification: (notification: Omit<Notification, 'id' | 'timestamp'>) => void;
  removeNotification: (id: string) => void;
  clearAllNotifications: () => void;
}

// Create context
const NotificationContext = createContext<NotificationContextType | undefined>(undefined);

// Provider component
export const NotificationProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [state, dispatch] = useReducer(notificationReducer, initialState);

  const addNotification = useCallback((notification: Omit<Notification, 'id' | 'timestamp'>) => {
    const id = Math.random().toString(36).substring(2, 11);
    dispatch({
      type: 'ADD_NOTIFICATION',
      payload: { ...notification, id, timestamp: new Date() },
    });

    // Auto-dismiss is deliberately NOT scheduled here.
    //
    // It used to be: this callback set its own `setTimeout(duration)` that
    // dispatched REMOVE_NOTIFICATION, while the Toast component ran a parallel
    // timer that started an exit animation and removed the toast 180ms later.
    // Two owners for one lifecycle, and the context always won the race — so
    // every toast was unmounted mid-animation and the exit transition, which
    // had been written and styled, never once played.
    //
    // The component owns dismissal now, because it is the only place that can:
    // it knows when its exit animation has finished, and it is where hovering
    // pauses the countdown. `duration` is carried on the notification and read
    // there. See components/Notifications.tsx.
  }, []);

  const removeNotification = useCallback((id: string) => {
    dispatch({ type: 'REMOVE_NOTIFICATION', payload: id });
  }, []);

  const clearAllNotifications = useCallback(() => {
    dispatch({ type: 'CLEAR_ALL' });
  }, []);

  const value: NotificationContextType = {
    notifications: state.notifications,
    addNotification,
    removeNotification,
    clearAllNotifications,
  };

  return (
    <NotificationContext.Provider value={value}>
      {children}
    </NotificationContext.Provider>
  );
};

// Hook to use notifications
// eslint-disable-next-line react-refresh/only-export-components
export const useNotifications = (): NotificationContextType => {
  const context = useContext(NotificationContext);
  if (context === undefined) {
    throw new Error('useNotifications must be used within a NotificationProvider');
  }
  return context;
};

// Helper functions for common notification types
// eslint-disable-next-line react-refresh/only-export-components
export const useNotificationHelpers = () => {
  const { addNotification } = useNotifications();

  const showSuccess = (title: string, message: string, duration = 5000) => {
    addNotification({
      type: 'success',
      title,
      message,
      duration,
    });
  };

  const showError = (title: string, message: string, duration = 0) => {
    addNotification({
      type: 'error',
      title,
      message,
      duration,
    });
  };

  const showWarning = (title: string, message: string, duration = 7000) => {
    addNotification({
      type: 'warning',
      title,
      message,
      duration,
    });
  };

  const showInfo = (title: string, message: string, duration = 5000) => {
    addNotification({
      type: 'info',
      title,
      message,
      duration,
    });
  };

  const showDownload = (title: string, message: string, duration = 6000) => {
    addNotification({
      type: 'download',
      title,
      message,
      duration,
    });
  };

  return {
    showSuccess,
    showError,
    showWarning,
    showInfo,
    showDownload,
  };
};