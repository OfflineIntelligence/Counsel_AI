// MessageContent: Rich markdown renderer for assistant messages
// Renders code blocks with syntax highlighting, tables, lists, and inline formatting

import React, { useState, useCallback, useEffect, useRef } from 'react';
import { FileGlyph, formatOf, FILE_ICON } from './FileBrandIcon';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Copy, Check, ChevronRight } from 'lucide-react';
import type { Components } from 'react-markdown';

interface MessageContentProps {
  content: string;
  role: 'user' | 'assistant' | 'system';
  /** The answer is still being written. */
  streaming?: boolean;
}

/** Reasoning models (DeepSeek-R1, QwQ, Qwen3…) write their thinking inside
 *  <think>…</think> before the answer. Split it out so it can be shown as a
 *  collapsible "Thoughts" block; an unclosed tag means it is still thinking. */
export function splitThinking(content: string): { thoughts: string; answer: string; thinking: boolean } {
  const open = content.search(/<think>/i);
  if (open === -1) {
    // Some templates drop the opening tag and only emit the closing one.
    const close = content.search(/<\/think>/i);
    if (close === -1) return { thoughts: '', answer: content, thinking: false };
    return { thoughts: content.slice(0, close).trim(), answer: content.slice(close + 8).trimStart(), thinking: false };
  }
  const after = content.slice(open + 7);
  const close = after.search(/<\/think>/i);
  if (close === -1) return { thoughts: after.trim(), answer: content.slice(0, open).trim(), thinking: true };
  return {
    thoughts: after.slice(0, close).trim(),
    answer: (content.slice(0, open) + after.slice(close + 8)).trim(),
    thinking: false,
  };
}

const Thoughts: React.FC<{ text: string; live: boolean }> = ({ text, live }) => {
  const [open, setOpen] = useState(false);
  // How long it thought: timed from the first render while live to the
  // render where the answer starts. Answers loaded from history show none.
  const startedAt = useRef<number | null>(live ? Date.now() : null);
  const [seconds, setSeconds] = useState<number | null>(null);
  useEffect(() => {
    if (live && startedAt.current === null) startedAt.current = Date.now();
    if (!live && startedAt.current !== null && seconds === null) {
      setSeconds(Math.max(1, Math.round((Date.now() - startedAt.current) / 1000)));
    }
  }, [live, seconds]);

  if (!text && !live) return null;
  // While thinking, the newest line trails under the label, faded, so the
  // reasoning is visibly moving without opening the panel.
  const lastLine = live ? text.split('\n').filter(l => l.trim()).pop() ?? '' : '';
  const label = live ? 'Thinking' : seconds ? `Thought for ${seconds}s` : 'Thoughts';
  return (
    <div className={`thoughts${open ? ' is-open' : ''}${live ? ' is-live' : ''}`}>
      <button type="button" className="thoughts__toggle" onClick={() => setOpen(o => !o)} aria-expanded={open}>
        <span className="thoughts__label">{label}</span>
        <ChevronRight size={14} strokeWidth={1.75} className="thoughts__chevron" />
      </button>
      {!open && lastLine && <div className="thoughts__peek">{lastLine}</div>}
      <div className="thoughts__panel">
        <div className="thoughts__panel-inner">
          <div className="thoughts__body">{text}</div>
        </div>
      </div>
    </div>
  );
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
          <span className="message-attachment-icon"><FileGlyph format={formatOf(file)} size={FILE_ICON.list} /></span>
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

// Defined once, outside the component: ReactMarkdown treats a new function
// as a new component type, so inline renderers would remount the whole
// answer on every streamed token.
const MARKDOWN_COMPONENTS: Components = {
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
};

const REMARK_PLUGINS = [remarkGfm];

const MessageContent = React.memo(function MessageContent({ content, role, streaming = false }: MessageContentProps) {
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

  // Assistant messages: reasoning first (collapsed), then the answer as
  // markdown. Text simply grows in place while streaming: no per-word
  // animation and no caret, so each token costs one cheap re-render.
  const { thoughts, answer, thinking } = splitThinking(content);
  return (
    <div className={`markdown-content${streaming ? ' is-streaming' : ''}`}>
      <Thoughts text={thoughts} live={streaming && thinking} />
      <ReactMarkdown
        remarkPlugins={REMARK_PLUGINS}
        components={MARKDOWN_COMPONENTS}
      >
        {answer}
      </ReactMarkdown>
    </div>
  );
});

export default MessageContent;
