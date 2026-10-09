// Settings window — the place where anything that writes to disk is confirmed.
// Stage 2 covers the Claude Code hooks and the general preferences; API keys and
// integrations land here too in a later stage.

import "./settings.css";
import {
  Bridge,
  IS_TAURI,
  onEvent,
  type BootInfo,
  type HookStatus,
  type LocalModel,
} from "../core/bridge";
import {
  DEFAULT_SETTINGS,
  OLLAMA_DEFAULT_BASE_URL,
  PROVIDER_INFO,
  PROVIDER_LABELS,
  PROVIDER_MODELS,
  apiKeyForProvider,
  type AgentId,
  type Provider,
  type Settings,
} from "../core/state";
import { h, clear } from "../views/dom";

let settings: Settings = { ...DEFAULT_SETTINGS };
let version = "";
/**
 * Whether `settings` holds what the app actually has, or is still the defaults.
 *
 * `Bridge.boot()` answers null when the IPC is not ready yet, and the settings
 * window is created at startup — so this window can render before the app has
 * answered. Saving at that point would write the defaults over a real
 * settings.json, and nothing here is allowed to do that.
 */
let settingsLoaded = false;

const root = document.getElementById("settings-root")!;

async function save() {
  if (!settingsLoaded) {
    console.warn("[coucou] refusing to save settings that were never loaded");
    return;
  }
  await Bridge.saveSettings(settings);
}

// ── Reusable bits ─────────────────────────────────────────────────────────────

function toggle(on: boolean, onChange: (v: boolean) => void): HTMLElement {
  const el = h("button", { class: on ? "switch on" : "switch", "aria-pressed": on });
  el.addEventListener("click", () => {
    const next = !el.classList.contains("on");
    el.classList.toggle("on", next);
    onChange(next);
  });
  return el;
}

function statusDot(ok: boolean): HTMLElement {
  return h("i", { class: "dot", style: `background:${ok ? "#22c55e" : "#f4505e"}` });
}

function renderDiff(text: string): HTMLElement {
  const box = h("div", { class: "diff" });
  for (const line of text.split("\n")) {
    const cls = line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : "ctx";
    box.append(h("div", { class: cls, text: line }));
  }
  return box;
}

// ── Coding agents (Claude Code, Codex, Cursor) ────────────────────────────────

/** What each agent gets out of being hooked, in the user's words. */
const AGENT_NOTES: Record<AgentId, { blurb: string; after: string }> = {
  claude: {
    blurb: "Tool calls, questions and permission requests show up in the island, and you can answer them there.",
    after: "Open a new Claude Code session to pick the hooks up.",
  },
  codex: {
    blurb: "Codex speaks Claude Code's hook protocol, so turns, tools and permission requests land in the island — and you can approve from it.",
    after: "Open a new Codex session to pick the hooks up. If Codex asks you to trust them, say yes: run /hooks in Codex.",
  },
  kimi: {
    blurb: "Prompts, tool calls, subagents and the end of a turn show up in the island, one pill of their own.",
    after: "Open a new Kimi session to pick the hooks up. Kimi answers its own approval prompts, so those stay in Kimi.",
  },
};

function agentSection(agent: AgentId, status: HookStatus): HTMLElement {
  const notes = AGENT_NOTES[agent];
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const head = () => h("h2", {}, statusDot(status.installed), h("span", { text: status.name }));
  const section = h("section", {}, head(), body);

  const rebuild = async () => {
    const fresh = (await Bridge.hooksStatus())?.find((s) => s.provider === agent);
    if (fresh) Object.assign(status, fresh);
    clear(body);
    draw();
    const title = section.querySelector("h2")!;
    clear(title);
    title.append(statusDot(status.installed), h("span", { text: status.name }));
  };

  function draw() {
    body.append(
      h("div", {
        class: "hint",
        text: status.installed
          ? `Coucou is hooked into your ${status.name} sessions. ${notes.blurb}`
          : `Install the hooks to see your ${status.name} sessions in the island. ${notes.blurb}`,
      }),
      h("div", { class: "row" },
        h("label", { text: "Config" }),
        h("span", { class: "path", text: status.settingsPath }),
      ),
      h("div", { class: "row" },
        h("label", { text: "Relay" }),
        h("span", { class: "path", text: status.hookPath }),
        statusDot(status.hookReady),
      ),
    );

    if (!status.hookReady) {
      body.append(h("div", {
        class: "notice warn",
        text: "coucou-hook.exe is not in place yet. Restart Coucou; if it still fails, build it with `cargo build -p coucou-hook`.",
      }));
    }

    const actions = h("div", { class: "row" });
    const install = h("button", {
      class: "primary",
      text: status.installed ? "Reinstall hooks…" : "Install hooks…",
      onclick: () => void showPreview(true),
    });
    // Writing hook commands that point at a relay which isn't there would give
    // every session a broken hook and nothing to show for it.
    if (!status.hookReady) {
      install.disabled = true;
      install.title = "The relay isn't installed yet.";
    }
    actions.append(install);
    if (status.installed) {
      actions.append(h("button", {
        class: "danger",
        text: "Uninstall hooks…",
        onclick: () => void showPreview(false),
      }));
    }
    body.append(actions);
  }

  async function showPreview(install: boolean) {
    let preview;
    try {
      preview = await Bridge.hooksPreview(agent, install);
    } catch (err) {
      // An unreadable or invalid config stops here rather than being treated as
      // empty and written over.
      clear(body);
      body.append(
        h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }),
        h("div", { class: "row" }, h("button", {
          text: "Back",
          onclick: () => { clear(body); draw(); },
        })),
      );
      return;
    }
    if (!preview) return;
    clear(body);
    body.append(
      h("div", {
        class: "hint",
        text: install
          ? `This is exactly what will change in ${preview.settingsPath}. Your own hooks are left untouched.`
          : "This removes Coucou's entries only. Your own hooks are left untouched.",
      }),
      renderDiff(preview.diff),
      h("div", { class: "row" },
        h("span", { class: "path", text: `Backup → ${preview.backup}` }),
      ),
    );
    const confirm = h("button", {
      class: install ? "primary" : "danger",
      text: install ? "Back up and write" : "Back up and remove",
    });
    confirm.addEventListener("click", async () => {
      confirm.disabled = true;
      try {
        const backup = await Bridge.hooksApply(agent, install, preview.fingerprint);
        clear(body);
        body.append(h("div", {
          class: "notice ok",
          text: `Done. Previous settings saved as ${backup}. ${notes.after}`,
        }));
        window.setTimeout(() => void rebuild(), 2600);
      } catch (err) {
        confirm.disabled = false;
        body.append(h("div", { class: "notice err", text: `Could not write: ${String(err)}` }));
      }
    });
    body.append(h("div", { class: "row" }, confirm, h("button", {
      text: "Cancel",
      onclick: () => { clear(body); draw(); },
    })));
  }

  draw();
  return section;
}

function agentsSection(statuses: HookStatus[]): HTMLElement[] {
  return (["claude", "codex", "kimi"] as AgentId[])
    .map((agent) => statuses.find((s) => s.provider === agent))
    .filter((s): s is HookStatus => Boolean(s))
    .map((s) => agentSection(s.provider, s));
}


// ── Providers: one key, one model list each ──────────
//
// The key lives with the provider, so one OpenAI key is typed once and every
// agent set to OpenAI benefits. An agent's section only says *who* answers for it
// and with which model — never a key.

function modelLabel(model: LocalModel): string {
  const bits: string[] = [];
  if (model.parameterSize) bits.push(model.parameterSize);
  if (model.family) bits.push(model.family);
  const info = bits.length ? ` — ${bits.join(" ")}` : "";
  return `${model.name}${info}${model.cloud ? " (cloud)" : ""}`;
}

/** Provider: its key, its address, and the model it defaults to. */
function providerSection(provider: Provider, hasKey: boolean): HTMLElement {
  const info = PROVIDER_INFO[provider];
  const config = settings.providers[provider] ?? (settings.providers[provider] = {
    model: "", baseUrl: "",
  });
  const feedback = h("div", {});
  const dot = statusDot(hasKey && info.needsKey);
  const keyName = apiKeyForProvider(provider);

  const state = h("span", {
    class: "hint",
    text: info.needsKey
      ? hasKey
        ? "Key saved in the Windows Credential Manager."
        : info.note
      : info.note,
  });

  const keyField = h("input", {
    type: "password",
    placeholder: hasKey ? "••••••••••••  (stored)" : info.placeholder,
    style: "flex:1 1 auto;min-width:0",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;
  const saveBtn = h("button", { class: "primary", text: "Save key" });
  const clearBtn = h("button", { class: "danger", text: "Remove" });

  const modelField = h("input", {
    type: "text",
    list: `provider-models-${provider}`,
    placeholder: provider === "ollama" ? "llama3.2:latest" : "model name",
    value: config.model,
    style: "flex:1 1 auto;min-width:0",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;
  const datalist = h("datalist", { id: `provider-models-${provider}` });
  for (const model of PROVIDER_MODELS[provider]) {
    datalist.append(h("option", { value: model }));
  }

  const baseField = h("input", {
    type: "text",
    placeholder: info.defaultBaseUrl ?? "https://…",
    value: config.baseUrl,
    style: "flex:1 1 auto;min-width:0",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;
  const detectBtn = h("button", { text: "Detect models" });
  if (provider === "ollama") ollamaModelField = modelField;

  const providerNote = h("div", { class: "hint" });
  ollamaProviderNote = (text: string) => {
    providerNote.textContent = text;
  };

  // Opening the settings window on Ollama with nothing chosen is the state the
  // user just came from, so it gets fixed on the spot.
  if (provider === "ollama") void fillOllamaModel();

  function save() {
    config.model = modelField.value.trim();
    config.baseUrl = baseField.value.trim();
    void Bridge.saveSettings(settings);
  }

  modelField.addEventListener("change", save);
  baseField.addEventListener("change", save);

  async function refreshKey() {
    const present = (await Bridge.secretPresent(keyName)) ?? false;
    dot.style.background = present ? "#22c55e" : "#f4505e";
    state.textContent = info.needsKey
      ? present
        ? "Key saved in the Windows Credential Manager."
        : info.note
      : info.note;
    keyField.placeholder = present ? "••••••••••••  (stored)" : info.placeholder;
    clearBtn.style.display = present ? "" : "none";
  }

  saveBtn.addEventListener("click", async () => {
    const value = keyField.value.trim();
    if (!value) return;
    clear(feedback);
    try {
      await Bridge.secretSet(keyName, value);
      keyField.value = "";
      feedback.append(h("div", { class: "notice ok", text: "Saved. It never touches disk." }));
      await refreshKey();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: `Could not save: ${String(err)}` }));
    }
  });

  clearBtn.addEventListener("click", async () => {
    clear(feedback);
    try {
      await Bridge.secretClear(keyName);
      feedback.append(h("div", { class: "notice ok", text: "Key removed." }));
      await refreshKey();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: `Could not remove: ${String(err)}` }));
    }
  });

  detectBtn.addEventListener("click", async () => {
    clear(feedback);
    baseField.value = baseField.value.trim() || OLLAMA_DEFAULT_BASE_URL;
    config.baseUrl = baseField.value;
    detectBtn.disabled = true;
    detectBtn.textContent = "Detecting…";
    save();
    try {
      const models = await Bridge.ollamaModels();
      clear(datalist);
      for (const model of models) {
        datalist.append(h("option", { value: model.name, label: modelLabel(model) }));
      }
      const local = models.find((m) => !m.cloud);
      if (!config.model && local) {
        config.model = local.name;
        modelField.value = local.name;
        save();
      }
      feedback.append(h("div", {
        class: models.length ? "notice ok" : "notice err",
        text: models.length
          ? `${models.length} model${models.length > 1 ? "s" : ""} found. Pick one above.`
          : "Ollama answered, but has no model yet. Run: ollama pull llama3.2",
      }));
    } catch (err) {
      feedback.append(h("div", {
        class: "notice err",
        text: String(err).replace(/^Error:\s*/, "Could not reach Ollama: "),
      }));
    } finally {
      detectBtn.disabled = false;
      detectBtn.textContent = "Detect models";
    }
  });

  const rows: HTMLElement[] = [];
  if (info.needsKey) {
    rows.push(
      h("div", { class: "row" }, h("label", { text: "API key" }), keyField, saveBtn, clearBtn),
      state,
    );
  } else {
    rows.push(state);
  }
  rows.push(h("div", { class: "row" },
    h("label", { text: "Model" }), modelField, detectBtn, datalist));
  // The address is already configured for the hosted providers: showing it would
  // only ask the user to confirm something Coucou already knows. Only a local
  // daemon has an address worth changing.
  if (info.addressEditable) {
    rows.push(h("div", { class: "row" }, h("label", { text: "Address" }), baseField));
  }
  rows.push(providerNote);

  return h("section", {},
    h("h2", {}, dot, h("span", { text: info.name })),
    ...rows,
    feedback);
}

function providerSections(present: Record<string, boolean>): HTMLElement[] {
  return (Object.keys(PROVIDER_INFO) as Provider[])
    .map((provider) => providerSection(provider, present[apiKeyForProvider(provider)] ?? false));
}

/**
 * Which provider answers the island's chat. One choice for everybody: Claude
 * Code, Codex and Kimi Code each have an account you already pay for, and asking
 * the same question three times was noise.
 */
function chatProviderPicker(): HTMLElement {
  const row = h("div", { class: "row" });
  const select = h("select", {}) as HTMLSelectElement;
  for (const [value, label] of Object.entries(PROVIDER_LABELS)) {
    select.append(h("option", { value, text: label }));
  }
  select.value = settings.chatProvider;
  select.addEventListener("change", () => {
    settings.chatProvider = select.value as Provider;
    void save();
    note.textContent = noteFor(settings.chatProvider);
    // Ollama ships without a model, so choosing it is what makes the chat fail
    // until one exists. Fill it in rather than making that a second errand.
    if (settings.chatProvider === "ollama") void fillOllamaModel();
  });

  const note = h("div", {
    class: "hint",
    text: noteFor(settings.chatProvider),
  });
  chatProviderNote = (provider: Provider) => {
    note.textContent = noteFor(provider);
  };

  row.append(h("label", { text: "Chat answers with" }), select);
  return h("section", {}, h("h2", {}, statusDot(true), h("span", { text: "Chat" })), row, note);
}


/**
 * Picks a local Ollama model for the chat, once.
 *
 * Only ever fills an empty field: a model the user chose stays chosen. `flag`
 * keeps the call from firing twice if it is already running.
 */
let ollamaFillDone = false;
async function fillOllamaModel(): Promise<void> {
  if (!settingsLoaded) return;
  const config = settings.providers.ollama;
  if (!config || config.model.trim() || ollamaFillDone) return;
  ollamaFillDone = true;
  try {
    const models = await Bridge.ollamaModels();
    const local = models.find((m) => !m.cloud) ?? models[0];
    if (!local) return;
    config.model = local.name;
    if (ollamaModelField) ollamaModelField.value = local.name;
    await save();
    chatProviderNote?.(settings.chatProvider);
    ollamaProviderNote?.(`Using ${local.name} for the chat.`);
  } catch {
    // The daemon is not running. The chat says so in words when it is used,
    // and Detect models next to the Model field still works.
    ollamaFillDone = false;
  }
}

// Lets the helper above refresh the two notes it may have invalidated.
let chatProviderNote: ((provider: Provider) => void) | null = null;
let ollamaProviderNote: ((text: string) => void) | null = null;
let ollamaModelField: HTMLInputElement | null = null;

function noteFor(provider: Provider): string {
  const config = settings.providers[provider];
  const model = config?.model || PROVIDER_INFO[provider].models[0] || "no model yet";
  const key = PROVIDER_INFO[provider].needsKey ? "" : " — no key needed";
  return `The island answers with ${PROVIDER_INFO[provider].name} · ${model}${key}.`;
}

// ── Integrations section ──────────────────────────────────────────────────────

interface IntegrationDef {
  id: string;
  name: string;
  color: string;
  /** Credential Manager keys, in the order they are shown. */
  fields: { key: string; label: string; placeholder: string; secret: boolean }[];
}

const INTEGRATIONS: IntegrationDef[] = [
  { id: "integration_stripe", name: "Stripe", color: "#0570DE",
    fields: [{ key: "stripe-api-key", label: "Secret key", placeholder: "sk_live_…", secret: true }] },
  { id: "integration_github", name: "GitHub", color: "#F4505E",
    fields: [{ key: "github-token", label: "Token", placeholder: "ghp_…", secret: true }] },
  { id: "integration_vercel", name: "Vercel", color: "#7C5CFF",
    fields: [{ key: "vercel-token", label: "Token", placeholder: "…", secret: true }] },
  { id: "integration_n8n", name: "n8n", color: "#F29B38",
    fields: [
      { key: "n8n-url", label: "Instance URL", placeholder: "https://n8n.example.com", secret: false },
      { key: "n8n-api-key", label: "API key", placeholder: "…", secret: true },
    ] },
  { id: "integration_resend", name: "Resend", color: "#22C55E",
    fields: [{ key: "resend-api-key", label: "API key", placeholder: "re_…", secret: true }] },
  { id: "integration_notion", name: "Notion", color: "#8C8C8C",
    fields: [{ key: "notion-api-key", label: "Integration token", placeholder: "ntn_…", secret: true }] },
  { id: "integration_calcom", name: "Cal.com", color: "#C9956A",
    fields: [{ key: "calcom-api-key", label: "API key", placeholder: "cal_…", secret: true }] },
];
const MAX_ACTIVE = 4;

function integrationsSection(present: Record<string, boolean>): HTMLElement {
  const note = h("div", { class: "hint" });
  const list = h("div", { style: "display:flex;flex-direction:column;gap:14px" });

  function updateNote() {
    const used = settings.activeIntegrations.length;
    note.textContent = `Pick up to ${MAX_ACTIVE} pills to show next to Mochi — ${used}/${MAX_ACTIVE} in use. Keys are stored in the Windows Credential Manager, never on disk.`;
  }

  for (const def of INTEGRATIONS) {
    const active = settings.activeIntegrations.includes(def.id);
    const sw = h("button", { class: active ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const on = settings.activeIntegrations.includes(def.id);
      if (on) {
        settings.activeIntegrations = settings.activeIntegrations.filter((x) => x !== def.id);
      } else {
        if (settings.activeIntegrations.length >= MAX_ACTIVE) return;
        settings.activeIntegrations = [...settings.activeIntegrations, def.id];
      }
      sw.classList.toggle("on", !on);
      updateNote();
      void save();
    });

    const rows = h("div", { style: "display:flex;flex-direction:column;gap:6px;flex:1 1 auto;min-width:0" });
    for (const field of def.fields) {
      const input = h("input", {
        type: field.secret ? "password" : "text",
        placeholder: present[field.key] ? "••••••••  (stored)" : field.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        style: "flex:1 1 auto;min-width:0",
      }) as HTMLInputElement;
      const saveBtn = h("button", { text: "Save" });
      const dotEl = statusDot(present[field.key] ?? false);
      saveBtn.addEventListener("click", async () => {
        const value = input.value.trim();
        try {
          await Bridge.secretSet(field.key, value);
          present[field.key] = value.length > 0;
          input.value = "";
          input.placeholder = value ? "••••••••  (stored)" : field.placeholder;
          dotEl.style.background = value ? "#22c55e" : "#f4505e";
        } catch {
          dotEl.style.background = "#f5a524";
        }
      });
      rows.append(
        h("div", { class: "row" },
          h("label", { style: "min-width:104px", text: field.label }),
          input, saveBtn, dotEl,
        ),
      );
    }

    list.append(
      h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
        h("div", { style: "display:flex;align-items:center;gap:8px;min-width:132px;padding-top:4px" },
          sw,
          h("i", { class: "dot", style: `background:${def.color}` }),
          h("span", { style: "font-size:12.5px", text: def.name }),
        ),
        rows,
      ),
    );
  }

  updateNote();
  return h("section", {}, h("h2", {}, h("span", { text: "Integrations" })), note, list);
}

// ── General section ───────────────────────────────────────────────────────────

function generalSection(): HTMLElement {
  const volume = h("input", {
    type: "range", min: "0", max: "0.2", step: "0.005",
    value: String(settings.soundVolume),
  }) as HTMLInputElement;
  volume.addEventListener("input", () => {
    settings.soundVolume = Number(volume.value);
    void save();
  });

  const autoClose = h("input", {
    type: "number", min: "5", max: "120", step: "1",
    value: String(Math.round(settings.autoCloseInterval)),
    style: "width:72px",
  }) as HTMLInputElement;
  autoClose.addEventListener("change", () => {
    settings.autoCloseInterval = Math.max(5, Math.min(120, Number(autoClose.value) || 15));
    autoClose.value = String(settings.autoCloseInterval);
    void save();
  });

  const screen = h("select", {}) as HTMLSelectElement;
  screen.append(
    h("option", { value: "primary", text: "Main display" }),
    h("option", { value: "cursor", text: "Display under the cursor" }),
  );
  screen.value = settings.screen;
  screen.addEventListener("change", () => {
    settings.screen = screen.value as Settings["screen"];
    void save();
  });

  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "General" })),
    h("div", { class: "row" },
      h("label", { text: "Sound" }),
      toggle(settings.soundEnabled, (v) => { settings.soundEnabled = v; void save(); }),
      volume,
    ),
    h("div", { class: "row" },
      h("label", { text: "Auto-close" }),
      autoClose,
      h("span", { class: "hint", text: "seconds after you leave the island" }),
    ),
    h("div", { class: "row" },
      h("label", { text: "Island lives on" }),
      screen,
    ),
    h("div", { class: "row" },
      h("label", { text: "Launch at startup" }),
      toggle(settings.autostart, (v) => { settings.autostart = v; void save(); }),
    ),
  );
}

// ── Boot ──────────────────────────────────────────────────────────────────────

async function render() {
  const statuses = (await Bridge.hooksStatus()) ?? [];

  const keys = [
    ...(Object.keys(PROVIDER_INFO) as Provider[]).map(apiKeyForProvider),
    "stripe-api-key", "github-token", "vercel-token",
    "n8n-url", "n8n-api-key", "resend-api-key", "notion-api-key", "calcom-api-key",
  ];
  const present: Record<string, boolean> = {};
  for (const k of keys) present[k] = (await Bridge.secretPresent(k)) ?? false;

  clear(root);
  root.append(
    h("h1", {}, h("span", { text: "Coucou" }), h("span", { class: "version", text: version })),
    ...agentsSection(statuses),
    chatProviderPicker(),
    ...providerSections(present),
    integrationsSection(present),
    generalSection(),
    h("div", {
      class: "hint",
      text: "No telemetry. Network requests only go to the services you configure yourself.",
    }),
  );
}

/**
 * Loads the app's settings, retrying while the IPC is not up.
 *
 * This window is created during startup, so its first `boot` can land before
 * the bridge answers — and `Bridge.call` turns a failure into `null` rather
 * than throwing. Without the retries the window would sit on the defaults for
 * its whole life, and the save guard would then refuse every change the user
 * made in it.
 */
async function loadBoot(): Promise<BootInfo | null> {
  for (let attempt = 0; attempt < 10; attempt++) {
    const boot = await Bridge.boot();
    if (boot?.settings) return boot;
    if (!IS_TAURI) return null; // plain browser, nothing to retry
    await new Promise((r) => setTimeout(r, 300));
  }
  return null;
}

async function main() {
  await bootInto();

  // The other window changes settings too; follow it rather than saving over
  // what it just wrote.
  void onEvent<Settings>("settings-changed", (s) => {
    settings = { ...settings, ...s };
  });

  // The save guard refuses to write settings that were never loaded, so this
  // window has to be able to leave that state. Startup created it hidden, and
  // a `boot` that landed too early can fail; retrying when the user actually
  // looks at the window is what makes it usable instead of inert. Only when
  // there is something to recover — redrawing otherwise would throw away
  // whatever is being typed.
  window.addEventListener("focus", () => {
    if (!settingsLoaded) void bootInto();
  });
}

/** Loads the app's settings and redraws. Does nothing once they are in hand. */
async function bootInto() {
  if (settingsLoaded) return;
  const boot = await loadBoot();
  if (!boot?.settings) {
    if (IS_TAURI) console.warn("[coucou] settings not loaded; will retry on focus");
    return;
  }
  settings = { ...settings, ...boot.settings };
  version = boot.version;
  settingsLoaded = true;
  ollamaFillDone = false; // the chosen model may have changed
  await render();
}

void main();