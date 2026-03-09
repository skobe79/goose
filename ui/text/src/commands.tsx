import React from "react";
import { Box, Text } from "ink";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import type { GooseClient } from "@block/goose-acp";
import { CRANBERRY, TEAL, GOLD, TEXT_PRIMARY, TEXT_SECONDARY, TEXT_DIM, RULE_COLOR } from "./colors.js";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ModelEntry {
  modelId: string;
  name: string;
  description?: string | null;
}

export interface ExtensionEntry {
  name: string;
  enabled: boolean;
  type: string;
}

export type DialogKind =
  | { kind: "model"; models: ModelEntry[]; currentModelId: string }
  | { kind: "extensions"; extensions: ExtensionEntry[]; mode: "list" | "add" }
  | { kind: "recipe-confirm"; title: string; description: string; prompt: string; instructions?: string };

// ─── Minimal YAML parser (recipe files only) ────────────────────────────────

function parseSimpleYaml(text: string): Record<string, unknown> {
  const result: Record<string, string> = {};
  let currentKey: string | null = null;
  let multilineValue = "";
  let multilineIndent = -1;

  for (const rawLine of text.split("\n")) {
    // Handle multiline scalar (block scalar or indented continuation)
    if (currentKey !== null && multilineIndent >= 0) {
      const stripped = rawLine.replace(/^\s*/, "");
      const indent = rawLine.length - rawLine.trimStart().length;
      if (indent > multilineIndent || (stripped === "" && indent === 0)) {
        multilineValue += (multilineValue ? "\n" : "") + rawLine.slice(multilineIndent + 2);
        continue;
      } else {
        result[currentKey] = multilineValue;
        currentKey = null;
        multilineValue = "";
        multilineIndent = -1;
      }
    }

    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;

    // Skip array items and nested objects for now
    if (line.startsWith("- ")) continue;

    const colonIdx = line.indexOf(":");
    if (colonIdx === -1) continue;

    const key = line.slice(0, colonIdx).trim();
    const rest = line.slice(colonIdx + 1).trim();

    if (rest === "" || rest === "|" || rest === ">") {
      // Block scalar or empty — start multiline
      currentKey = key;
      multilineValue = "";
      multilineIndent = rawLine.length - rawLine.trimStart().length;
      continue;
    }

    // Strip quotes
    let value = rest;
    if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
      value = value.slice(1, -1);
    }

    result[key] = value;
  }

  if (currentKey !== null) {
    result[currentKey] = multilineValue;
  }

  return result;
}

// ─── Recipe loading ──────────────────────────────────────────────────────────

export interface ParsedRecipe {
  title: string;
  description: string;
  prompt?: string;
  instructions?: string;
}

export function loadRecipe(pathArg: string): ParsedRecipe {
  const filePath = resolve(pathArg);
  const raw = readFileSync(filePath, "utf-8");

  let obj: Record<string, unknown>;
  try {
    const parsed = JSON.parse(raw);
    // Handle nested { recipe: { ... } } format
    obj = parsed.recipe ?? parsed;
  } catch {
    const parsed = parseSimpleYaml(raw);
    // Handle nested recipe: key
    obj = (parsed.recipe as Record<string, unknown>) ?? parsed;
  }

  const title = String(obj.title ?? "Untitled Recipe");
  const description = String(obj.description ?? "");
  const prompt = obj.prompt ? String(obj.prompt) : undefined;
  const instructions = obj.instructions ? String(obj.instructions) : undefined;

  if (!prompt && !instructions) {
    throw new Error("Recipe must have at least a 'prompt' or 'instructions' field");
  }

  return { title, description, prompt, instructions };
}

// ─── Slash command parsing ───────────────────────────────────────────────────

export type SlashCommand =
  | { cmd: "model" }
  | { cmd: "extensions" }
  | { cmd: "recipe"; path: string }
  | { cmd: "help" }
  | null;

export function parseSlashCommand(input: string): SlashCommand {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/")) return null;

  const parts = trimmed.split(/\s+/);
  const cmd = parts[0]!.slice(1).toLowerCase();

  switch (cmd) {
    case "model":
    case "models":
      return { cmd: "model" };
    case "ext":
    case "extension":
    case "extensions":
      return { cmd: "extensions" };
    case "recipe":
    case "run": {
      const path = parts.slice(1).join(" ");
      if (!path) return { cmd: "help" };
      return { cmd: "recipe", path };
    }
    case "help":
    case "?":
      return { cmd: "help" };
    default:
      return null;
  }
}

// ─── Fetch helpers ───────────────────────────────────────────────────────────

export async function fetchExtensions(
  client: GooseClient,
): Promise<ExtensionEntry[]> {
  const resp = await client.goose.configExtensions();
  return (resp.extensions as unknown as Array<Record<string, unknown>>).map((e) => ({
    name: String(e.name ?? "unknown"),
    enabled: Boolean(e.enabled ?? true),
    type: String(e.type ?? "unknown"),
  }));
}

export async function setModel(
  client: GooseClient,
  sessionId: string,
  modelId: string,
): Promise<void> {
  await client.unstable_setSessionModel({ sessionId, modelId });
}

export async function addExtension(
  client: GooseClient,
  sessionId: string,
  input: string,
): Promise<void> {
  let config: unknown;

  if (input.startsWith("http://") || input.startsWith("https://")) {
    config = {
      type: "streamable_http",
      name: new URL(input).hostname,
      uri: input,
      description: "",
    };
  } else {
    config = {
      type: "builtin",
      name: input,
      display_name: null,
      timeout: 300,
    };
  }

  await client.goose.extensionsAdd({ session_id: sessionId, config });
}

export async function removeExtension(
  client: GooseClient,
  sessionId: string,
  name: string,
): Promise<void> {
  await client.goose.extensionsRemove({ session_id: sessionId, name });
}

// ─── Dialog Components ───────────────────────────────────────────────────────

export function ModelDialog({
  models,
  currentModelId,
  selectedIdx,
  width,
}: {
  models: ModelEntry[];
  currentModelId: string;
  selectedIdx: number;
  width: number;
}) {
  const dialogWidth = Math.min(width - 7, 62);
  return (
    <Box
      flexDirection="column"
      marginLeft={5}
      marginTop={1}
      paddingX={2}
      paddingY={1}
      borderStyle="round"
      borderColor={TEAL}
      width={dialogWidth}
    >
      <Text color={TEAL} bold>
        🔀 Switch Model
      </Text>
      <Box marginTop={1} flexDirection="column">
        {models.map((m, i) => {
          const active = i === selectedIdx;
          const isCurrent = m.modelId === currentModelId;
          return (
            <Box key={m.modelId}>
              <Text color={active ? TEAL : RULE_COLOR}>
                {active ? " ▸ " : "   "}
              </Text>
              <Text color={active ? TEXT_PRIMARY : TEXT_SECONDARY} bold={active}>
                {m.name}
              </Text>
              {isCurrent && (
                <Text color={TEAL} dimColor> (current)</Text>
              )}
            </Box>
          );
        })}
      </Box>
      <Box marginTop={1}>
        <Text color={TEXT_DIM}>↑↓ select · enter confirm · esc cancel</Text>
      </Box>
    </Box>
  );
}

export function ExtensionsDialog({
  extensions,
  selectedIdx,
  width,
  mode,
}: {
  extensions: ExtensionEntry[];
  selectedIdx: number;
  width: number;
  mode: "list" | "add";
}) {
  const dialogWidth = Math.min(width - 7, 62);

  if (mode === "add") {
    return (
      <Box
        flexDirection="column"
        marginLeft={5}
        marginTop={1}
        paddingX={2}
        paddingY={1}
        borderStyle="round"
        borderColor={GOLD}
        width={dialogWidth}
      >
        <Text color={GOLD} bold>
          ➕ Add Extension
        </Text>
        <Box marginTop={1}>
          <Text color={TEXT_SECONDARY}>
            Type a builtin name or streamable HTTP URL in the input bar, then press enter.
          </Text>
        </Box>
        <Box marginTop={1}>
          <Text color={TEXT_DIM}>esc cancel</Text>
        </Box>
      </Box>
    );
  }

  return (
    <Box
      flexDirection="column"
      marginLeft={5}
      marginTop={1}
      paddingX={2}
      paddingY={1}
      borderStyle="round"
      borderColor={GOLD}
      width={dialogWidth}
    >
      <Text color={GOLD} bold>
        🧩 Extensions
      </Text>
      <Box marginTop={1} flexDirection="column">
        {extensions.length === 0 ? (
          <Text color={TEXT_DIM}>No extensions configured</Text>
        ) : (
          extensions.map((ext, i) => {
            const active = i === selectedIdx;
            return (
              <Box key={ext.name}>
                <Text color={active ? GOLD : RULE_COLOR}>
                  {active ? " ▸ " : "   "}
                </Text>
                <Text color={ext.enabled ? TEAL : TEXT_DIM}>
                  {ext.enabled ? "●" : "○"}{" "}
                </Text>
                <Text color={active ? TEXT_PRIMARY : TEXT_SECONDARY} bold={active}>
                  {ext.name}
                </Text>
                <Text color={TEXT_DIM}> ({ext.type})</Text>
              </Box>
            );
          })
        )}
      </Box>
      <Box marginTop={1}>
        <Text color={TEXT_DIM}>↑↓ select · d remove · a add · esc close</Text>
      </Box>
    </Box>
  );
}

export function RecipeConfirmDialog({
  title,
  description,
  width,
  selectedIdx,
}: {
  title: string;
  description: string;
  width: number;
  selectedIdx: number;
}) {
  const dialogWidth = Math.min(width - 7, 62);
  const options = ["Run recipe", "Cancel"];
  return (
    <Box
      flexDirection="column"
      marginLeft={5}
      marginTop={1}
      paddingX={2}
      paddingY={1}
      borderStyle="round"
      borderColor={CRANBERRY}
      width={dialogWidth}
    >
      <Text color={CRANBERRY} bold>
        📋 Recipe
      </Text>
      <Box marginTop={1} flexDirection="column">
        <Text color={TEXT_PRIMARY} bold>{title}</Text>
        {description ? <Text color={TEXT_SECONDARY}>{description}</Text> : null}
      </Box>
      <Box marginTop={1} flexDirection="column">
        {options.map((opt, i) => {
          const active = i === selectedIdx;
          return (
            <Box key={opt}>
              <Text color={active ? CRANBERRY : RULE_COLOR}>
                {active ? " ▸ " : "   "}
              </Text>
              <Text color={active ? TEXT_PRIMARY : TEXT_SECONDARY} bold={active}>
                {opt}
              </Text>
            </Box>
          );
        })}
      </Box>
      <Box marginTop={1}>
        <Text color={TEXT_DIM}>↑↓ select · enter confirm · esc cancel</Text>
      </Box>
    </Box>
  );
}

export function HelpDialog({ width }: { width: number }) {
  const dialogWidth = Math.min(width - 7, 62);
  const commands = [
    { cmd: "/model", desc: "Switch provider model" },
    { cmd: "/extensions", desc: "List, add, or remove extensions" },
    { cmd: "/recipe <path>", desc: "Load and run a recipe file" },
    { cmd: "/help", desc: "Show this help" },
  ];
  return (
    <Box
      flexDirection="column"
      marginLeft={5}
      marginTop={1}
      paddingX={2}
      paddingY={1}
      borderStyle="round"
      borderColor={TEXT_SECONDARY}
      width={dialogWidth}
    >
      <Text color={TEXT_SECONDARY} bold>
        ⌨ Commands
      </Text>
      <Box marginTop={1} flexDirection="column">
        {commands.map((c) => (
          <Box key={c.cmd}>
            <Box width={22}>
              <Text color={TEXT_PRIMARY} bold>{c.cmd}</Text>
            </Box>
            <Text color={TEXT_DIM}>{c.desc}</Text>
          </Box>
        ))}
      </Box>
      <Box marginTop={1}>
        <Text color={TEXT_DIM}>esc close</Text>
      </Box>
    </Box>
  );
}
