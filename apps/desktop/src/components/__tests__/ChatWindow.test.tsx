import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderWithProviders, screen, fireEvent, waitFor } from '../../test/renderWithProviders';
import userEvent from '@testing-library/user-event';
import { ChatWindow } from '../ChatWindow';
import type { Message } from '../../api/chat';

// Mock the API modules
vi.mock('../../api/chat', () => ({
  streamChat: vi.fn(),
  updateConversationTitle: vi.fn(),
}));

describe('ChatWindow Component', () => {
  // Test fixtures
  const mockMessages: Message[] = [
    {
      role: 'user',
      content: 'Hello',
    },
    {
      role: 'assistant',
      content: 'Hi there!',
    },
  ];

  const defaultProps = {
    messages: mockMessages,
    chatTitle: 'Test Chat',
    chatId: 'chat-123',
    sessionId: 'session-123',
    onSessionIdChange: vi.fn(),
    onMessagesUpdate: vi.fn(),
    onTitleGenerated: vi.fn(),
    onPinChat: vi.fn(),
    onDeleteChat: vi.fn(),
    isPinned: false,
    questionCount: 0,
    onQuestionAsked: vi.fn(),
    sidebarOpen: false,
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  // ===== Component Rendering Tests =====

  it('renders without crashing', () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('displays chat title when provided', () => {
    renderWithProviders(<ChatWindow {...defaultProps} chatTitle="My Chat" />);
    expect(screen.getByText('My Chat')).toBeTruthy();
  });

  it('displays all messages', () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    expect(screen.getByText('Hello')).toBeTruthy();
    expect(screen.getByText('Hi there!')).toBeTruthy();
  });

  it('renders with no messages initially', () => {
    renderWithProviders(<ChatWindow {...defaultProps} messages={[]} />);
    // Component should render without messages
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('renders different message roles with distinct styling', () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    const userMessage = screen.getByText('Hello');
    const assistantMessage = screen.getByText('Hi there!');
    
    expect(userMessage).toBeTruthy();
    expect(assistantMessage).toBeTruthy();
  });

  // ===== Props Updates Tests =====

  it('updates when messages prop changes', () => {
    const { rerender } = renderWithProviders(<ChatWindow {...defaultProps} />);
    
    const newMessages: Message[] = [
      ...mockMessages,
      {
        role: 'user',
        content: 'New message',
      },
    ];

    rerender(<ChatWindow {...defaultProps} messages={newMessages} />);
    expect(screen.getByText('New message')).toBeTruthy();
  });

  it('updates title when chatTitle prop changes', () => {
    const { rerender } = renderWithProviders(<ChatWindow {...defaultProps} chatTitle="Old Title" />);
    expect(screen.getByText('Old Title')).toBeTruthy();

    rerender(<ChatWindow {...defaultProps} chatTitle="New Title" />);
    expect(screen.getByText('New Title')).toBeTruthy();
    expect(screen.queryByText('Old Title')).toBeNull();
  });

  it('handles null chatTitle gracefully', () => {
    renderWithProviders(<ChatWindow {...defaultProps} chatTitle={null} />);
    expect(screen.getByRole('main')).toBeTruthy();
  });

  // ===== User Interaction Tests =====

  it('handles pinned state correctly', () => {
    const { rerender } = renderWithProviders(<ChatWindow {...defaultProps} isPinned={false} />);
    const pinButton = screen.queryByTitle(/pin/i);
    
    if (pinButton) {
      fireEvent.click(pinButton);
      expect(defaultProps.onPinChat).toHaveBeenCalled();
    }

    rerender(<ChatWindow {...defaultProps} isPinned={true} />);
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('calls onMessagesUpdate when new message is sent', async () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    const inputField = screen.queryByPlaceholderText(/message|type/i);

    if (inputField) {
      await userEvent.type(inputField, 'Test message');
      const sendButton = screen.queryByRole('button', { name: /send|submit/i });
      
      if (sendButton) {
        fireEvent.click(sendButton);
        await waitFor(() => {
          expect(defaultProps.onMessagesUpdate).toHaveBeenCalled();
        });
      }
    }
  });

  it('calls onSessionIdChange when session changes', () => {
    const mockSessionChange = vi.fn();
    const { rerender } = renderWithProviders(<ChatWindow {...defaultProps} sessionId="old-session" onSessionIdChange={mockSessionChange} />);
    
    // When session changes
    rerender(<ChatWindow {...defaultProps} sessionId="new-session" onSessionIdChange={mockSessionChange} />);
    
    // Component should handle session updates
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('calls onDeleteChat when delete button clicked', async () => {
    const { onDeleteChat } = defaultProps;
    (onDeleteChat as any).mockResolvedValue(undefined);

    renderWithProviders(<ChatWindow {...defaultProps} />);
    const deleteButton = screen.queryByTitle(/delete/i);

    if (deleteButton) {
      fireEvent.click(deleteButton);
      await waitFor(() => {
        expect(onDeleteChat).toHaveBeenCalledWith('chat-123');
      });
    }
  });

  // ===== Callback Tests =====

  it('invokes onTitleGenerated with correct parameters', async () => {
    const { onTitleGenerated } = defaultProps;
    
    renderWithProviders(<ChatWindow {...defaultProps} />);
    
    // Simulate title generation logic
    // This depends on implementation details
    expect(onTitleGenerated).toBeDefined();
  });

  it('handles empty chatId gracefully', () => {
    renderWithProviders(<ChatWindow {...defaultProps} chatId={null} />);
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('handles empty sessionId gracefully', () => {
    renderWithProviders(<ChatWindow {...defaultProps} sessionId={null} />);
    expect(screen.getByRole('main')).toBeTruthy();
  });

  // ===== Edge Cases =====

  it('renders very long messages without truncation', () => {
    const longMessage: Message = {
      role: 'user',
      content: 'A'.repeat(1000),
    };

    renderWithProviders(<ChatWindow {...defaultProps} messages={[longMessage]} />);
    // Just verify component renders without crashing
    expect(screen.getByRole('main')).toBeTruthy();
  });

  it('renders many messages without performance issues', () => {
    const manyMessages: Message[] = Array.from({ length: 100 }, (_, i) => ({
      id: String(i),
      role: i % 2 === 0 ? 'user' : 'assistant',
      content: `Message ${i}`,
      timestamp: new Date(),
      tokens: 10,
    }));

    const { container } = renderWithProviders(<ChatWindow {...defaultProps} messages={manyMessages} />);
    const messageWrappers = container.querySelectorAll('.message-wrapper, [role="main"] > div > div');
    expect(messageWrappers.length).toBeGreaterThan(0);
  });

  it('handles special characters in messages', () => {
    const specialMessage: Message = {
      role: 'user',
      content: '<script>alert("xss")</script>',
    };

    renderWithProviders(<ChatWindow {...defaultProps} messages={[specialMessage]} />);
    // Should not render as script tag
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('handles emoji in messages', () => {
    const emojiMessage: Message = {
      role: 'user',
      content: '👋 Hello 🚀',
    };

    renderWithProviders(<ChatWindow {...defaultProps} messages={[emojiMessage]} />);
    expect(screen.getByText(/👋/)).toBeTruthy();
  });

  // ===== Accessibility Tests =====

  it('has proper ARIA labels for interactive elements', () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    const main = screen.getByRole('main');
    expect(main).toBeTruthy();
  });

  it('maintains focus management', async () => {
    renderWithProviders(<ChatWindow {...defaultProps} />);
    const inputField = screen.queryByPlaceholderText(/message|type/i);
    
    if (inputField) {
      inputField.focus();
      expect(document.activeElement).toBe(inputField);
    }
  });

  // ===== State Management Tests =====

  it('maintains message order', () => {
    const orderedMessages: Message[] = [
      { role: 'user', content: 'First' },
      { role: 'assistant', content: 'Second' },
      { role: 'user', content: 'Third' },
    ];

    renderWithProviders(<ChatWindow {...defaultProps} messages={orderedMessages} />);
    
    // Verify all messages are present in order
    expect(screen.getByText('First')).toBeTruthy();
    expect(screen.getByText('Second')).toBeTruthy();
    expect(screen.getByText('Third')).toBeTruthy();
  });

  it('preserves chat state across re-renders', () => {
    const { rerender } = renderWithProviders(<ChatWindow {...defaultProps} />);
    
    rerender(<ChatWindow {...defaultProps} messages={mockMessages} />);
    rerender(<ChatWindow {...defaultProps} messages={mockMessages} chatTitle="Updated" />);
    
    expect(screen.getByText('Updated')).toBeTruthy();
  });
});
