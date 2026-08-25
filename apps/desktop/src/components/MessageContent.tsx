// MessageContent: Rich markdown renderer for assistant messages
// Renders code blocks with syntax highlighting, tables, lists, and inline formatting

import React, { useState, useCallback } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Copy, Check } from 'lucide-react';

interface MessageContentProps {
  content: string;
  role: 'user' | 'assistant' | 'system';
}

// Helper to get file icon based on extension
const getFileIcon = (name: string): string => {
  const ext = name.split('.').pop()?.toLowerCase() || '';
  if (['pdf'].includes(ext)) return '📄';
  if (['doc', 'docx', 'rtf', 'odt', 'txt'].includes(ext)) return '📝';
  if (['xls', 'xlsx', 'csv', 'ods'].includes(ext)) return '📊';
  if (['ppt', 'pptx', 'odp'].includes(ext)) return '📽️';
  if (['py'].includes(ext)) return '🐍';
  if (['js', 'ts', 'jsx', 'tsx'].includes(ext)) return '⚡';
  if (['rs'].includes(ext)) return '🦀';
  if (['go'].includes(ext)) return '🔷';
  if (['html', 'css', 'scss'].includes(ext)) return '🌐';
  if (['json', 'xml', 'yaml', 'yml'].includes(ext)) return '📋';
  if (['md'].includes(ext)) return '📑';
  if (['sh', 'bat', 'ps1'].includes(ext)) return '⚙️';
  return '📎';
};

// Parse the trailing [Attached files: …] marker some older persisted
// user messages carry (the frontend no longer appends it — the backend
// injects a proper document header — but chats saved before that change
// still contain it, so this strip keeps them displaying cleanly).
const parseAttachedFiles = (content: string): { text: string; files: string[] } => {
  let text = content;
  let files: string[] = [];

  const attachedFilesPattern = /\n?\[Attached files: ([^\]]+)\]$/;
  const match = text.match(attachedFilesPattern);

  if (match) {
    files = match[1].split(',').map(f => f.trim());
    text = text.replace(attachedFilesPattern, '').trim();
  }

  return { text: text.trim(), files };
};

// Attachment chips component
const AttachmentChips: React.FC<{ files: string[] }> = ({ files }) => {
  if (files.length === 0) return null;
  
  return (
    <div className="message-attachments">
      {files.map((file, idx) => (
        <span key={idx} className="message-attachment-chip">
          <span className="message-attachment-icon">{getFileIcon(file)}</span>
          <span className="message-attachment-name">{file}</span>
        </span>
      ))}
    </div>
  );
};

const CodeBlock: React.FC<{
  language: string;
  children: string;
}> = ({ language, children }) => {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(children).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [children]);

  return (
    <div className="code-block-wrapper">
      <div className="code-block-header">
        <span className="code-block-language">{language || 'code'}</span>
        <button className="code-copy-button" onClick={handleCopy} title="Copy code">
          {copied ? <Check size={14} /> : <Copy size={14} />}
          <span>{copied ? 'Copied!' : 'Copy'}</span>
        </button>
      </div>
      <pre className="code-block-pre">
        <code className={`code-block-code language-${language}`}>
          {children}
        </code>
      </pre>
    </div>
  );
};

const MessageContent: React.FC<MessageContentProps> = ({ content, role }) => {
  // User messages: render as plain text only (attachments not shown - used for context only)
  if (role === 'user') {
    // Strip the trailing "[Attached files: …]" marker from any message
    // that still carries it (older persisted chats — see parseAttachedFiles).
    const displayText = content.replace(/\n?\[Attached files: [^\]]+\]$/, '').trim();
    return (
      <div>
        <p className="message-text">{displayText}</p>
      </div>
    );
  }

  // Assistant messages: render as rich markdown
  return (
    <div className="markdown-content">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          // Code blocks with syntax highlighting and copy button
          code({ className, children, ...props }) {
            const match = /language-(\w+)/.exec(className || '');
            const isInline = !match && !String(children).includes('\n');

            if (isInline) {
              return (
                <code className="inline-code" {...props}>
                  {children}
                </code>
              );
            }

            return (
              <CodeBlock language={match ? match[1] : ''}>
                {String(children).replace(/\n$/, '')}
              </CodeBlock>
            );
          },

          // Styled pre to avoid double wrapping
          pre({ children }) {
            return <>{children}</>;
          },

          // Tables
          table({ children }) {
            return (
              <div className="table-wrapper">
                <table className="markdown-table">{children}</table>
              </div>
            );
          },

          // Links open in new tab
          a({ href, children }) {
            return (
              <a href={href} target="_blank" rel="noopener noreferrer" className="markdown-link">
                {children}
              </a>
            );
          },

          // Blockquotes
          blockquote({ children }) {
            return <blockquote className="markdown-blockquote">{children}</blockquote>;
          },

          // Headings
          h1({ children }) { return <h1 className="markdown-h1">{children}</h1>; },
          h2({ children }) { return <h2 className="markdown-h2">{children}</h2>; },
          h3({ children }) { return <h3 className="markdown-h3">{children}</h3>; },

          // Lists
          ul({ children }) { return <ul className="markdown-ul">{children}</ul>; },
          ol({ children }) { return <ol className="markdown-ol">{children}</ol>; },
          li({ children }) { return <li className="markdown-li">{children}</li>; },

          // Paragraphs
          p({ children }) { return <p className="markdown-p">{children}</p>; },

          // Horizontal rule
          hr() { return <hr className="markdown-hr" />; },
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
};

export default MessageContent;
