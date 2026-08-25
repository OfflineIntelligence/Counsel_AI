/* eslint-disable @typescript-eslint/no-explicit-any */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { streamChat, getConversations, createNewConversation, deleteConversation, updateConversationTitle } from '../chat';
import type { Message, ConversationSummary } from '../chat';

// Mock fetch globally
global.fetch = vi.fn();

describe('Chat API', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  // ===== streamChat Tests =====

  describe('streamChat', () => {
    it('throws error when sessionId is missing', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Hello' },
      ];

      const generator = streamChat(messages);
      
      try {
        await generator.next();
        expect.fail('Should have thrown error');
      } catch (error: any) {
        expect(error.message).toContain('Session ID');
      }
    });

    it('makes POST request with correct URL', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Hello' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      const generator = streamChat(messages, 'session-123');
      try {
        await generator.next();
      } catch {
        // Expected - mock may not complete fully
      }

      expect(global.fetch).toHaveBeenCalledWith(
        expect.stringContaining('/generate/stream'),
        expect.any(Object)
      );
    });

    it('sends correct request headers', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Test' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      try {
        const generator = streamChat(messages, 'session-123');
        await generator.next();
      } catch {
        // Expected
      }

      const callArgs = (global.fetch as any).mock.calls[0];
      expect(callArgs[1].headers['Content-Type']).toBe('application/json');
    });

    it('sends messages and session ID in request body', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Hello' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      try {
        const generator = streamChat(messages, 'session-123');
        await generator.next();
      } catch {
        // Expected
      }

      const callArgs = (global.fetch as any).mock.calls[0];
      const body = JSON.parse(callArgs[1].body);

      expect(body.messages).toEqual(messages);
      expect(body.session_id).toBe('session-123');
    });

    it('includes default parameters in request', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Test' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      try {
        const generator = streamChat(messages, 'session-123');
        await generator.next();
      } catch {
        // Expected
      }

      const callArgs = (global.fetch as any).mock.calls[0];
      const body = JSON.parse(callArgs[1].body);

      expect(body.max_tokens).toBe(2000);
      expect(body.temperature).toBe(0.7);
      expect(body.stream).toBe(true);
    });

    it('handles empty messages array', async () => {
      const messages: Message[] = [];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      expect(async () => {
        const generator = streamChat(messages, 'session-123');
        await generator.next();
      }).not.toThrow();
    });

    it('handles multiple messages', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'Message 1' },
        { role: 'assistant', content: 'Response 1' },
        { role: 'user', content: 'Message 2' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      try {
        const generator = streamChat(messages, 'session-123');
        await generator.next();
      } catch {
        // Expected
      }

      const callArgs = (global.fetch as any).mock.calls[0];
      const body = JSON.parse(callArgs[1].body);

      expect(body.messages.length).toBe(3);
    });
  });

  // ===== getConversations Tests =====

  describe('getConversations', () => {
    it('returns array of conversations', async () => {
      const mockConversations: ConversationSummary[] = [
        {
          id: '1',
          title: 'Test Chat',
          created_at: '2024-01-01T00:00:00Z',
          last_accessed: '2024-01-01T00:00:00Z',
          message_count: 5,
          pinned: false,
        },
      ];

      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ conversations: mockConversations }),
      });

      const result = await getConversations();

      expect(result).toEqual(mockConversations);
    });

    it('makes GET request to conversations endpoint', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ conversations: [] }),
      });

      await getConversations();

      expect(global.fetch).toHaveBeenCalledWith(
        expect.stringContaining('/conversations')
      );
    });

    it('handles empty conversations list', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ conversations: [] }),
      });

      const result = await getConversations();

      expect(result).toEqual([]);
    });

    it('throws error on network failure', async () => {
      (global.fetch as any).mockRejectedValue(new Error('Network error'));

      await expect(getConversations()).rejects.toThrow();
    });

    it('throws error on non-200 response', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
      });

      await expect(getConversations()).rejects.toThrow();
    });
  });

  // ===== createNewConversation Tests =====

  describe('createNewConversation', () => {
    it('creates and returns new conversation', async () => {
      const mockConversation = {
        id: 'new-123',
        title: 'New Conversation',
      };

      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => mockConversation,
      });

      const result = await createNewConversation();

      expect(result.id).toBe('new-123');
      expect(result.title).toBe('New Conversation');
    });

    it('makes POST request to create endpoint', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'new', title: 'New' }),
      });

      await createNewConversation();

      expect(global.fetch).toHaveBeenCalledWith(
        expect.stringContaining('/conversations'),
        expect.objectContaining({
          method: 'POST',
        })
      );
    });

    it('handles creation error', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 400,
      });

      await expect(createNewConversation()).rejects.toThrow();
    });
  });

  // ===== deleteConversation Tests =====

  describe('deleteConversation', () => {
    it('deletes conversation successfully', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        status: 204,
      });

      await expect(deleteConversation('chat-123')).resolves.not.toThrow();
    });

    it('makes DELETE request with correct ID', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        status: 204,
      });

      await deleteConversation('chat-123');

      expect(global.fetch).toHaveBeenCalledWith(
        expect.stringContaining('/conversations/chat-123'),
        expect.objectContaining({
          method: 'DELETE',
        })
      );
    });

    it('throws error when deletion fails', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 404,
      });

      await expect(deleteConversation('nonexistent')).rejects.toThrow();
    });

    it('handles network errors', async () => {
      (global.fetch as any).mockRejectedValue(new Error('Network timeout'));

      await expect(deleteConversation('chat-123')).rejects.toThrow();
    });
  });

  // ===== updateConversationTitle Tests =====

  describe('updateConversationTitle', () => {
    it('updates conversation title successfully', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: 'Updated Title' }),
      });

      const result = await updateConversationTitle('chat-123', 'Updated Title');

      expect(result.title).toBe('Updated Title');
    });

    it('makes PUT request with new title', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: 'New Title' }),
      });

      await updateConversationTitle('chat-123', 'New Title');

      const callArgs = (global.fetch as any).mock.calls[0];
      expect(callArgs[0]).toContain('chat-123');
      expect(callArgs[1].method).toBe('PUT');
    });

    it('sends title in request body', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: 'New Title' }),
      });

      await updateConversationTitle('chat-123', 'New Title');

      const callArgs = (global.fetch as any).mock.calls[0];
      const body = JSON.parse(callArgs[1].body);

      expect(body.title).toBe('New Title');
    });

    it('handles empty title', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: '' }),
      });

      const result = await updateConversationTitle('chat-123', '');

      expect(result.title).toBe('');
    });

    it('handles very long titles', async () => {
      const longTitle = 'A'.repeat(1000);

      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: longTitle }),
      });

      const result = await updateConversationTitle('chat-123', longTitle);

      expect(result.title).toBe(longTitle);
    });

    it('handles special characters in title', async () => {
      const specialTitle = '<script>alert("xss")</script>';

      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: 'chat-123', title: specialTitle }),
      });

      const result = await updateConversationTitle('chat-123', specialTitle);

      expect(result.title).toBe(specialTitle);
    });

    it('throws error on failure', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 404,
      });

      await expect(updateConversationTitle('nonexistent', 'Title')).rejects.toThrow();
    });
  });

  // ===== Error Handling Tests =====

  describe('Error Handling', () => {
    it('handles timeout errors', async () => {
      (global.fetch as any).mockRejectedValue(new Error('Request timeout'));

      await expect(getConversations()).rejects.toThrow();
    });

    it('handles malformed JSON responses', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => {
          throw new Error('Invalid JSON');
        },
      });

      await expect(getConversations()).rejects.toThrow();
    });

    it('handles 500 server errors', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
      });

      await expect(getConversations()).rejects.toThrow();
    });

    it('handles 401 unauthorized', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: false,
        status: 401,
        statusText: 'Unauthorized',
      });

      await expect(getConversations()).rejects.toThrow();
    });
  });

  // ===== Request Format Tests =====

  describe('Request Formatting', () => {
    it('formats conversation title updates correctly', async () => {
      (global.fetch as any).mockResolvedValue({
        ok: true,
        json: async () => ({ id: '1', title: 'Updated' }),
      });

      await updateConversationTitle('1', 'Updated');

      const body = JSON.parse((global.fetch as any).mock.calls[0][1].body);
      expect(body).toHaveProperty('title');
      expect(typeof body.title).toBe('string');
    });

    it('formats chat stream request with all parameters', async () => {
      const messages: Message[] = [
        { role: 'user', content: 'test' },
      ];

      const mockResponse = new ReadableStream({
        start(controller) {
          controller.close();
        },
      });

      (global.fetch as any).mockResolvedValue({
        ok: true,
        body: mockResponse,
      });

      try {
        const generator = streamChat(messages, 'session');
        await generator.next();
      } catch {
        // Expected
      }

      const body = JSON.parse((global.fetch as any).mock.calls[0][1].body);
      expect(body).toHaveProperty('model');
      expect(body).toHaveProperty('messages');
      expect(body).toHaveProperty('session_id');
      expect(body).toHaveProperty('max_tokens');
      expect(body).toHaveProperty('temperature');
      expect(body).toHaveProperty('stream');
    });
  });
});
