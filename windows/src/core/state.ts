// App state — mirror of AppState.swift (the parts the island needs).

import type { BotEmoteName, BotStateName, IslandMode, IslandViewName } from "./layout";
import type { EyeShape } from "../mochi/engine";

export type AgentSource = "claudeCode" | "codex" | "kimi" | "n8n";
export type PillBadge = "approval" | "finished" | "error";

export interface AgentTask {
  id: string;
  name: string;
  color: string;
  state: BotStateName;
  stepIndex: number;
  steps: string[];
  source: AgentSource;
  isIntegration: boolean;
  emote?: BotEmoteName | null;
  miniEye?: EyeShape | null;
  pillBadge?: PillBadge | null;
  sessionCwd?: string | null;
  /** Folder the session runs in. Context beside the agent's name, never instead. */
  sessionProject?: string | null;
}

export interface ApprovalInfo {
  requestId: string;
  sessionId: string;
  tool: string;
  command: string;
  /** Which agent is asking — Claude Code and Codex both let us answer. */
  agent: AgentId;
}

export interface ChatMessage {
  id: number;
  role: "user" | "assistant";
  content: string;
}

export type PromptContext =
  | { kind: "window"; appName: string; title: string; url?: string }
  | { kind: "file"; name: string; path?: string };

export interface ResultItem {
  label: string;
  detail: string;
  url?: string;
}

export interface SearchResult {
  title: string;
  items: ResultItem[];
  note?: string;
}

const task = (
  id: string, name: string, color: string, source: AgentSource,
): AgentTask => ({
  id, name, color, state: "idle", stepIndex: 0, steps: [], source, isIntegration: true,
});

/** AgentTask.integrationAgents — same ids, names and colours as macOS.
 *
 *  The name is the agent, never the project: a pill has to say *who* is working,
 *  and the folder it works in is shown next to it. */
export const INTEGRATION_AGENTS: AgentTask[] = [
  task("integration_claude", "Claude Code", "#F5F6F8", "claudeCode"),
  task("integration_codex", "Codex", "#10A37F", "codex"),
  task("integration_kimi", "Kimi Code", "#22D3EE", "kimi"),
  task("integration_resend", "Resend", "#22C55E", "n8n"),
  task("integration_n8n", "n8n", "#F29B38", "n8n"),
  task("integration_vercel", "Vercel", "#7C5CFF", "n8n"),
  task("integration_github", "GitHub", "#F4505E", "n8n"),
  task("integration_notion", "Notion", "#8C8C8C", "n8n"),
  task("integration_calcom", "Cal.com", "#C9956A", "n8n"),
  task("integration_stripe", "Stripe", "#0570DE", "n8n"),
];

/** The coding agents Coucou follows through hooks. Always loaded, like Claude. */
export const AGENT_TASK_IDS = {
  claude: "integration_claude",
  codex: "integration_codex",
  kimi: "integration_kimi",
} as const;

export type AgentId = keyof typeof AGENT_TASK_IDS;

export const TOGGLEABLE_INTEGRATION_IDS = [
  "integration_resend", "integration_n8n", "integration_vercel", "integration_github",
  "integration_notion", "integration_calcom", "integration_stripe",
];

/** What an integration poller last reported. */
export interface IntegrationInfo {
  data: Record<string, unknown>;
  error: string | null;
  loaded: boolean;
  configured: boolean;
}

export interface Settings {
  soundEnabled: boolean;
  soundVolume: number;
  autoCloseInterval: number;
  absenceInterval: number;
  activeIntegrations: string[];
  screen: "primary" | "cursor";
  autostart: boolean;
  hooksInstalled: boolean;
  codexHooksInstalled: boolean;
  kimiHooksInstalled: boolean;
  /** Which provider answers the island's chat by default. */
  chatProvider: Provider;
  /** Per provider: the model it defaults to and the address it answers on. */
  providers: Record<Provider, ProviderSettings>;
  /** Model used by the chat, whatever the provider. */
  model: string;
  /** Who answers the chat: "anthropic" or "ollama" (models on this machine). */
  provider: Provider;
  /** Where Ollama listens. Only read when provider is "ollama". */
  baseUrl: string;
}



/** Who answers the island's chat: a hosted API, or a model on this machine. */
export type Provider = "anthropic" | "openai" | "moonshot" | "ollama";

/** One provider's own settings: the model it defaults to and where it lives. */
export interface ProviderSettings {
  model: string;
  baseUrl: string;
}

/** Credential Manager entry holding one provider's API key. */
export const apiKeyForProvider = (provider: Provider) => `api-key-${provider}`;

export const PROVIDER_LABELS: Record<Provider, string> = {
  anthropic: "Claude (Anthropic API)",
  openai: "OpenAI (Codex, GPT)",
  moonshot: "Moonshot (Kimi)",
  ollama: "Ollama — local models",
};

/**
 * What the settings window shows for one provider.
 *
 * The address is already configured: a user picks a model and pastes a key,
 * nothing else. `addressEditable` is only true where the address genuinely
 * differs from machine to machine — a local daemon on another port.
 */
export const PROVIDER_INFO: Record<
  Provider,
  {
    name: string;
    note: string;
    needsKey: boolean;
    placeholder: string;
    models: string[];
    defaultBaseUrl?: string;
    addressEditable?: boolean;
  }
> = {
  anthropic: {
    name: "Anthropic (Claude)",
    note: "Paste an Anthropic API key and pick a Claude model.",
    needsKey: true,
    placeholder: "sk-ant-…",
    models: ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"],
  },
  openai: {
    name: "OpenAI",
    note: "Paste an OpenAI API key and pick a model. The address is api.openai.com — already set.",
    needsKey: true,
    placeholder: "sk-…",
    models: ["gpt-5.1-codex", "gpt-5.1", "gpt-5-mini"],
    defaultBaseUrl: "https://api.openai.com/v1",
  },
  moonshot: {
    name: "Moonshot (Kimi)",
    note: "Paste a Moonshot API key from platform.moonshot.ai and pick a Kimi model. The address is already set.",
    needsKey: true,
    placeholder: "sk-…",
    models: ["kimi-k2-turbo-preview", "kimi-k2-0711-preview", "kimi-latest"],
    defaultBaseUrl: "https://api.moonshot.ai/v1",
  },
  ollama: {
    name: "Ollama (local)",
    note: "No key needed — Ollama runs on this computer. Start it, then Detect models or run: ollama pull llama3.2",
    needsKey: false,
    placeholder: "",
    models: [],
    defaultBaseUrl: "http://127.0.0.1:11434",
    addressEditable: true,
  },
};

/** Model suggestions per provider — a shortcut, not a whitelist. */
export const PROVIDER_MODELS: Record<Provider, string[]> = Object.fromEntries(
  (Object.keys(PROVIDER_INFO) as Provider[]).map((p) => [p, PROVIDER_INFO[p].models]),
) as Record<Provider, string[]>;


export function defaultProviders(): Record<Provider, ProviderSettings> {
  return {
    anthropic: { model: "claude-opus-5", baseUrl: "" },
    openai: { model: "gpt-5.1-codex", baseUrl: PROVIDER_INFO.openai.defaultBaseUrl! },
    moonshot: { model: "kimi-k2-turbo-preview", baseUrl: PROVIDER_INFO.moonshot.defaultBaseUrl! },
    ollama: { model: "", baseUrl: PROVIDER_INFO.ollama.defaultBaseUrl! },
  };
}

/** Where Ollama listens unless the user says otherwise. */
export const OLLAMA_DEFAULT_BASE_URL = "http://127.0.0.1:11434";


export const DEFAULT_SETTINGS: Settings = {
  soundEnabled: true,
  soundVolume: 0.12,
  autoCloseInterval: 15,
  absenceInterval: 180,
  activeIntegrations: [
    "integration_resend", "integration_n8n", "integration_vercel", "integration_github",
  ],
  screen: "primary",
  autostart: false,
  hooksInstalled: false,
  codexHooksInstalled: false,
  kimiHooksInstalled: false,
  model: "claude-opus-5",
  provider: "anthropic",
  baseUrl: OLLAMA_DEFAULT_BASE_URL,
  chatProvider: "anthropic",
  providers: defaultProviders(),
};

type Listener = () => void;

class AppState {
  mode: IslandMode = "hidden";
  view: IslandViewName = "overview";

  tasks: AgentTask[] = [];
  focusId: string | null = null;

  stateOverride: BotStateName | null = null;

  /** Cursor in logical screen pixels, origin top-left (like AppState.mousePosition). */
  mouse = { x: 0, y: 0 };
  /** Cursor relative to the island's top-left corner. */
  mouseInIsland = { x: 0, y: 0 };

  isPinned = false;
  paused = false;

  uploadProgress = 0;
  uploadDuration = 2.4;
  fileDragOver = false;

  promptContext: PromptContext | null = null;
  droppedFile: { name: string; path: string } | null = null;
  noteMessage: string | null = null;
  searchResult: SearchResult | null = null;
  chatHistory: ChatMessage[] = [];
  pendingApproval: ApprovalInfo | null = null;

  integrations: Record<string, IntegrationInfo> = {};

  lastActivity = performance.now();

  settings: Settings = { ...DEFAULT_SETTINGS };

  private listeners = new Set<Listener>();

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Marks the UI dirty; the island re-renders on the next frame. */
  notify() {
    for (const fn of this.listeners) fn();
  }

  get focusTask(): AgentTask | null {
    return this.tasks.find((t) => t.id === this.focusId) ?? this.tasks[0] ?? null;
  }

  get effectiveState(): BotStateName {
    return this.stateOverride ?? this.focusTask?.state ?? "idle";
  }

  get otherTasks(): AgentTask[] {
    return this.tasks.filter((t) => t.id !== this.focusId);
  }

  setFocus(id: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    this.focusId = id;
    t.pillBadge = null;
    this.notify();
  }

  updateTask(id: string, state: BotStateName) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.state = state;
    this.notify();
  }

  appendStep(id: string, step: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.steps.push(step);
    if (t.steps.length > 20) t.steps.shift();
    t.stepIndex = t.steps.length - 1;
    this.notify();
  }

  setPillBadge(id: string, badge: PillBadge | null) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.pillBadge = badge;
    this.notify();
  }

  /** loadIntegrationTasks() — the coding agents always on, the rest opt-in (max 4). */
  loadIntegrationTasks() {
    const always = new Set<string>(Object.values(AGENT_TASK_IDS));
    for (const proto of INTEGRATION_AGENTS) {
      const shouldLoad =
        always.has(proto.id) || this.settings.activeIntegrations.includes(proto.id);
      const idx = this.tasks.findIndex((t) => t.id === proto.id);
      if (shouldLoad && idx < 0) this.tasks.push({ ...proto, steps: [] });
      if (!shouldLoad && idx >= 0) this.tasks.splice(idx, 1);
    }
    // Keep the declared order so pills never shuffle.
    const order = INTEGRATION_AGENTS.map((t) => t.id);
    this.tasks.sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id));
    if (!this.focusId) this.focusId = "integration_claude";
    this.notify();
  }

  toggleIntegration(id: string) {
    if (Object.values(AGENT_TASK_IDS).includes(id as never)) return;
    const active = this.settings.activeIntegrations;
    if (active.includes(id)) {
      this.settings.activeIntegrations = active.filter((x) => x !== id);
      if (this.focusId === id) this.focusId = "integration_claude";
    } else {
      if (active.length >= 4) return;
      this.settings.activeIntegrations = [...active, id];
    }
    this.loadIntegrationTasks();
  }

  defaultView(): IslandViewName {
    return this.tasks.length === 0 ? "empty" : "overview";
  }
}

export const State = new AppState();