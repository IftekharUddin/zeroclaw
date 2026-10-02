import { useState } from 'react';
import { Link, useLocation } from 'react-router-dom';
import { MoreHorizontal, Search } from 'lucide-react';
import { t, SUPPORTED_LOCALES } from '@/lib/i18n';
import { useLocaleContext } from '@/App';
import { useAuth } from '@/hooks/useAuth';
import { SettingsModal } from '@/components/SettingsModal';
import { useWorkspaceSettings } from '@/components/WorkspaceSettings';

export default function Header({
  onOpenPalette,
}: {
  onOpenPalette: () => void;
}) {
  const { pathname } = useLocation();
  const { logout } = useAuth();
  const { locale, setAppLocale } = useLocaleContext();
  const openSettings = useWorkspaceSettings();
  const [appearance, setAppearance] = useState(false);
  const [menu, setMenu] = useState(false);
  const home = pathname === '/';
  return (
    <>
      <header className="relative z-40 flex h-14 shrink-0 items-center justify-between gap-3 px-4 sm:px-6">
        <div className="flex min-w-0 items-center gap-5">
          <Link
            to="/"
            aria-label={t('home.title')}
            className="text-sm font-semibold tracking-tight text-pc-text-muted hover:text-pc-text"
          >
            ZeroClaw
          </Link>
          {!home && (
            <nav
              aria-label={t('workspace.choose')}
              className="flex items-center gap-1 text-sm"
            >
              {[
                ['/agent', 'workspace.agent'],
                ['/code', 'nav.code'],
                ['/sops', 'workspace.sop'],
              ].map(([to, label]) => (
                <Link
                  key={to}
                  to={to!}
                  aria-current={pathname.startsWith(to!) ? 'page' : undefined}
                  className={`rounded-lg px-2 py-1.5 sm:px-3 ${pathname.startsWith(to!) ? 'bg-pc-elevated text-pc-text' : 'text-pc-text-muted hover:text-pc-text'}`}
                >
                  {t(label!)}
                </Link>
              ))}
            </nav>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={onOpenPalette}
            aria-label={t('workspace.search_settings')}
            title={t('workspace.search_settings')}
            className="flex items-center gap-2 rounded-lg p-2 text-pc-text-muted hover:bg-pc-elevated hover:text-pc-text"
          >
            <Search className="h-4 w-4" />
            <kbd className="hidden text-xs sm:inline">⌘ K</kbd>
          </button>
          <button
            type="button"
            onClick={() => setMenu(!menu)}
            aria-expanded={menu}
            aria-label={t('workspace.more')}
            className="rounded-lg p-2 text-pc-text-muted hover:bg-pc-elevated"
          >
            <MoreHorizontal className="h-4 w-4" />
          </button>
        </div>
        {menu && (
          <>
            <button
              type="button"
              aria-label={t('common.close')}
              className="fixed inset-0 cursor-default"
              onClick={() => setMenu(false)}
            />
            <div className="absolute right-4 top-12 z-10 w-56 rounded-xl border border-pc-border bg-pc-surface p-2 text-sm shadow-xl">
              <button
                className="w-full rounded-lg p-2 text-left hover:bg-pc-elevated"
                onClick={() => {
                  setMenu(false);
                  openSettings('/config');
                }}
              >
                {t('config.all_settings')}
              </button>
              <button
                className="w-full rounded-lg p-2 text-left hover:bg-pc-elevated"
                onClick={() => {
                  setMenu(false);
                  setAppearance(true);
                }}
              >
                {t('nav.appearance')}
              </button>
              <label className="flex items-center gap-2 p-2 text-pc-text-muted">
                {t('settings.language')}
                <select
                  aria-label={t('settings.language')}
                  className="min-w-0 flex-1 bg-pc-surface text-pc-text"
                  value={locale}
                  onChange={(e) => setAppLocale(e.target.value)}
                >
                  {SUPPORTED_LOCALES.map(({ code, name }) => (
                    <option key={code} value={code}>
                      {name}
                    </option>
                  ))}
                </select>
              </label>
              <button
                className="w-full rounded-lg p-2 text-left hover:bg-pc-elevated"
                onClick={() => {
                  if (window.confirm(t('auth.logout_confirm'))) logout();
                }}
              >
                {t('auth.logout')}
              </button>
            </div>
          </>
        )}
      </header>
      <SettingsModal open={appearance} onClose={() => setAppearance(false)} />
    </>
  );
}
