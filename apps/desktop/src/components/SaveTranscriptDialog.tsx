// Custom save dialog component for Tauri environments with directory picker
// Allows users to select a directory and specify filename before saving transcript
import { useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
/* `BaseDirectory` and plugin-dialog's `save` were imported here and never
   called -- this dialog picks its own directory and writes an absolute path.
   The `save` import mattered beyond tidiness: a static import of
   @tauri-apps/plugin-dialog from this module is one of the reasons Rollup
   could not move that plugin out of the entry chunk, because a module that is
   statically imported anywhere is pinned there for everyone. */

// docx and jspdf are loaded ON DEMAND, in the branch that needs them -- see
// the switch in handleSave below.
//
// They used to be imported statically here AND again dynamically a few lines
// into that switch, which is the worst of both: a static import of a module
// pins it into the entry chunk, and the later `await import()` of the same
// module then resolves against that already-bundled copy. So the dynamic
// import looked like a lazy load, read like one in review, and deferred
// nothing. Between them the two libraries are a large part of the main bundle,
// and this dialog is only reached by choosing "Save transcript".
//
// SaveTranscriptWebDialog.tsx already did it this way; the two agree now.

interface SaveTranscriptDialogProps {
  open: boolean;
  defaultFileName: string;
  content: string;
  onClose: () => void;
  onSaved?: (fullPath: string) => void;
}

const ensureExtension = (name: string, extension: string) => {
  if (name.endsWith(extension)) {
    return name;
  }
  // Remove any existing extensions and add the new one
  const baseName = name.replace(/\.\w+$/, '');
  return `${baseName}${extension}`;
};

export function SaveTranscriptDialog({ open: isOpen, defaultFileName, content, onClose, onSaved }: SaveTranscriptDialogProps) {
  const [directory, setDirectory] = useState<string | null>(null);
  const [fileName, setFileName] = useState<string>(ensureExtension(defaultFileName, '.txt'));
  const [fileFormat, setFileFormat] = useState<'txt' | 'docx' | 'pdf'>('txt');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!isOpen) return null;

  const pickDirectory = async () => {
    setError(null);
    try {
      const dir = await open({ directory: true, multiple: false });
      if (typeof dir === 'string') {
        setDirectory(dir);
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : 'Failed to open directory picker');
    }
  };

  const handleSave = async () => {
    setError(null);
    if (!directory) {
      setError('Please choose a folder.');
      return;
    }
    const safeName = ensureExtension(fileName.trim() || defaultFileName, `.${fileFormat}`);
    const fullPath = `${directory.replace(/\/$/, '')}/${safeName}`;
    setSaving(true);
    try {
      switch (fileFormat) {
        case 'txt':
          // For TXT files, save as text
          await writeTextFile(fullPath, content);
          break;
        case 'docx': {
          const { Document: DocxDocument, Paragraph: DocxParagraph, Packer } = await import('docx');
          const doc = new DocxDocument({
            sections: [{
              properties: {},
              children: [
                new DocxParagraph(content)
              ],
            }],
          });
          
          const buffer = await Packer.toBuffer(doc);
          const fileData = new Uint8Array(buffer);
          
          // Write binary data to file
          const fs = await import('@tauri-apps/plugin-fs');
          await fs.writeFile(fullPath, fileData);
          break;
        }
        case 'pdf': {
          const { jsPDF } = await import('jspdf');
          const pdf = new jsPDF();
          const textLines = pdf.splitTextToSize(content, 180);
          pdf.text(textLines, 10, 10);
          const pdfArrayBuffer = pdf.output('arraybuffer');
          const fileData = new Uint8Array(pdfArrayBuffer as ArrayBuffer);
          
          // Write binary data to file
          const fs = await import('@tauri-apps/plugin-fs');
          await fs.writeFile(fullPath, fileData);
          break;
        }
        default:
          await writeTextFile(fullPath, content);
      }
      
      onSaved?.(fullPath);
      onClose();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : 'Failed to save file');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div style={{
      position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.4)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000
    }}>
      <div style={{ background: 'var(--bg-surface)', width: 520, maxWidth: '90vw', borderRadius: 12, boxShadow: '0 8px 32px rgba(0,0,0,0.2)' }}>
        <div style={{ padding: '16px 20px', borderBottom: '1px solid #eee', fontWeight: 'var(--fw-semibold)' }}>Save Transcript</div>
        <div style={{ padding: 20 }}>
          <div style={{ marginBottom: 12 }}>
            <label style={{ display: 'block', fontSize: 12, color: 'var(--text-secondary)', marginBottom: 6 }}>File name</label>
            <input
              value={fileName}
              onChange={(e) => setFileName(e.target.value)}
              placeholder="chat-YYYY-MM-DD.txt"
              style={{ width: '100%', padding: '10px 12px', border: '1px solid #ddd', borderRadius: 8 }}
            />
          </div>
          <div style={{ marginBottom: 12 }}>
            <label style={{ display: 'block', fontSize: 12, color: 'var(--text-secondary)', marginBottom: 6 }}>File format</label>
            <div style={{ display: 'flex', gap: 8 }}>
              {(['txt', 'docx', 'pdf'] as const).map((format) => (
                <button
                  key={format}
                  type="button"
                  onClick={() => setFileFormat(format)}
                  style={{
                    flex: 1,
                    padding: '8px 12px',
                    borderRadius: 6,
                    border: fileFormat === format ? '2px solid #0c6' : '1px solid #ddd',
                    background: fileFormat === format ? 'var(--bg-selected)' : 'var(--bg-surface)',
                    color: fileFormat === format ? 'var(--accent)' : 'var(--text-primary)',
                    fontWeight: fileFormat === format ? 'var(--fw-semibold)' : 400,
                    cursor: 'pointer',
                  }}
                >
                  {format.toUpperCase()}
                </button>
              ))}
            </div>
          </div>
          <div style={{ marginBottom: 12 }}>
            <label style={{ display: 'block', fontSize: 12, color: 'var(--text-secondary)', marginBottom: 6 }}>Folder</label>
            <div style={{ display: 'flex', gap: 8 }}>
              <input
                readOnly
                value={directory ?? ''}
                placeholder="No folder selected"
                style={{ flex: 1, padding: '10px 12px', border: '1px solid #ddd', borderRadius: 8, background: 'var(--bg-inset)' }}
              />
              <button onClick={pickDirectory} style={{ padding: '10px 14px', borderRadius: 8, border: '1px solid #ddd', background: 'var(--bg-hover)' }}>
                Choose…
              </button>
            </div>
          </div>
          {error && (
            <div style={{ color: 'var(--danger)', fontSize: 13, marginTop: 8 }}>{error}</div>
          )}
        </div>
        <div style={{ padding: 16, display: 'flex', justifyContent: 'flex-end', gap: 8, borderTop: '1px solid #eee' }}>
          <button onClick={onClose} disabled={saving} style={{ padding: '10px 14px', borderRadius: 8, border: '1px solid #ddd', background: 'var(--bg-hover)' }}>Cancel</button>
          <button onClick={handleSave} disabled={saving} style={{ padding: '10px 14px', borderRadius: 8, border: '1px solid #0b5', background: 'var(--accent)', color: 'var(--bg-surface)' }}>{saving ? 'Saving…' : 'Save'}</button>
        </div>
      </div>
    </div>
  );
}
