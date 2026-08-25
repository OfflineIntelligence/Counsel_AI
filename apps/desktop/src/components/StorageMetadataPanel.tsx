import React, { useState, useEffect, useCallback } from 'react';
import { getApiBaseSync } from '../api/backendUrl';
import { ArrowLeft, Database, HardDrive, Folder, FileBox, Server, Clock, Calendar, Settings } from 'lucide-react';
import { fetchStorageSettings, type StorageSettings } from '../api/storageSettings';

interface StoragePaths {
  app_data_dir: string;
  models_dir: string;
  registry_dir: string;
  database_path: string;
}

interface DownloadedModelInfo {
  id: string;
  name: string;
  format: string;
  size_bytes: number;
  size_human: string;
  download_date: string;
  download_source: string;
  file_path: string;
  metadata_path: string | null;
}

interface StorageStats {
  models_total_bytes: number;
  models_total_human: string;
  available_space_bytes: number;
  available_space_human: string;
  model_count: number;
}

interface DatabaseInfo {
  path: string;
  size_bytes: number;
  size_human: string;
}

interface StorageMetadata {
  paths: StoragePaths;
  models: DownloadedModelInfo[];
  storage_stats: StorageStats;
  database_info: DatabaseInfo;
}

const StorageMetadataPanel: React.FC<{
  isOpen: boolean;
  onClose: () => void;
}> = ({ isOpen, onClose }) => {
  const [metadata, setMetadata] = useState<StorageMetadata | null>(null);
  // Total app disk budget, so this page reports the same ceiling the
  // backend actually enforces rather than models-only figures.
  const [budget, setBudget] = useState<StorageSettings | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<'overview' | 'models' | 'paths'>('overview');

  const fetchMetadata = useCallback(async () => {
    try {
      setLoading(true);
      setError(null);
      const response = await fetch(`${getApiBaseSync()}/storage/metadata`);
      if (!response.ok) {
        throw new Error(`Failed to fetch storage metadata: ${response.status}`);
      }
      const data = await response.json();
      setMetadata(data);

      // Budget is a separate endpoint and a separate concern: failing to read
      // it must not blank out the whole storage page, so it is caught here and
      // simply leaves the total-usage card hidden.
      try {
        setBudget(await fetchStorageSettings());
      } catch (budgetErr) {
        console.warn('Could not load the storage budget:', budgetErr);
        setBudget(null);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load storage metadata');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (isOpen) {
      fetchMetadata();
    }
  }, [isOpen, fetchMetadata]);

  if (!isOpen) return null;

  const formatPath = (path: string) => {
    // Format path for display - show shortened version if too long
    if (path.length > 60) {
      return '...' + path.slice(-57);
    }
    return path;
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', width: '100%', backgroundColor: 'var(--bg-canvas)' }}>
      {/* Header */}
      <div className="chat-header">
        <div className="chat-header-bar centered">
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
            <button className="header-icon-button" onClick={onClose} title="Back">
              <ArrowLeft size={18} />
            </button>
            <h1 className="chat-title" style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
              <HardDrive size={20} />
              Local Storage
            </h1>
          </div>
          <div className="header-actions">
            <button className="header-button" onClick={fetchMetadata} title="Refresh">
              <Settings size={16} />
            </button>
          </div>
        </div>
      </div>

      {/* Tab Navigation */}
      <div style={{ 
        display: 'flex', 
        borderBottom: '1px solid var(--border-primary)',
        backgroundColor: 'var(--bg-secondary)'
      }}>
        {(['overview', 'models', 'paths'] as const).map((tab) => (
          <button
            key={tab}
            onClick={() => setActiveTab(tab)}
            style={{
              flex: 1,
              padding: '12px 16px',
              background: 'none',
              border: 'none',
              borderBottom: `2px solid ${activeTab === tab ? 'var(--accent)' : 'transparent'}`,
              color: activeTab === tab ? 'var(--text-primary)' : 'var(--text-secondary)',
              fontWeight: activeTab === tab ? 600 : 400,
              cursor: 'pointer',
              textTransform: 'capitalize',
              transition: 'all 0.2s'
            }}
          >
            {tab}
          </button>
        ))}
      </div>

      {/* Content */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '16px' }}>
        <div style={{ maxWidth: '900px', margin: '0 auto' }}>
          {loading ? (
            <div style={{ textAlign: 'center', padding: '60px 0', color: 'var(--text-muted)' }}>
              <HardDrive size={40} style={{ margin: '0 auto 12px', opacity: 0.5 }} />
              <p>Loading storage metadata...</p>
            </div>
          ) : error ? (
            <div style={{ textAlign: 'center', padding: '60px 0', color: 'var(--danger)' }}>
              <p>{error}</p>
              <button 
                onClick={fetchMetadata}
                style={{
                  marginTop: '12px',
                  padding: '8px 16px',
                  background: 'var(--accent)',
                  color: 'var(--text-on-accent)',
                  border: 'none',
                  borderRadius: '6px',
                  cursor: 'pointer'
                }}
              >
                Retry
              </button>
            </div>
          ) : metadata ? (
            <>
              {activeTab === 'overview' && (
                <>
                  {/* Storage Stats Cards */}
                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))', gap: '16px', marginBottom: '24px' }}>
                    <StatCard 
                      icon={<FileBox size={24} style={{ color: 'var(--text-secondary)' }} />}
                      title="Downloaded Models"
                      value={metadata.storage_stats.model_count.toString()}
                      subtitle={`${metadata.storage_stats.models_total_human} total`}
                    />
                    <StatCard 
                      icon={<HardDrive size={24} style={{ color: 'var(--text-secondary)' }} />}
                      title="Storage Used"
                      value={metadata.storage_stats.models_total_human}
                      subtitle="Models only"
                    />
                    <StatCard 
                      icon={<Database size={24} style={{ color: 'var(--text-secondary)' }} />}
                      title="Available Space"
                      value={metadata.storage_stats.available_space_human}
                      subtitle="Free for downloads"
                    />
                    <StatCard 
                      icon={<Server size={24} style={{ color: 'var(--text-secondary)' }} />}
                      title="Database Size"
                      value={metadata.database_info.size_human}
                      subtitle="Conversations & data"
                    />
                    {budget && (
                      <StatCard
                        icon={<HardDrive size={24} style={{ color: budget.over_budget ? 'var(--danger)' : 'var(--text-secondary)' }} />}
                        title="Total App Usage"
                        value={budget.usage_human}
                        subtitle={
                          budget.limit_mb === 0
                            ? 'No limit set'
                            : `of ${budget.limit_human}${budget.over_budget ? ' — over limit' : ''}`
                        }
                      />
                    )}
                  </div>

                  {/* Quick Info */}
                  <div style={{ 
                    backgroundColor: 'var(--bg-secondary)', 
                    border: '1px solid var(--border-primary)',
                    borderRadius: '12px', 
                    padding: '16px',
                    marginBottom: '16px'
                  }}>
                    <h3 style={{ fontSize: '14px', fontWeight: 600, marginBottom: '12px', display: 'flex', alignItems: 'center', gap: '8px' }}>
                      <Folder size={16} />
                      Storage Locations
                    </h3>
                    <PathRow label="Application Data" path={formatPath(metadata.paths.app_data_dir)} />
                    <PathRow label="Models Directory" path={formatPath(metadata.paths.models_dir)} />
                    <PathRow label="Database File" path={formatPath(metadata.paths.database_path)} />
                  </div>
                </>
              )}

              {activeTab === 'models' && (
                <>
                  <h3 style={{ fontSize: '16px', fontWeight: 600, marginBottom: '16px' }}>
                    Downloaded Models ({metadata.models.length})
                  </h3>
                  {metadata.models.length === 0 ? (
                    <div style={{ 
                      textAlign: 'center', 
                      padding: '40px', 
                      color: 'var(--text-muted)',
                      backgroundColor: 'var(--bg-secondary)',
                      borderRadius: '12px'
                    }}>
                      <FileBox size={40} style={{ margin: '0 auto 12px', opacity: 0.5 }} />
                      <p>No models downloaded yet</p>
                      <p style={{ fontSize: '13px', marginTop: '8px' }}>
                        Go to the Models page to download your first model
                      </p>
                    </div>
                  ) : (
                    metadata.models.map((model) => (
                      <div 
                        key={model.id}
                        style={{ 
                          backgroundColor: 'var(--bg-secondary)', 
                          border: '1px solid var(--border-primary)',
                          borderRadius: '12px', 
                          padding: '16px',
                          marginBottom: '12px'
                        }}
                      >
                        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', marginBottom: '8px' }}>
                          <div>
                            <h4 style={{ fontSize: '15px', fontWeight: 600, marginBottom: '4px' }}>{model.name}</h4>
                            <p style={{ fontSize: '12px', color: 'var(--text-muted)' }}>ID: {model.id}</p>
                          </div>
                          <span style={{ 
                            fontSize: '12px', 
                            padding: '4px 8px', 
                            backgroundColor: 'var(--bg-tertiary)',
                            borderRadius: '4px',
                            color: 'var(--text-secondary)'
                          }}>
                            {model.format.toUpperCase()}
                          </span>
                        </div>
                        
                        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: '12px', marginTop: '12px', fontSize: '13px' }}>
                          <div>
                            <span style={{ color: 'var(--text-muted)' }}>Size: </span>
                            <span style={{ fontWeight: 500 }}>{model.size_human}</span>
                          </div>
                          <div>
                            <span style={{ color: 'var(--text-muted)' }}>Source: </span>
                            <span style={{ textTransform: 'capitalize' }}>{model.download_source}</span>
                          </div>
                          <div>
                            <span style={{ color: 'var(--text-muted)' }}>Downloaded: </span>
                            <span>{model.download_date}</span>
                          </div>
                        </div>

                        <div style={{ marginTop: '12px', paddingTop: '12px', borderTop: '1px solid var(--border-primary)' }}>
                          <p style={{ fontSize: '11px', color: 'var(--text-muted)', wordBreak: 'break-all' }}>
                            <span style={{ color: 'var(--text-secondary)' }}>Location: </span>
                            {model.file_path}
                          </p>
                        </div>
                      </div>
                    ))
                  )}
                </>
              )}

              {activeTab === 'paths' && (
                <>
                  <h3 style={{ fontSize: '16px', fontWeight: 600, marginBottom: '16px' }}>
                    System Storage Paths
                  </h3>
                  
                  <PathDetailCard
                    title="Application Data Directory"
                    icon={<Folder size={20} style={{ color: 'var(--text-secondary)' }} />}
                    path={metadata.paths.app_data_dir}
                    description="Main directory for all Offline_Intelligence application data including models, registry, and database"
                  />
                  
                  <PathDetailCard 
                    title="Models Directory"
                    icon={<FileBox size={20} style={{ color: 'var(--text-secondary)' }} />}
                    path={metadata.paths.models_dir}
                    description="Contains all downloaded model files organized by model ID"
                  />
                  
                  <PathDetailCard 
                    title="Registry Directory"
                    icon={<Settings size={20} style={{ color: 'var(--text-secondary)' }} />}
                    path={metadata.paths.registry_dir}
                    description="Contains model metadata JSON files and registry information"
                  />
                  
                  <PathDetailCard 
                    title="Database File"
                    icon={<Database size={20} style={{ color: 'var(--text-secondary)' }} />}
                    path={metadata.paths.database_path}
                    description="SQLite database containing conversations, sessions, documents, and file metadata"
                  />

                  <div style={{ 
                    marginTop: '24px',
                    padding: '16px',
                    backgroundColor: 'rgba(59, 130, 246, 0.1)',
                    border: '1px solid rgba(59, 130, 246, 0.3)',
                    borderRadius: '8px'
                  }}>
                    <h4 style={{ fontSize: '14px', fontWeight: 600, marginBottom: '8px', color: 'var(--text-secondary)' }}>
                      Platform Information
                    </h4>
                    <p style={{ fontSize: '13px', color: 'var(--text-secondary)', lineHeight: '1.6' }}>
                      Storage locations are platform-specific:
                    </p>
                    <ul style={{ fontSize: '13px', color: 'var(--text-secondary)', marginTop: '8px', paddingLeft: '20px', lineHeight: '1.8' }}>
                      <li><strong>Windows:</strong> %APPDATA%\Offline_Intelligence\</li>
                      <li><strong>macOS:</strong> ~/Library/Application Support/Offline_Intelligence/</li>
                      <li><strong>Linux:</strong> ~/.local/share/offline_intelligence/</li>
                    </ul>
                  </div>
                </>
              )}
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
};

// Helper Components
const StatCard: React.FC<{
  icon: React.ReactNode;
  title: string;
  value: string;
  subtitle: string;
}> = ({ icon, title, value, subtitle }) => (
  <div style={{ 
    backgroundColor: 'var(--bg-secondary)', 
    border: '1px solid var(--border-primary)',
    borderRadius: '12px', 
    padding: '16px',
  }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: '12px', marginBottom: '8px' }}>
      {icon}
      <span style={{ fontSize: '12px', color: 'var(--text-muted)' }}>{title}</span>
    </div>
    <div style={{ fontSize: '24px', fontWeight: 700, color: 'var(--text-primary)', marginBottom: '4px' }}>
      {value}
    </div>
    <div style={{ fontSize: '12px', color: 'var(--text-secondary)' }}>
      {subtitle}
    </div>
  </div>
);

const PathRow: React.FC<{
  label: string;
  path: string;
}> = ({ label, path }) => (
  <div style={{ 
    display: 'flex', 
    justifyContent: 'space-between', 
    alignItems: 'center',
    padding: '8px 0',
    borderBottom: '1px solid var(--border-primary)'
  }}>
    <span style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>{label}</span>
    <span style={{ fontSize: '12px', color: 'var(--text-muted)', fontFamily: 'monospace' }} title={path}>
      {path}
    </span>
  </div>
);

const PathDetailCard: React.FC<{
  title: string;
  icon: React.ReactNode;
  path: string;
  description: string;
}> = ({ title, icon, path, description }) => (
  <div style={{ 
    backgroundColor: 'var(--bg-secondary)', 
    border: '1px solid var(--border-primary)',
    borderRadius: '12px', 
    padding: '16px',
    marginBottom: '12px'
  }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: '12px', marginBottom: '12px' }}>
      {icon}
      <h4 style={{ fontSize: '15px', fontWeight: 600 }}>{title}</h4>
    </div>
    <p style={{ fontSize: '13px', color: 'var(--text-secondary)', marginBottom: '8px', lineHeight: '1.5' }}>
      {description}
    </p>
    <div style={{ 
      padding: '10px 12px',
      backgroundColor: 'var(--bg-tertiary)',
      borderRadius: '6px',
      fontFamily: 'monospace',
      fontSize: '12px',
      color: 'var(--text-muted)',
      wordBreak: 'break-all'
    }}>
      {path}
    </div>
  </div>
);

export default StorageMetadataPanel;
