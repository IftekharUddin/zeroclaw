import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import {
  ChevronLeft,
  ChevronRight,
  Plus,
  Search,
  Settings,
} from 'lucide-react';
import {
  listRuns,
  listSops,
  type SopRunSummary,
  type SopSummary,
} from '@/lib/sops';
import { useWorkspaceSettings } from '@/components/WorkspaceSettings';
import {
  useWorkspaceVisible,
  VisibleWorkspace,
} from '@/components/layout/WorkspaceOutlet';
import { SopEditor } from '@/pages/Sops';
import { t } from '@/lib/i18n';

export default function SopWorkspace() {
  const { name } = useParams();
  const { pathname: currentPath } = useLocation();
  const visible = useWorkspaceVisible();
  const pathRef = useRef(currentPath);
  if (visible) pathRef.current = currentPath;
  const pathname = pathRef.current;
  const navigate = useNavigate();
  const [sops, setSops] = useState<SopSummary[]>([]);
  const [runs, setRuns] = useState<SopRunSummary[]>([]);
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [runError, setRunError] = useState('');
  const [loaded, setLoaded] = useState(false);
  const [revision, setRevision] = useState(0);
  const editors = useRef<Record<string, ReactNode>>({});
  const lastSelection = useRef<string | null | undefined>(undefined);
  const carousel = useRef<HTMLDivElement>(null);
  const settings = useWorkspaceSettings();
  if (pathname === '/sops/new') lastSelection.current = null;
  else if (name) lastSelection.current = name;
  const selected =
    lastSelection.current !== undefined ? lastSelection.current : sops[0]?.name;
  const selectedKey = selected ?? '@new';
  if (selected !== undefined)
    editors.current[selectedKey] = (
      <SopEditor
        editing={selected}
        onSaved={(savedName) => {
          if (selected === null) delete editors.current['@new'];
          setRevision((value) => value + 1);
          void navigate(`/sops/${encodeURIComponent(savedName)}`, {
            replace: true,
          });
        }}
      />
    );
  useEffect(() => {
    let cancelled = false;
    void listSops()
      .then((items) => {
        if (!cancelled) {
          setSops(items);
          setError('');
        }
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoaded(true);
      });
    return () => {
      cancelled = true;
    };
  }, [revision]);
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const refresh = async () => {
      try {
        const items = await listRuns();
        if (!cancelled) {
          setRuns(items);
          setRunError('');
        }
      } catch (e) {
        if (!cancelled) setRunError(e instanceof Error ? e.message : String(e));
      }
      if (!cancelled) timer = setTimeout(() => void refresh(), 5000);
    };
    void refresh();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);
  const filtered = sops.filter((sop) =>
    `${sop.name} ${sop.description}`
      .toLowerCase()
      .includes(query.toLowerCase()),
  );
  const attention = runs
    .filter((run) => run.active || run.status === 'failed')
    .sort((a, b) => b.started_at.localeCompare(a.started_at));
  return (
    <div className="flex min-h-full flex-col gap-4 px-4 pb-5 pt-2 sm:px-6">
      <div className="flex items-center gap-2">
        <label className="flex min-w-0 max-w-sm flex-1 items-center gap-2 rounded-lg bg-pc-elevated px-3 py-2">
          <Search className="h-4 w-4 text-pc-text-muted" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-label={t('workspace.find_sop')}
            placeholder={t('workspace.find_sop')}
            className="min-w-0 w-full bg-transparent text-sm outline-none"
          />
        </label>
        <button
          type="button"
          onClick={() => void navigate('/sops/new')}
          className="ml-auto flex items-center gap-2 rounded-lg px-3 py-2 text-sm hover:bg-pc-elevated disabled:opacity-40"
        >
          <Plus className="h-4 w-4" />
          {t('sops.new')}
        </button>
        <button
          type="button"
          onClick={() => settings('/config/sop')}
          aria-label={t('workspace.settings')}
          className="rounded-lg p-2 text-pc-text-muted hover:bg-pc-elevated"
        >
          <Settings className="h-4 w-4" />
        </button>
      </div>
      {error && (
        <p role="alert" className="text-sm text-status-error">
          {error}
        </p>
      )}
      <div className="flex items-center gap-1">
        <button
          type="button"
          aria-label={t('workspace.previous_sop')}
          className="shrink-0 rounded p-1 text-pc-text-muted hover:bg-pc-elevated"
          onClick={() =>
            carousel.current?.scrollBy({ left: -320, behavior: 'smooth' })
          }
        >
          <ChevronLeft className="h-4 w-4" />
        </button>
        <div
          ref={carousel}
          role="region"
          aria-roledescription={t('workspace.carousel')}
          aria-label={t('sops.title')}
          className="flex min-w-0 flex-1 snap-x gap-3 overflow-x-auto pb-2"
        >
          {filtered.map((sop) => (
            <button
              key={sop.name}
              type="button"
              aria-pressed={selected === sop.name}
              onClick={() =>
                void navigate(`/sops/${encodeURIComponent(sop.name)}`)
              }
              className={`w-56 shrink-0 snap-start rounded-xl border px-4 py-3 text-left disabled:opacity-40 ${selected === sop.name ? 'border-pc-accent/50 bg-pc-elevated' : 'border-pc-border hover:bg-pc-elevated/50'}`}
            >
              <span className="block truncate text-sm font-medium">
                {sop.name}
              </span>
              <span className="mt-1 block truncate text-xs text-pc-text-muted">
                {sop.description || t('workspace.sop_definition')}
              </span>
            </button>
          ))}
          {loaded && !filtered.length && (
            <p className="p-3 text-sm text-pc-text-muted">
              {t(sops.length ? 'nav.cmdk.empty' : 'sops.empty')}
            </p>
          )}
        </div>
        <button
          type="button"
          aria-label={t('workspace.next_sop')}
          className="shrink-0 rounded p-1 text-pc-text-muted hover:bg-pc-elevated"
          onClick={() =>
            carousel.current?.scrollBy({ left: 320, behavior: 'smooth' })
          }
        >
          <ChevronRight className="h-4 w-4" />
        </button>
      </div>
      {(attention.length > 0 || runError) && (
        <section
          aria-label={t('workspace.run_activity')}
          className="flex flex-wrap items-center gap-2 text-xs"
        >
          <span className="text-pc-text-muted">
            {t('workspace.run_activity')}
          </span>
          {attention.slice(0, 4).map((run) => (
            <Link
              key={run.run_id}
              to={`/runs/${encodeURIComponent(run.sop_name)}/${encodeURIComponent(run.run_id)}`}
              className={`rounded-full border px-3 py-1.5 ${run.status === 'failed' ? 'border-status-error/30 text-status-error' : 'border-pc-accent/30 text-pc-accent'}`}
            >
              {run.sop_name} · {t(`sops.run_status.${run.status}`)}
            </Link>
          ))}
          <Link to="/runs" className="ml-auto text-pc-text-muted underline">
            {t('nav.runs')}
            {attention.length > 4 ? ` (${attention.length})` : ''}
          </Link>
          {runError && (
            <details className="w-full text-pc-text-muted">
              <summary className="cursor-pointer">
                {t('workspace.runs_unavailable')}
              </summary>
              <p role="alert" className="mt-2 text-xs">
                {runError}
              </p>
              <button
                type="button"
                onClick={() => settings('/config/sop')}
                className="mt-2 underline"
              >
                {t('workspace.settings')}
              </button>
            </details>
          )}
        </section>
      )}
      {Object.entries(editors.current).map(([key, editor]) => (
        <div key={key} hidden={key !== selectedKey}>
          <VisibleWorkspace.Provider value={visible && key === selectedKey}>
            {editor}
          </VisibleWorkspace.Provider>
        </div>
      ))}
      {!loaded && (
        <p role="status" className="text-sm text-pc-text-muted">
          {t('common.loading')}
        </p>
      )}
    </div>
  );
}
