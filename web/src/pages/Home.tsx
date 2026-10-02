import { Link, Navigate, useSearchParams } from 'react-router-dom';
import { Bot, Code2, Workflow } from 'lucide-react';
import { t } from '@/lib/i18n';

export default function Home() {
  const [params] = useSearchParams();
  if (params.has('tab')) return <Navigate to={`/system?${params}`} replace />;
  return (
    <div className="flex min-h-full items-center justify-center px-6 py-16">
      <nav
        aria-label={t('workspace.choose')}
        className="grid w-full max-w-3xl grid-cols-1 gap-3 sm:grid-cols-3 sm:gap-6"
      >
        {[
          { to: '/agent', label: 'workspace.agent', icon: Bot },
          { to: '/code', label: 'nav.code', icon: Code2 },
          { to: '/sops', label: 'workspace.sop', icon: Workflow },
        ].map(({ to, label, icon: Icon }) => (
          <Link
            key={to}
            to={to}
            className="group flex items-center justify-center gap-4 rounded-2xl px-8 py-10 text-xl font-medium text-pc-text-secondary transition-colors hover:bg-pc-elevated hover:text-pc-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-pc-accent sm:flex-col sm:gap-6 sm:py-16"
          >
            <Icon
              className="h-8 w-8 stroke-[1.25] text-pc-text-muted group-hover:text-pc-accent"
              aria-hidden
            />
            {t(label)}
          </Link>
        ))}
      </nav>
    </div>
  );
}
