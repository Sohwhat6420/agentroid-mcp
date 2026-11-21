// Typed errors for the MCP server, in two families.
//
// 1. Tool-result errors. A tool call never throws to the client: a failure comes back as a result
//    with `ok: false` and `error: {kind, code, message, data?}`. `kind` is one of ERROR_KINDS below,
//    a closed list that is part of the v1 contract: a new kind is a breaking change. ERROR_KIND_INFO
//    says what each kind means and whether repeating the same call can help; docs/REFERENCE.md
//    carries the same table and a test keeps the two in step.
//
// 2. Start-up and configuration errors. These are thrown before any tool runs (a bad HTTP config, a
//    bad token entry, a missing credstore helper). Each is an AgentroidError with a stable `code`,
//    so a caller can tell them apart without matching on message text. Messages never carry a token.
//    DaemonError (daemon.ts) stays separate: its `code` is the JSON-RPC number, and server-core
//    turns it into a tool-result kind.

export const ERROR_KINDS = ["invalid_input", "unknown_tool", "refused", "lease_held", "not_holder", "unknown_device", "ambiguous_device", "device_error", "daemon_error", "timeout", "transport", "bad_reply", "bad_output"] as const;
export type ErrorKind = (typeof ERROR_KINDS)[number];

/**
 * What repeating the same call can do: "no" (it will fail the same way), "after_fix" (only once the
 * input, the device choice or the lease is put right), "after_look" (maybe, but the action may
 * already have happened, so look at the screen first), "yes" (a passing fault on the link).
 */
export type Retry = "no" | "after_fix" | "after_look" | "yes";

export interface ErrorKindInfo {
  /** One plain sentence: what went wrong. */
  meaning: string;
  retry: Retry;
}

export const ERROR_KIND_INFO: Record<ErrorKind, ErrorKindInfo> = {
  invalid_input: { meaning: "The arguments do not match the tool's input schema, or the daemon rejected them as invalid.", retry: "after_fix" },
  unknown_tool: { meaning: "No tool has that name in this server's profile.", retry: "no" },
  refused: { meaning: "A safety check refused the call: the device mode, the screen guard, the token scope or the protected-device rule.", retry: "no" },
  lease_held: { meaning: "Another session holds the lease on this device.", retry: "after_fix" },
  not_holder: { meaning: "This session does not hold the lease the call needs (it lapsed, or the connection to bridged was replaced).", retry: "after_fix" },
  unknown_device: { meaning: "No connected, allowed device matches the name given.", retry: "after_fix" },
  ambiguous_device: { meaning: "Several devices are connected and the call named none, so the server will not choose.", retry: "after_fix" },
  device_error: { meaning: "The phone or adb failed while carrying out the call.", retry: "after_look" },
  daemon_error: { meaning: "bridged failed in a way that has no more specific kind.", retry: "after_look" },
  timeout: { meaning: "No answer within the call's time limit (never more than 110 s).", retry: "after_look" },
  transport: { meaning: "The MCP server cannot reach bridged over its named pipe.", retry: "yes" },
  bad_reply: { meaning: "bridged answered with something that is not a valid JSON-RPC reply.", retry: "no" },
  bad_output: { meaning: "The result failed the tool's own output schema, so it was replaced by this error.", retry: "no" },
};

export const isErrorKind = (v: unknown): v is ErrorKind => typeof v === "string" && (ERROR_KINDS as readonly string[]).includes(v);

/** The stable codes of thrown (start-up and configuration) errors. */
export const ERROR_CODES = ["config", "bind_refused", "token", "credstore"] as const;
export type ErrorCode = (typeof ERROR_CODES)[number];

/** Base class of every error this package throws on purpose. */
export class AgentroidError extends Error {
  readonly code: ErrorCode;
  constructor(code: ErrorCode, message: string) {
    super(message);
    this.name = "AgentroidError";
    this.code = code;
  }
}

/** A bad HTTP listener config (or a missing one). */
export class ConfigError extends AgentroidError {
  constructor(message: string, code: "config" | "bind_refused" = "config") {
    super(code, message);
    this.name = "ConfigError";
  }
}

/** The listener refuses an address or port, or the OS would not let it bind. A kind of ConfigError. */
export class BindRefused extends ConfigError {
  constructor(message: string) {
    super(message, "bind_refused");
    this.name = "BindRefused";
  }
}

/** A bad token entry, device serial or mode. The message names the entry, never the token. */
export class TokenError extends AgentroidError {
  constructor(message: string) {
    super("token", message);
    this.name = "TokenError";
  }
}

/** The credstore helper is missing, refused the request, or answered badly. */
export class CredstoreError extends AgentroidError {
  constructor(message: string) {
    super("credstore", message);
    this.name = "CredstoreError";
  }
}
