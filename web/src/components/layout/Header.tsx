import { useState, useRef, useEffect } from 'react';
import { Link, useLocation } from 'react-router-dom';
import { LogOut, Palette, Settings, ChevronDown, Menu, Globe, Search } from 'lucide-react';
import { t, SUPPORTED_LOCALES } from '@/lib/i18n';
import { useLocaleContext } from '@/App';
import { useAuth } from '@/hooks/useAuth';
import { SettingsModal } from '@/components/SettingsModal';
import { Button } from '@/components/ui';

import { featureSettingsPath, routeTitleKey } from '@/lib/navigation';

interface HeaderProps {
  onMenuToggle: () => void;
  onOpenPalette: () => void;
}

export default function Header({ onMenuToggle, onOpenPalette }: HeaderProps) {
  const location = useLocation();
  const { logout } = useAuth();
  const { locale, setAppLocale } = useLocaleContext();
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [langOpen, setLangOpen] = useState(false);
  const langRef = useRef<HTMLDivElement>(null);

  // Fall back to a plain title for unknown routes rather than mislabeling
  // them as "Dashboard" — e.g. early /quickstart hits before the entry was
  // mapped here showed "Dashboard" for the first-run flow.
  const titleKey = routeTitleKey(location.pathname);
  const settingsPath = featureSettingsPath(location.pathname);
  const pageTitle = titleKey ? t(titleKey) : '';

  const handleLogout = () => {
    if (window.confirm(t('auth.logout_confirm'))) {
      logout();
    }
  };

  // Close dropdown when clicking outside
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (langRef.current && !langRef.current.contains(e.target as Node)) {
        setLangOpen(false);
      }
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, []);

  return (
    <>
      <header className="h-14 flex items-center justify-between px-3 sm:px-6 border-b animate-fade-in relative" style={{ background: 'var(--pc-bg-surface)', borderColor: 'var(--pc-border)', backdropFilter: 'blur(12px)', zIndex: 100 }}>
        <div className="flex items-center gap-3 min-w-0">
          {/* Hamburger — opens the mobile drawer; hidden on desktop where the
              slim rail is always present. */}
          <Button
            variant="ghost"
            onClick={onMenuToggle}
            className="md:hidden h-9 w-9 -ml-1.5 border-transparent px-0 shrink-0"
            aria-label={t('header.open_menu')}
          >
            <Menu className="h-5 w-5 shrink-0" />
          </Button>

          {/* Page title — the primary on-screen label for the current section
              now that the rail is icon-only. Truncates (rather than pushing the
              action cluster) so the right-side icons never get squeezed. */}
          <h1 className="h-9 leading-9 text-lg font-semibold tracking-tight truncate min-w-0" style={{ color: 'var(--pc-text-primary)' }}>{pageTitle}</h1>
        </div>

        {/* Right-side controls. shrink-0 so the title yields first and these
            keep their size (icons here are flex items that would otherwise
            collapse in a tight row). */}
        <div className="flex items-center gap-2 h-9 shrink-0">
          {/* Command-palette trigger — styled like a search field. Opens the
              palette; the same action is bound globally to ⌘K / Ctrl+K, shown
              as a hint chip on the right. */}
          <button
            type="button"
            onClick={onOpenPalette}
            className="hidden sm:flex h-9 items-center gap-2 rounded-[var(--radius-md)] border border-pc-border bg-pc-input pl-2.5 pr-2 text-sm text-pc-text-muted transition-colors hover:border-pc-border-strong hover:text-pc-text-secondary hover:bg-[var(--pc-hover)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-pc-surface"
            aria-label={t('nav.cmdk.placeholder')}
          >
            <Search className="h-[20px] w-[20px] shrink-0" aria-hidden="true" />
            <span className="hidden md:inline w-40 truncate text-left">{t('nav.search_short')}</span>
            <kbd className="ml-1 flex items-center gap-0.5 rounded-[var(--radius-sm)] border border-pc-border bg-pc-elevated px-1.5 py-0.5 text-[11px] font-mono text-pc-text-faint">
              <span className="text-[13px] leading-none">⌘</span>K
            </kbd>
          </button>

          {/* Command-palette trigger — compact icon-only on the smallest screens. */}
          <Button
            variant="ghost"
            onClick={onOpenPalette}
            className="sm:hidden h-9 w-9 border-transparent px-0"
            aria-label={t('nav.cmdk.placeholder')}
          >
            <Search className="h-[20px] w-[20px] shrink-0" />
          </Button>

          {settingsPath && (
            <Link to={settingsPath} className="flex h-9 w-9 items-center justify-center rounded-md hover:bg-pc-elevated focus-visible:ring-2 focus-visible:ring-pc-accent" aria-label={t('nav.feature_settings')} title={t('nav.feature_settings')}>
              <Settings className="h-5 w-5" />
            </Link>
          )}
          {/* Appearance */}
          <Button
            variant="ghost"
            onClick={() => setSettingsOpen(true)}
            className="h-9 w-9 border-transparent px-0"
            aria-label={t('nav.appearance')}
          >
            <Palette className="h-[20px] w-[20px] shrink-0" />
          </Button>

          {/* Language switcher dropdown */}
          <div ref={langRef} className="relative" style={{ zIndex: 9999 }}>
            <Button
              variant="ghost"
              onClick={() => setLangOpen(!langOpen)}
              aria-expanded={langOpen}
              aria-label={t('settings.language')}
              className="h-9 w-9 sm:w-auto px-0 sm:px-3 text-xs font-semibold gap-1.5"
              style={{ background: 'var(--pc-bg-elevated)' }}
            >
              <Globe className="h-[20px] w-[20px] shrink-0" />
              <span className="hidden sm:inline">{locale.toUpperCase()}</span>
              <ChevronDown className="hidden sm:block h-3 w-3 shrink-0" style={{ transform: langOpen ? 'rotate(180deg)' : undefined, transition: 'transform 0.15s' }} />
            </Button>

            {langOpen && (
              <div
                className="absolute right-0 top-full mt-1 rounded-xl border overflow-hidden shadow-lg"
                style={{
                  background: 'var(--pc-bg-elevated)',
                  borderColor: 'var(--pc-border)',
                  maxHeight: '360px',
                  overflowY: 'auto',
                  minWidth: '200px',
                  zIndex: 9999,
                }}
              >
                {SUPPORTED_LOCALES.map(({ code, name }) => (
                  <button
                    key={code}
                    type="button"
                    onClick={() => {
                      setAppLocale(code);
                      setLangOpen(false);
                    }}
                    className="w-full px-3 py-2 text-xs text-left flex items-center gap-2.5 transition-colors"
                    style={{
                      color: code === locale ? 'var(--pc-accent)' : 'var(--pc-text-secondary)',
                      background: code === locale ? 'var(--pc-accent-glow)' : 'transparent',
                      fontWeight: code === locale ? 600 : 400,
                    }}
                    onMouseEnter={(e) => {
                      if (code !== locale) {
                        e.currentTarget.style.background = 'var(--pc-hover)';
                        e.currentTarget.style.color = 'var(--pc-text-primary)';
                      }
                    }}
                    onMouseLeave={(e) => {
                      if (code !== locale) {
                        e.currentTarget.style.background = 'transparent';
                        e.currentTarget.style.color = 'var(--pc-text-secondary)';
                      }
                    }}
                  >
                    <span className="flex-1">{name}</span>
                    <span className="font-mono opacity-40">{code.toUpperCase()}</span>
                  </button>
                ))}
              </div>
            )}
          </div>

          {/* Logout */}
          <Button
            variant="ghost"
            onClick={handleLogout}
            className="h-9 w-9 sm:w-auto px-0 sm:px-3 text-xs gap-1.5 hover:text-status-error hover:border-status-error/25 hover:bg-status-error/10"
            aria-label={t('auth.logout')}
          >
            <LogOut className="h-[20px] w-[20px] shrink-0" />
            <span className="hidden sm:inline">{t('auth.logout')}</span>
          </Button>
        </div>
      </header>

      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </>
  );
}
