import { useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import {
  Activity,
  ArrowRight,
  Bot,
  Code2,
  History,
  Search,
  Settings,
  Workflow,
} from "lucide-react";
import {
  getRunningSessions,
  getSessions,
  getWorkspaceAvailability,
  type WorkspaceAvailability,
} from "@/lib/api";
import { listRuns, type SopRunSummary } from "@/lib/sops";
import { sessionTarget } from "@/lib/sessionNavigation";
import { formatRelative } from "@/lib/format";
import { t } from "@/lib/i18n";
import { useCodeSessions } from "@/hooks/useCodeSessions";
import { usePolling } from "@/hooks/usePolling";
import type { Session } from "@/types/api";
import Dashboard from "./Dashboard";

type Snapshot<T> = { data: T | null; error: boolean };

/** Root bookmarks with the old tab query still reach their original content. */
export default function Home() {
  const [params] = useSearchParams();
  return params.has("tab") ? <Dashboard /> : <WorkHome />;
}

function WorkHome() {
  const [availability, setAvailability] = useState<
    Snapshot<WorkspaceAvailability>
  >({ data: null, error: false });
  const [history, setHistory] = useState<Snapshot<Session[]>>({
    data: null,
    error: false,
  });
  const [running, setRunning] = useState<Snapshot<string[]>>({
    data: null,
    error: false,
  });
  const [runs, setRuns] = useState<Snapshot<SopRunSummary[]>>({
    data: null,
    error: false,
  });
  const [query, setQuery] = useState("");
  const code = useCodeSessions(availability.data?.code === true);

  // Each surface settles independently: a failed optional service cannot hide history.
  usePolling(async (stale) => {
    const update = async <T,>(
      read: () => Promise<T>,
      write: (value: Snapshot<T>) => void,
    ) => {
      try {
        const data = await read();
        if (!stale()) write({ data, error: false });
      } catch {
        if (!stale()) write({ data: null, error: true });
      }
    };
    await Promise.all([
      update(getWorkspaceAvailability, setAvailability),
      update(getSessions, setHistory),
      update(
        async () =>
          (await getRunningSessions()).sessions.map(
            (session) => `gw_${session.session_id}`,
          ),
        setRunning,
      ),
    ]);
  }, 5000);
  usePolling(
    async (stale) => {
      try {
        const data = await listRuns();
        if (!stale()) setRuns({ data, error: false });
      } catch {
        if (!stale()) setRuns({ data: null, error: true });
      }
    },
    5000,
    [],
    availability.data?.workflows === true,
  );

  const sessions = [...(history.data ?? []), ...code.sessions].sort((a, b) =>
    b.last_activity.localeCompare(a.last_activity),
  );
  const active = sessions.filter(
    (session) =>
      running.data?.includes(session.session_key) ||
      ("state" in session && session.state === "running"),
  );
  const activeRuns = (runs.data ?? []).filter((run) => run.active);
  const recent = sessions
    .filter(
      (session) =>
        !query ||
        [
          session.name,
          session.session_id,
          session.agent_alias,
          session.channel_id,
        ].some((value) => value?.toLowerCase().includes(query.toLowerCase())),
    )
    .slice(0, 8);
  const agents = availability.data?.agents ?? [];
  const actionClass =
    "flex items-center gap-3 rounded-xl border border-pc-border bg-pc-surface p-4 text-sm font-medium hover:border-pc-accent/50 hover:bg-pc-elevated focus-visible:ring-2 focus-visible:ring-pc-accent";

  return (
    <div className="mx-auto max-w-6xl p-4 sm:p-8 space-y-8">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight">
            {t("home.heading")}
          </h2>
          <p className="mt-2 text-sm text-pc-text-muted">
            {t("home.description")}
          </p>
        </div>
        <Link
          to="/config"
          className="inline-flex items-center gap-2 rounded-lg px-3 py-2 text-sm text-pc-text-muted hover:bg-pc-elevated"
        >
          <Settings className="h-4 w-4" />
          {t("nav.config")}
        </Link>
      </div>

      {availability.error ? (
        <Unavailable />
      ) : !availability.data ? (
        <Loading />
      ) : agents.length === 0 ? (
        <div className="rounded-xl border border-pc-border bg-pc-surface p-6">
          <h3 className="font-medium">{t("home.setup_title")}</h3>
          <p className="mt-1 text-sm text-pc-text-muted">
            {t("home.setup_hint")}
          </p>
          <Link
            to="/quickstart"
            className="mt-4 inline-flex items-center gap-2 text-sm text-pc-accent"
          >
            {t("nav.quickstart")}
            <ArrowRight className="h-4 w-4" />
          </Link>
        </div>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          <Link
            to={
              agents.length === 1
                ? `/agent/${encodeURIComponent(agents[0]!)}`
                : "/agents"
            }
            className={actionClass}
          >
            <Bot className="h-5 w-5 text-pc-accent" />
            <span className="flex-1">{t("home.chat")}</span>
            <ArrowRight className="h-4 w-4 text-pc-text-muted" />
          </Link>
          {availability.data.code && (
            <Link to="/code" className={actionClass}>
              <Code2 className="h-5 w-5 text-pc-accent" />
              <span className="flex-1">{t("home.code")}</span>
              <ArrowRight className="h-4 w-4 text-pc-text-muted" />
            </Link>
          )}
          {availability.data.workflows && (
            <Link to="/sops" className={actionClass}>
              <Workflow className="h-5 w-5 text-pc-accent" />
              <span className="flex-1">{t("nav.sops")}</span>
              <ArrowRight className="h-4 w-4 text-pc-text-muted" />
            </Link>
          )}
        </div>
      )}

      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_18rem]">
        <section className="min-w-0" aria-labelledby="recent-heading">
          <div className="flex flex-wrap items-center justify-between gap-3 mb-4">
            <h3
              id="recent-heading"
              className="font-semibold flex items-center gap-2"
            >
              <History className="h-4 w-4 text-pc-text-muted" />
              {t("home.recent")}
            </h3>
            <Link to="/sessions" className="text-sm text-pc-accent">
              {t("home.all_sessions")}
              <span aria-hidden="true"> →</span>
            </Link>
          </div>
          <div className="relative mb-3">
            <Search className="absolute left-3 top-3 h-4 w-4 text-pc-text-muted" />
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              aria-label={t("home.search_sessions")}
              placeholder={t("home.search_sessions")}
              className="input-electric w-full pl-9 pr-3 py-2 text-sm"
            />
          </div>
          <div className="rounded-xl border border-pc-border bg-pc-surface overflow-hidden">
            {history.error && <Unavailable />}
            {code.error && <Unavailable />}
            {!history.data && !history.error ? (
              <Loading />
            ) : recent.length === 0 ? (
              <p className="p-8 text-sm text-pc-text-muted">
                {t(
                  query
                    ? "home.no_matches"
                    : availability.data?.session_persistence === false
                      ? "home.history_disabled"
                      : "home.no_sessions",
                )}
              </p>
            ) : (
              recent.map((session) => (
                <Link
                  key={session.session_key}
                  to={sessionTarget(session)}
                  className="group flex items-center gap-3 px-4 py-4 border-b last:border-b-0 border-pc-border hover:bg-pc-elevated focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-pc-accent"
                >
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm font-medium">
                      {session.name ||
                        ("surface" in session
                          ? `${t("nav.code")} · ${session.agent_alias}`
                          : session.session_id)}
                    </p>
                    <p className="mt-1 truncate text-xs text-pc-text-muted">
                      {[session.agent_alias, session.channel_id]
                        .filter(Boolean)
                        .join(" · ") || t("home.sessions")}
                    </p>
                  </div>
                  <time
                    dateTime={session.last_activity}
                    title={new Date(session.last_activity).toLocaleString()}
                    className="shrink-0 text-xs text-pc-text-muted"
                  >
                    {formatRelative(session.last_activity)}
                  </time>
                  <ArrowRight className="h-4 w-4 shrink-0 text-pc-text-muted group-hover:text-pc-accent" />
                </Link>
              ))
            )}
          </div>
        </section>

        <aside className="space-y-6">
          <section aria-labelledby="active-heading">
            <h3
              id="active-heading"
              className="mb-4 flex items-center gap-2 font-semibold"
            >
              <Activity className="h-4 w-4 text-pc-accent" />
              {t("home.active")}
            </h3>
            <div className="rounded-xl border border-pc-border bg-pc-surface p-4 space-y-3">
              {running.error ||
              history.error ||
              code.error ||
              (availability.data?.workflows && runs.error) ? (
                <Unavailable />
              ) : !running.data || !code.loaded ? (
                <Loading />
              ) : active.length + activeRuns.length === 0 ? (
                <p className="text-sm text-pc-text-muted">{t("home.idle")}</p>
              ) : (
                <>
                  {active.map((session) => (
                    <Link
                      key={session.session_key}
                      to={sessionTarget(session)}
                      className="block rounded-lg p-2 hover:bg-pc-elevated"
                    >
                      <p className="truncate text-sm font-medium">
                        {session.agent_alias || session.session_id}
                      </p>
                      <p className="text-xs text-pc-text-muted mt-1">
                        {t(
                          "surface" in session
                            ? "code.working"
                            : "home.running_chat",
                        )}
                      </p>
                    </Link>
                  ))}
                  {activeRuns.map((run) => (
                    <Link
                      key={run.run_id}
                      to={`/runs/${encodeURIComponent(run.sop_name)}/${encodeURIComponent(run.run_id)}`}
                      className="block rounded-lg p-2 hover:bg-pc-elevated"
                    >
                      <p className="truncate text-sm font-medium">
                        {run.sop_name}
                      </p>
                      <p className="text-xs text-pc-text-muted mt-1">
                        {t("nav.runs")} · {run.current_step + 1}/
                        {run.total_steps}
                      </p>
                    </Link>
                  ))}
                </>
              )}
              <p className="text-xs text-pc-text-faint">
                {t("home.activity_scope")}
              </p>
            </div>
          </section>
          {agents.length > 0 && (
            <section aria-labelledby="agents-heading">
              <div className="mb-3 flex items-center justify-between">
                <h3 id="agents-heading" className="text-sm font-semibold">
                  {t("home.available_agents")}
                </h3>
                <Link to="/agents" className="text-xs text-pc-accent">
                  {t("home.view_all")}
                </Link>
              </div>
              {agents.slice(0, 5).map((alias) => (
                <div
                  key={alias}
                  className="flex items-center gap-2 rounded-lg hover:bg-pc-elevated"
                >
                  <Link
                    to={`/agent/${encodeURIComponent(alias)}`}
                    className="flex min-w-0 flex-1 items-center gap-2 px-2 py-3 text-sm"
                  >
                    <Bot className="h-4 w-4 shrink-0 text-pc-text-muted" />
                    <span className="truncate">{alias}</span>
                  </Link>
                  <Link
                    to={`/config/agents/${encodeURIComponent(alias)}`}
                    className="p-2 text-pc-text-muted"
                    aria-label={`${t("nav.feature_settings")}: ${alias}`}
                  >
                    <Settings className="h-4 w-4" />
                  </Link>
                </div>
              ))}
            </section>
          )}
        </aside>
      </div>
    </div>
  );
}

function Loading() {
  return (
    <p role="status" className="p-4 text-sm text-pc-text-muted">
      {t("common.loading")}
    </p>
  );
}
function Unavailable() {
  return (
    <p role="status" className="p-4 text-sm text-status-error">
      {t("home.unavailable")}
    </p>
  );
}
