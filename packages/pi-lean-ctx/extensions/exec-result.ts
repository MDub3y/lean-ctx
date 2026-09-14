// Exit-code and environment handling shared by the CLI-backed tools.
//
// Kept free of Pi imports so the rules are unit-testable on their own: the
// grep/find exit convention (#1499, #1762) and the env-key policy that the
// CLI's `ctx_shell` enforces on its `env` parameter (#1761) both live here.

export interface ExecExit {
  code: number;
  stdout: string;
  stderr: string;
}

export type SearchExit =
  | { kind: "matches"; stdout: string }
  | { kind: "empty" }
  | { kind: "error"; message: string };

/**
 * Classify the exit of a search-style lean-ctx command (`find`, `-c rg …`).
 *
 * Both follow the grep convention: exit 1 with nothing on stderr is a
 * *completed* search with zero matches, not a failure. `ctx_find` used to
 * route every nonzero exit through a generic "lean-ctx failed" error, so a
 * filename with no visible match read as a broken tool (#1762). Exit 1 *with*
 * stderr is a real failure — e.g. "rg not recognized" — and stays one (#1499).
 */
export function classifySearchExit(result: ExecExit, fallback: string): SearchExit {
  if (result.code === 0) return { kind: "matches", stdout: result.stdout };
  const stderr = (result.stderr ?? "").trim();
  if (result.code === 1) {
    return stderr.length > 0 ? { kind: "error", message: stderr } : { kind: "empty" };
  }
  const message = (stderr || result.stdout || fallback).trim();
  return { kind: "error", message };
}

// Mirror of `is_dangerous_env_key` in rust/src/tools/registered/ctx_shell.rs:
// the keys the MCP `env` parameter silently drops. The Pi tool must apply the
// same policy, or the `env` recovery the CLI recommends (#1761) would be a
// wider door here than there.
const BLOCKED_ENV_KEYS = new Set([
  // Dynamic linker injection
  "LD_PRELOAD",
  "LD_LIBRARY_PATH",
  "DYLD_INSERT_LIBRARIES",
  "DYLD_LIBRARY_PATH",
  "DYLD_FRAMEWORK_PATH",
  // Shell re-entry / startup injection
  "BASH_ENV",
  "ENV",
  "PROMPT_COMMAND",
  "SHELL",
  "IFS",
  "CDPATH",
  // Binary resolution hijacking
  "PATH",
  "GIT_EXEC_PATH",
  "GIT_SSH",
  "GIT_SSH_COMMAND",
  // Identity / home directory manipulation
  "HOME",
  "USER",
  "LOGNAME",
  "XDG_CONFIG_HOME",
  "XDG_DATA_HOME",
  "XDG_STATE_HOME",
  "XDG_CACHE_HOME",
  // Language runtime search path hijacking
  "PYTHONPATH",
  "PYTHONSTARTUP",
  "PYTHONHOME",
  "NODE_PATH",
  "NODE_OPTIONS",
  "RUBYOPT",
  "RUBYLIB",
  "GEM_PATH",
  "GEM_HOME",
  "PERL5LIB",
  "PERL5OPT",
  "CLASSPATH",
  "JAVA_HOME",
  "CARGO_HOME",
  "RUSTUP_HOME",
  "GOPATH",
  "GOROOT",
]);

export function isDangerousEnvKey(key: string): boolean {
  const upper = key.toUpperCase();
  if (BLOCKED_ENV_KEYS.has(upper)) return true;
  if (upper.startsWith("LD_") && upper.endsWith("_PATH")) return true;
  // lean-ctx config overrides never come from a tool call.
  if (upper.startsWith("LEAN_CTX_") || upper.startsWith("LCTX_")) return true;
  return false;
}

export interface SanitizedEnv {
  accepted: Record<string, string>;
  rejected: string[];
}

/**
 * Split a caller-supplied `env` object into the variables the subprocess may
 * see and the keys that were dropped (protected, or not a string value).
 */
export function sanitizeExtraEnv(extra: unknown): SanitizedEnv {
  const accepted: Record<string, string> = {};
  const rejected: string[] = [];
  if (!extra || typeof extra !== "object" || Array.isArray(extra)) {
    return { accepted, rejected };
  }
  for (const [key, value] of Object.entries(extra as Record<string, unknown>)) {
    if (typeof value !== "string" || isDangerousEnvKey(key)) {
      rejected.push(key);
      continue;
    }
    accepted[key] = value;
  }
  return { accepted, rejected };
}
