// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

// herdr-tower: reports this opencode session's turns to the tower.
//
// Installed by `herdr-tower init`; rewritten by it, so edits here are lost.
// opencode has no hook configuration, only plugins, so this is the hook: it
// turns the few events that move a tower row into
// `herdr-tower hook opencode --event <name>`, with a small JSON object on stdin.
//
// Everything is best-effort. A hook that fails must never disturb the session
// it only observes, so every call is detached, unawaited and silenced.

import { spawn } from "node:child_process";

const BIN = __HERDR_TOWER_BIN__;

export const HerdrTower = async ({ client }) => {
  // Outside a herdr pane there is no row to report to.
  if (process.env.HERDR_ENV !== "1" || !process.env.HERDR_PANE_ID) return {};

  const send = (event, payload) => {
    try {
      const child = spawn(BIN, ["hook", "opencode", "--event", event], {
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
      });
      child.on("error", () => {});
      child.stdin.on("error", () => {});
      child.stdin.end(JSON.stringify(payload));
      child.unref();
    } catch {}
  };

  // A subagent's session runs inside this same process, and its turns end long
  // before the one a person is waiting on. Its events are not this row's.
  const children = new Set();
  const noteSession = (info) => {
    if (info && info.id && info.parentID) children.add(info.id);
  };
  const ours = (sessionID) => !!sessionID && !children.has(sessionID);

  // The window of each model, from opencode's own provider list, asked once.
  const windows = new Map();
  let windowsLoaded = null;
  const windowOf = async (providerID, modelID) => {
    if (!windowsLoaded) {
      windowsLoaded = client.config
        .providers()
        .then((res) => {
          for (const p of res?.data?.providers ?? []) {
            for (const [id, m] of Object.entries(p.models ?? {})) {
              const ctx = m?.limit?.context;
              if (ctx > 0) windows.set(`${p.id}/${id}`, ctx);
            }
          }
        })
        .catch(() => {});
    }
    await windowsLoaded;
    return windows.get(`${providerID}/${modelID}`) ?? 0;
  };
  const reported = new Set();
  const REPORTED_KEEP = 256;

  return {
    "chat.message": async (input, output) => {
      if (!ours(input.sessionID)) return;
      const prompt = (output.parts ?? [])
        .filter((p) => p.type === "text" && !p.synthetic)
        .map((p) => p.text)
        .join("\n");
      send("user-prompt-submit", {
        session_id: input.sessionID,
        prompt,
        model: input.model?.modelID ?? "",
      });
    },

    event: async ({ event }) => {
      const p = event.properties ?? {};
      switch (event.type) {
        case "session.created":
        case "session.updated":
          noteSession(p.info);
          return;
        case "session.idle":
          if (ours(p.sessionID)) send("stop", { session_id: p.sessionID });
          return;
        case "permission.asked": {
          if (!ours(p.sessionID)) return;
          const what = p.metadata?.command ?? (p.patterns ?? []).join(" ");
          const message = [p.permission, what].filter(Boolean).join(": ");
          send("notification", { session_id: p.sessionID, message });
          return;
        }
        case "question.asked":
          if (!ours(p.sessionID)) return;
          send("notification", {
            session_id: p.sessionID,
            message: p.questions?.[0]?.question ?? "",
          });
          return;
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
          if (ours(p.sessionID)) send("resume", { session_id: p.sessionID });
          return;
        case "session.error": {
          // Esc in the TUI arrives as an error too; a person stopping a turn
          // is not a failure.
          const err = p.error;
          if (!err || err.name === "MessageAbortedError") return;
          if (p.sessionID && !ours(p.sessionID)) return;
          const message = [err.name, err.data?.message].filter(Boolean).join(": ");
          send("error", { session_id: p.sessionID ?? "", message });
          return;
        }
        case "message.updated": {
          const info = p.info;
          if (info?.role !== "assistant" || !info.time?.completed) return;
          if (!ours(info.sessionID) || reported.has(info.id)) return;
          reported.add(info.id);
          // Only a message's repeat events need catching, so the oldest ids
          // go once there are enough; a set in insertion order drops them first.
          if (reported.size > REPORTED_KEEP) {
            reported.delete(reported.values().next().value);
          }
          const t = info.tokens ?? {};
          const tokens = (t.input ?? 0) + (t.cache?.read ?? 0) + (t.cache?.write ?? 0);
          if (tokens <= 0) return;
          send("usage", {
            session_id: info.sessionID,
            model: info.modelID ?? "",
            context_tokens: tokens,
            context_window: await windowOf(info.providerID, info.modelID),
          });
          return;
        }
      }
    },
  };
};
