// MCP 2024-11-05 compliant HTTP client for Weaver's Streamable HTTP transport.

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export type ToolDescriptor = {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
};

export type RepoSummary = {
  id: string;
  path: string;
  name?: string | null;
  ingested_at: string;
};

// --------------------------------------------------------------------------
// JSON-RPC wire types
// --------------------------------------------------------------------------

type RpcRequest = {
  jsonrpc: '2.0';
  id: number;
  method: string;
  params?: unknown;
};

type RpcNotification = {
  jsonrpc: '2.0';
  method: string;
  params?: unknown;
};

type RpcError = { code: number; message: string; data?: unknown };

type RpcResponse<T> = {
  jsonrpc: '2.0';
  id: number;
  result?: T;
  error?: RpcError;
};

// --------------------------------------------------------------------------
// Client
// --------------------------------------------------------------------------

export class McpHttpClient {
  /** Session ID assigned by the server after initialize. */
  private sessionId: string | null = null;
  private nextId = 1;
  private initialized = false;
  /** Keepalive timer — fires every 2 min to prevent the 5-min server timeout. */
  private keepaliveTimer: ReturnType<typeof setInterval> | null = null;

  constructor(private readonly endpoint: string) {}

  /**
   * Send MCP `initialize` then `notifications/initialized`.
   * Must be called once before any other method.
   */
  async initialize(clientName = 'weaver-manager'): Promise<void> {
    // Send initialize — the server assigns a session ID in the response headers.
    await this.request<unknown>('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: clientName, version: '0.1.0' },
    });

    // Required handshake notification (fire-and-forget, no response expected).
    await this.notify('notifications/initialized', {});

    this.initialized = true;

    // Start periodic pings every 2 minutes to keep the session alive.
    // rmcp kills sessions after 5 minutes of inactivity.
    this.keepaliveTimer = setInterval(() => void this.ping(), 2 * 60 * 1000);
  }

  /** List all MCP tools exposed by the server. */
  async listTools(): Promise<ToolDescriptor[]> {
    this.assertReady();
    const result = await this.request<{ tools?: ToolDescriptor[] }>('tools/list', {});
    return result.tools ?? [];
  }

  /** Invoke a tool by name with the given arguments. */
  async callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
    this.assertReady();
    return this.request<unknown>('tools/call', { name, arguments: args });
  }

  /**
   * Convenience: call `list_repos` and return the repos array.
   * The tool takes no arguments.
   */
  async listRepos(): Promise<RepoSummary[]> {
    const raw = await this.callTool('list_repos', {});
    const payload = extractToolPayload(raw);
    if (isRecord(payload) && Array.isArray(payload.repos)) {
      return payload.repos as RepoSummary[];
    }
    return [];
  }

  /**
   * Send DELETE to close the session (best-effort).
   */
  async disconnect(): Promise<void> {
    if (this.keepaliveTimer !== null) {
      clearInterval(this.keepaliveTimer);
      this.keepaliveTimer = null;
    }
    if (!this.sessionId) return;
    try {
      await fetch(this.endpoint, {
        method: 'DELETE',
        headers: { 'Mcp-Session-Id': this.sessionId },
      });
    } catch {
      // ignore — server may already be gone
    }
    this.sessionId = null;
    this.initialized = false;
  }

  get isReady(): boolean {
    return this.initialized;
  }

  /**
   * Send a MCP `ping` to keep the session alive.
   * Silently ignores errors (session may have expired or server restarted).
   */
  async ping(): Promise<void> {
    if (!this.sessionId) return;
    try {
      await this.request<unknown>('ping', {});
    } catch {
      // ignore — keepalive best-effort
    }
  }

  get currentSessionId(): string | null {
    return this.sessionId;
  }

  /**
   * Open a persistent GET SSE connection to receive server-initiated
   * notifications (MCP Streamable HTTP §3.2).  Returns a stop function.
   * Silently no-ops if the server doesn't support the GET listener (non-2xx).
   * Reconnects automatically with exponential backoff until stopped.
   */
  listen(handler: (method: string, params: unknown) => void): () => void {
    let active = true;
    let retryMs = 1000;

    const run = async () => {
      while (active) {
        if (!this.sessionId) {
          await delay(500);
          continue;
        }
        try {
          const res = await fetch(this.endpoint, {
            method: 'GET',
            headers: {
              Accept: 'text/event-stream',
              'Mcp-Session-Id': this.sessionId,
            },
          });
          if (!res.ok || !res.body) return; // server doesn't support GET SSE
          retryMs = 1000;
          const reader = res.body.getReader();
          const dec = new TextDecoder();
          let buf = '';
          while (active) {
            const { done, value } = await reader.read();
            if (done) break;
            buf += dec.decode(value, { stream: true });
            let nl: number;
            while ((nl = buf.indexOf('\n')) !== -1) {
              const line = buf.slice(0, nl).trimEnd();
              buf = buf.slice(nl + 1);
              if (!line.startsWith('data:')) continue;
              const text = line.slice(5).trim();
              if (!text || text === '[DONE]') continue;
              try {
                const msg = JSON.parse(text) as { method?: string; params?: unknown };
                if (msg.method) handler(msg.method, msg.params ?? {});
              } catch { /* ignore malformed frames */ }
            }
          }
          reader.cancel();
        } catch {
          if (!active) return;
          await delay(retryMs);
          retryMs = Math.min(retryMs * 2, 30_000);
        }
      }
    };

    void run();
    return () => { active = false; };
  }

  // ------------------------------------------------------------------------
  // Internal helpers
  // ------------------------------------------------------------------------

  private assertReady() {
    if (!this.initialized) {
      throw new Error('McpHttpClient: call initialize() before using the client.');
    }
  }

  private async request<T>(method: string, params: unknown): Promise<T> {
    const body: RpcRequest = { jsonrpc: '2.0', id: this.nextId++, method, params };

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      Accept: 'application/json, text/event-stream',
    };
    if (this.sessionId) {
      headers['Mcp-Session-Id'] = this.sessionId;
    }

    const response = await fetch(this.endpoint, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });

    if (!response.ok && response.status !== 200) {
      throw new Error(`MCP HTTP ${response.status}: ${response.statusText}`);
    }

    // Server sets the canonical session ID on the first initialize response.
    const serverSession = response.headers.get('mcp-session-id');
    if (serverSession) {
      this.sessionId = serverSession;
    }

    const text = await response.text();
    const envelope = parseSseOrJson<T>(text);

    if (envelope.error) {
      throw new Error(`MCP error ${envelope.error.code}: ${envelope.error.message}`);
    }
    if (envelope.result === undefined) {
      throw new Error(`MCP: no result for '${method}'`);
    }
    return envelope.result;
  }

  /**
   * Send a JSON-RPC notification (no `id`, no response expected).
   * Uses 202 Accepted from the server; we ignore the body.
   */
  private async notify(method: string, params: unknown): Promise<void> {
    const body: RpcNotification = { jsonrpc: '2.0', method, params };

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      Accept: 'application/json, text/event-stream',
    };
    if (this.sessionId) {
      headers['Mcp-Session-Id'] = this.sessionId;
    }

    await fetch(this.endpoint, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    // Notifications: ignore response body entirely.
  }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/**
 * Parse a Streamable HTTP response body which may be either plain JSON
 * or SSE-framed (`data: {...}\n\n`).
 */
function parseSseOrJson<T>(text: string): RpcResponse<T> {
  const trimmed = text.trim();
  if (!trimmed) {
    throw new Error('Empty MCP response');
  }

  if (trimmed.startsWith('{')) {
    return JSON.parse(trimmed) as RpcResponse<T>;
  }

  // SSE framing: extract last `data:` line
  const dataLines = trimmed
    .split(/\r?\n/)
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .filter(Boolean);

  if (dataLines.length === 0) {
    throw new Error('Unsupported MCP response format (no data: lines in SSE)');
  }

  return JSON.parse(dataLines[dataLines.length - 1]!) as RpcResponse<T>;
}

/**
 * Unwrap the `content` array from a tool call result to the first useful value.
 * Handles `{ json }`, `{ data }`, and `{ text }` content items.
 */
export function extractToolPayload(result: unknown): unknown {
  if (!isRecord(result)) return result;

  const content = result.content;
  if (!Array.isArray(content)) return result;

  for (const item of content) {
    if (!isRecord(item)) continue;
    if ('json' in item) return item.json;
    if ('data' in item) return item.data;
    if (typeof item.text === 'string') {
      const text = item.text.trim();
      if (text.startsWith('{') || text.startsWith('[')) {
        try { return JSON.parse(text); } catch { /* fall through */ }
      }
      return item.text;
    }
  }

  return result;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
