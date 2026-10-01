import {
  Activity,
  Bot,
  Clock,
  Code2,
  History,
  House,
  ListChecks,
  MessageSquare,
  Monitor,
  Puzzle,
  Settings,
  Smartphone,
  Sparkles,
  Stethoscope,
  Terminal,
  Workflow,
  Wrench,
  type LucideIcon,
} from "lucide-react";

/** The shared destination catalogue for navigation, search and page titles. */
export interface NavItem {
  to: string;
  icon: LucideIcon;
  labelKey: string;
}

export interface NavGroup {
  headingKey: string;
  items: NavItem[];
}
export const navGroups: NavGroup[] = [
  {
    headingKey: "nav.group.home",
    items: [
      { to: "/", icon: House, labelKey: "home.title" },
      { to: "/sessions", icon: History, labelKey: "home.sessions" },
      { to: "/agents", icon: MessageSquare, labelKey: "nav.agents" },
      { to: "/code", icon: Code2, labelKey: "nav.code" },
      { to: "/sops", icon: Workflow, labelKey: "nav.sops" },
      { to: "/runs", icon: ListChecks, labelKey: "nav.runs" },
    ],
  },
  {
    headingKey: "nav.group.configure",
    items: [
      { to: "/config", icon: Settings, labelKey: "nav.config" },
      { to: "/config/agents", icon: Bot, labelKey: "nav.agent" },
      { to: "/tools", icon: Wrench, labelKey: "nav.tools" },
      { to: "/skills", icon: Sparkles, labelKey: "nav.skills" },
      { to: "/integrations", icon: Puzzle, labelKey: "nav.integrations" },
      { to: "/cron", icon: Clock, labelKey: "nav.cron" },
    ],
  },
  {
    headingKey: "nav.group.operations",
    items: [
      { to: "/system", icon: Activity, labelKey: "nav.system" },
      { to: "/logs", icon: Terminal, labelKey: "nav.logs" },
      { to: "/pairing", icon: Smartphone, labelKey: "nav.pairing" },
      { to: "/doctor", icon: Stethoscope, labelKey: "nav.doctor" },
      { to: "/canvas", icon: Monitor, labelKey: "nav.canvas" },
      { to: "/acp-console", icon: Terminal, labelKey: "nav.acp" },
    ],
  },
];

const railDestinations = navGroups.flatMap(({ headingKey, items }) =>
  items.map((item) => ({ ...item, groupKey: headingKey })),
);

export const destinations = [
  ...railDestinations,
  {
    to: "/system?tab=channels",
    icon: MessageSquare,
    labelKey: "dashboard.channels",
    groupKey: "nav.group.operations",
  },
  {
    to: "/system?tab=memories",
    icon: Bot,
    labelKey: "nav.memory",
    groupKey: "nav.group.operations",
  },
  {
    to: "/system?tab=cost",
    icon: Activity,
    labelKey: "nav.cost",
    groupKey: "nav.group.operations",
  },
  {
    to: "/system?tab=health",
    icon: Stethoscope,
    labelKey: "dashboard.health",
    groupKey: "nav.group.operations",
  },
  {
    to: "/quickstart",
    icon: Sparkles,
    labelKey: "nav.quickstart",
    groupKey: "nav.group.configure",
  },
];

export function routeTitleKey(path: string): string | undefined {
  if (path.startsWith("/agent/")) return "nav.agent";
  if (path.startsWith("/setup/")) return "nav.config";
  if (path === "/quickstart") return "nav.quickstart";
  return destinations
    .filter(
      ({ to }) => path === to || (to !== "/" && path.startsWith(`${to}/`)),
    )
    .sort((a, b) => b.to.length - a.to.length)[0]?.labelKey;
}

/** Context links are routes to the owning configuration, never policy copies. */
export function featureSettingsPath(path: string): string | null {
  const segments = path.split("/").filter(Boolean);
  if (segments[0] === "agent" && segments[1])
    return `/config/agents/${segments[1]}`;
  const sections: Record<string, string> = {
    agents: "agents",
    code: "agents",
    sessions: "agents",
    sops: "sop",
    runs: "sop",
    cron: "cron",
    tools: "risk_profiles",
    skills: "skill_bundles",
    integrations: "mcp",
    pairing: "gateway",
    logs: "observability",
    "acp-console": "acp",
    system: "gateway",
  };
  const section = sections[segments[0] ?? ""];
  return section ? `/config/${section}` : null;
}
