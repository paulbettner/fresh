export type RemoteInputError =
  | "ssh-host-required"
  | "invalid-ssh-target"
  | "invalid-ssh-port"
  | "ssh-path-conflict"
  | "unmatched-quote"
  | "dangling-escape"
  | "conflicting-ssh-option"
  | "missing-ssh-option-value"
  | "unexpected-ssh-positional";

export type RemoteInputResult<T> =
  | { ok: true; value: T }
  | { ok: false; error: RemoteInputError };

export interface ParsedSshInput {
  user?: string;
  host: string;
  port: number | null;
  remotePath: string | null;
}

function parsePort(value: string): RemoteInputResult<number> {
  if (!/^\d+$/.test(value)) return { ok: false, error: "invalid-ssh-port" };
  const port = Number(value);
  return Number.isSafeInteger(port) && port > 0 && port <= 65535
    ? { ok: true, value: port }
    : { ok: false, error: "invalid-ssh-port" };
}

function parseAuthority(
  authority: string,
): RemoteInputResult<Omit<ParsedSshInput, "remotePath">> {
  const at = authority.indexOf("@");
  let user: string | undefined;
  let hostPort = authority;
  if (at >= 0) {
    if (at === 0 || at !== authority.lastIndexOf("@")) {
      return { ok: false, error: "invalid-ssh-target" };
    }
    user = authority.slice(0, at);
    hostPort = authority.slice(at + 1);
    if (/\s/.test(user) || /[/?#\[\]]/.test(user)) {
      return { ok: false, error: "invalid-ssh-target" };
    }
  }

  let host: string;
  let port: number | null = null;
  if (hostPort.startsWith("[")) {
    const close = hostPort.indexOf("]");
    if (close <= 1) return { ok: false, error: "invalid-ssh-target" };
    host = hostPort.slice(1, close);
    const suffix = hostPort.slice(close + 1);
    if (suffix !== "") {
      if (!suffix.startsWith(":")) {
        return { ok: false, error: "invalid-ssh-target" };
      }
      const parsed = parsePort(suffix.slice(1));
      if (!parsed.ok) return parsed;
      port = parsed.value;
    }
  } else {
    const colons = [...hostPort].filter((char) => char === ":").length;
    if (colons > 1 || hostPort.includes("[") || hostPort.includes("]")) {
      return { ok: false, error: "invalid-ssh-target" };
    }
    const colon = hostPort.indexOf(":");
    host = colon < 0 ? hostPort : hostPort.slice(0, colon);
    if (colon >= 0) {
      const parsed = parsePort(hostPort.slice(colon + 1));
      if (!parsed.ok) return parsed;
      port = parsed.value;
    }
  }

  if (!host || /\s/.test(host) || /[/?#\[\]]/.test(host)) {
    return { ok: false, error: "invalid-ssh-target" };
  }
  return { ok: true, value: { user, host, port } };
}

/** Parse the structured SSH target and reconcile its URL path with the path field. */
export function parseSshInput(
  hostInput: string,
  explicitPath: string,
): RemoteInputResult<ParsedSshInput> {
  const input = hostInput.trim();
  if (!input) return { ok: false, error: "ssh-host-required" };

  let authority = input;
  let urlPath: string | null = null;
  if (input.startsWith("ssh://")) {
    const rest = input.slice("ssh://".length);
    const slash = rest.indexOf("/");
    authority = slash < 0 ? rest : rest.slice(0, slash);
    urlPath = slash < 0 ? null : rest.slice(slash);
  } else if (input.includes("/")) {
    return { ok: false, error: "invalid-ssh-target" };
  }

  const parsed = parseAuthority(authority);
  if (!parsed.ok) return parsed;
  const fieldPath = explicitPath.trim() || null;
  if (urlPath !== null && fieldPath !== null && urlPath !== fieldPath) {
    return { ok: false, error: "ssh-path-conflict" };
  }
  return {
    ok: true,
    value: { ...parsed.value, remotePath: urlPath ?? fieldPath },
  };
}

function tokenizeSshArgs(input: string): RemoteInputResult<string[]> {
  const tokens: string[] = [];
  let token = "";
  let tokenStarted = false;
  let quote: "'" | '"' | null = null;

  for (let i = 0; i < input.length; i += 1) {
    const char = input[i];
    if (quote !== "'" && char === "\\") {
      if (i + 1 >= input.length) return { ok: false, error: "dangling-escape" };
      token += input[++i];
      tokenStarted = true;
    } else if (char === "'" || char === '"') {
      if (quote === null) {
        quote = char;
        tokenStarted = true;
      } else if (quote === char) {
        quote = null;
      } else {
        token += char;
        tokenStarted = true;
      }
    } else if (quote === null && /\s/.test(char)) {
      if (tokenStarted) {
        tokens.push(token);
        token = "";
        tokenStarted = false;
      }
    } else {
      token += char;
      tokenStarted = true;
    }
  }
  if (quote !== null) return { ok: false, error: "unmatched-quote" };
  if (tokenStarted) tokens.push(token);
  return { ok: true, value: tokens };
}

const OPTIONS_WITH_VALUES: Record<string, true> = {
  "-B": true,
  "-b": true,
  "-c": true,
  "-D": true,
  "-E": true,
  "-e": true,
  "-F": true,
  "-I": true,
  "-J": true,
  "-L": true,
  "-m": true,
  "-O": true,
  "-o": true,
  "-Q": true,
  "-R": true,
  "-S": true,
  "-W": true,
  "-w": true,
};
const OPTIONS_WITH_VALUES_KEYS = Object.keys(OPTIONS_WITH_VALUES);
const CONFLICTING_OPENSSH_KEYS: Record<string, true> = {
  hostname: true,
  identityfile: true,
  port: true,
  user: true,
};

/** Split SSH options without invoking a shell and reject options that override structured fields. */
export function parseSshExtraArgs(input: string): RemoteInputResult<string[]> {
  const parsed = tokenizeSshArgs(input);
  if (!parsed.ok) return parsed;
  const args = parsed.value;

  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (!arg.startsWith("-") || arg === "-") {
      return { ok: false, error: "unexpected-ssh-positional" };
    }
    if (arg === "--") return { ok: false, error: "conflicting-ssh-option" };
    if (
      ["-p", "-l", "-i"].some((option) =>
        arg === option || (arg.startsWith(option) && arg.length > option.length)
      )
    ) {
      return { ok: false, error: "conflicting-ssh-option" };
    }

    const option = OPTIONS_WITH_VALUES_KEYS.find((candidate) =>
      arg === candidate ||
      (arg.startsWith(candidate) && arg.length > candidate.length)
    );
    if (!option) continue;
    const value = arg === option ? args[++i] : arg.slice(option.length);
    if (value === undefined || value === "") {
      return { ok: false, error: "missing-ssh-option-value" };
    }
    if (option === "-o") {
      const key = value.trim().split(/[=\s]/, 1)[0].toLowerCase();
      if (CONFLICTING_OPENSSH_KEYS[key]) {
        return { ok: false, error: "conflicting-ssh-option" };
      }
    }
  }
  return { ok: true, value: args };
}
