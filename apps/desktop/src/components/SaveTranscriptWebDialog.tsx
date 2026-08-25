// Web-based save dialog for dev environments (Tauri dev popup, browser dev)
// Uses browser download API with filename editing - shows modal before auto-download
import { useState, useEffect } from 'react';

interface Props {
  open: boolean;
  defaultFileName: string;
  content: string;
  onClose: () => void;
  onSaved?: (info: string) => void;
}

const ensureExtension = (name: string, extension: string) => {
  if (name.endsWith(extension)) {
    return name;
  }
  // Remove any existing extensions and add the new one
  const baseName = name.replace(/\.\w+$/, '');
  return `${baseName}${extension}`;
};

export function SaveTranscriptWebDialog({ open, defaultFileName, content, onClose, onSaved }: Props) {
  const [fileName, setFileName] = useState<string>(ensureExtension(defaultFileName, '.txt'));
  const [fileFormat, setFileFormat] = useState<'txt' | 'docx' | 'pdf'>('txt');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Update fileName when defaultFileName prop changes
  useEffect(() => {
    if (open) {
      setFileName(ensureExtension(defaultFileName, '.txt'));
      setFileFormat('txt');
      setError(null);
    }
  }, [open, defaultFileName]);

  if (!open) return null;

  const browserDownload = async (name: string, format: 'txt' | 'docx' | 'pdf') => {
    let blob;
    
    switch (format) {
      case 'txt':
        blob = new Blob([content], { type: 'text/plain;charset=utf-8' });
        break;
      case 'docx': {
        // Create a proper DOCX using the docx library
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
        blob = new Blob([buffer], { type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document' });
        break;
      }
      case 'pdf': {
        // Create a proper PDF using the jsPDF library
        const { jsPDF } = await import('jspdf');
        const pdf = new jsPDF();
        const textLines = pdf.splitTextToSize(content, 180);
        pdf.text(textLines, 10, 10);
        const pdfBlob = pdf.output('blob');
        blob = pdfBlob;
        break;
      }
      default:
        blob = new Blob([content], { type: 'text/plain;charset=utf-8' });
    }
    
    const link = document.createElement('a');
    link.href = URL.createObjectURL(blob);
    link.download = ensureExtension(name, `.${format}`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    setTimeout(() => URL.revokeObjectURL(link.href), 100);
    onSaved?.(link.download);
  };

  const save = async () => {
    setError(null);
    const name = ensureExtension(fileName.trim() || defaultFileName, `.${fileFormat}`);
    setSaving(true);
    try {
      const fsWindow = window as Window & { showSaveFilePicker?: (options: { suggestedName: string; types: { description: string; accept: Record<string, string[]> }[] }) => Promise<FileSystemFileHandle> };
      if (fsWindow.showSaveFilePicker) {
        let acceptTypes: Record<string, string[]>;
        switch (fileFormat) {
          case 'txt':
            acceptTypes = { 'text/plain': ['.txt'] };
            break;
          case 'docx':
            acceptTypes = { 'application/vnd.openxmlformats-officedocument.wordprocessingml.document': ['.docx'] };
            break;
          case 'pdf':
            acceptTypes = { 'application/pdf': ['.pdf'] };
            break;
          default:
            acceptTypes = { 'text/plain': ['.txt'] };
        }
        
        const handle = await fsWindow.showSaveFilePicker({
          suggestedName: name,
          types: [
            {
              description: fileFormat.toUpperCase(),
              accept: acceptTypes
            }
          ]
        });
        
        // Handle different formats differently
        if (fileFormat === 'txt') {
          const writable = await handle.createWritable();
          await writable.write(content);
          await writable.close();
        } else {
          // For docx and pdf, we need to generate the proper binary content
          let fileData: ArrayBuffer;
          
          if (fileFormat === 'docx') {
            const { Document: DocxDocument, Paragraph: DocxParagraph, Packer } = await import('docx');
            const doc = new DocxDocument({
              sections: [{
                properties: {},
                children: [
                  new DocxParagraph(content)
                ],
              }],
            });
            fileData = await Packer.toBuffer(doc);
          } else { // pdf
            const { jsPDF } = await import('jspdf');
            const pdf = new jsPDF();
            const textLines = pdf.splitTextToSize(content, 180);
            pdf.text(textLines, 10, 10);
            fileData = pdf.output('arraybuffer');
          }
          
          const writable = await handle.createWritable();
          await writable.write(fileData);
          await writable.close();
        }
        
        onSaved?.(name);
      } else {
        // Fallback to standard download
        await browserDownload(name, fileFormat);
      }
      onClose();
    } catch (e: unknown) {
      if (e && typeof e === 'object' && 'name' in e && e.name === 'AbortError') {
        // user cancelled dialog
        onClose();
      } else if (e && typeof e === 'object' && 'message' in e) {
        setError((e as { message?: string }).message ?? 'Failed to save');
      } else {
        setError('Failed to save');
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.4)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000 }}>
      <div style={{ background: 'var(--bg-surface)', width: 520, maxWidth: '90vw', borderRadius: 12, boxShadow: '0 8px 32px rgba(0,0,0,0.2)' }}>
        <div style={{ padding: '16px 20px', borderBottom: '1px solid #eee', fontWeight: 600 }}>Save Transcript</div>
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
                    fontWeight: fileFormat === format ? 600 : 400,
                    cursor: 'pointer',
                  }}
                >
                  {format.toUpperCase()}
                </button>
              ))}
            </div>
          </div>
          <div style={{ color: 'var(--text-secondary)', fontSize: 12, marginTop: 4 }}>
            Your file will save to the default Downloads folder.
          </div>
          {error && <div style={{ color: 'var(--danger)', fontSize: 13, marginTop: 8 }}>{error}</div>}
        </div>
        <div style={{ padding: 16, display: 'flex', justifyContent: 'flex-end', gap: 8, borderTop: '1px solid #eee' }}>
          <button onClick={onClose} disabled={saving} style={{ padding: '10px 14px', borderRadius: 8, border: '1px solid #ddd', background: 'var(--bg-hover)' }}>Cancel</button>
          <button onClick={save} disabled={saving} style={{ padding: '10px 14px', borderRadius: 8, border: '1px solid #0b5', background: 'var(--accent)', color: 'var(--bg-surface)' }}>{saving ? 'Saving…' : 'Save'}</button>
        </div>
      </div>
    </div>
  );
}
