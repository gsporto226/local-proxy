//! A throwaway upstream that behaves like an OpenAI chat-completions server AND
//! an Anthropic messages server, emitting canned JSON/SSE. Used by mock.test.ts.

export interface MockUpstream {
  port: number;
  stop: () => void;
  /** Headers seen on the most recent `/responses` request. */
  lastResponsesHeaders: () => Record<string, string>;
  /** Body seen on the most recent `/responses` request. */
  lastResponsesBody: () => any;
  /** How many token refreshes `/token` has served. */
  tokenHits: () => number;
}

function sse(text: string): Response {
  return new Response(text, {
    headers: { "content-type": "text/event-stream" },
  });
}

function chatHandler(body: any): Response {
  if (body?.model === "gpt-error") {
    return Response.json(
      {
        error: {
          message: "bad key",
          type: "authentication_error",
          code: "invalid_api_key",
        },
      },
      { status: 401 },
    );
  }
  if (body?.stream) {
    const chunks = [
      { id: "cmpl_1", object: "chat.completion.chunk", created: 1, model: "gpt-4o", choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] },
      { id: "cmpl_1", object: "chat.completion.chunk", created: 1, model: "gpt-4o", choices: [{ index: 0, delta: { content: "Hel" }, finish_reason: null }] },
      { id: "cmpl_1", object: "chat.completion.chunk", created: 1, model: "gpt-4o", choices: [{ index: 0, delta: { content: "lo" }, finish_reason: null }] },
      { id: "cmpl_1", object: "chat.completion.chunk", created: 1, model: "gpt-4o", choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 } },
    ];
    const text = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";
    return sse(text);
  }
  return Response.json({
    id: "cmpl_1",
    object: "chat.completion",
    created: 1,
    model: "gpt-4o",
    choices: [
      {
        index: 0,
        message: { role: "assistant", content: "hi" },
        finish_reason: "stop",
        logprobs: null,
      },
    ],
    // `cost` reported by cost-aware OpenAI-compatible upstreams (OpenRouter,
    // Groq, …); the proxy records it verbatim as the session cost.
    usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5, cost: 0.0042 },
  });
}

function messagesHandler(body: any): Response {
  if (body?.stream) {
    const events = [
      { type: "message_start", message: { id: "msg_1", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 2, output_tokens: 0 } } },
      { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
      { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "oi" } },
      { type: "content_block_stop", index: 0 },
      { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 2 } },
      { type: "message_stop" },
    ];
    const text = events
      .map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`)
      .join("");
    return sse(text);
  }
  return Response.json({
    id: "msg_1",
    type: "message",
    role: "assistant",
    model: "claude-sonnet-4-5",
    content: [{ type: "text", text: "hi" }],
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 2, output_tokens: 2 },
  });
}

function responsesHandler(body: any): Response {
  if (body?.stream) {
    const events = [
      { type: "response.created", response: { id: "resp_1", object: "response", created_at: 1, status: "in_progress", model: "gpt-6.1-sol-medium", output: [] } },
      { type: "response.output_item.added", output_index: 0, item: { type: "message", id: "msg_1", status: "in_progress", role: "assistant", content: [] } },
      { type: "response.output_text.delta", item_id: "msg_1", output_index: 0, content_index: 0, delta: "Ol" },
      { type: "response.output_text.delta", item_id: "msg_1", output_index: 0, content_index: 0, delta: "a" },
      { type: "response.completed", response: { id: "resp_1", object: "response", created_at: 1, status: "completed", model: "gpt-6.1-sol-medium", output: [], usage: { input_tokens: 4, output_tokens: 2, total_tokens: 6 } } },
    ];
    const text = events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
    return sse(text);
  }
  return Response.json({
    id: "resp_1",
    object: "response",
    created_at: 1,
    status: "completed",
    model: "gpt-6.1-sol-medium",
    output: [
      {
        type: "message",
        id: "msg_1",
        status: "completed",
        role: "assistant",
        content: [{ type: "output_text", text: "hi", annotations: [] }],
      },
    ],
    usage: { input_tokens: 4, output_tokens: 2, total_tokens: 6 },
  });
}

export async function startMockUpstream(): Promise<MockUpstream> {
  let lastHeaders: Record<string, string> = {};
  let lastBody: any = {};
  let tokenHits = 0;
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url);
      let body: any = {};
      try {
        body = JSON.parse(await req.text());
      } catch {
        // empty/invalid body
      }
      if (url.pathname === "/v1/chat/completions") return chatHandler(body);
      if (url.pathname === "/v1/messages") return messagesHandler(body);
      if (url.pathname === "/responses") {
        lastHeaders = Object.fromEntries(req.headers.entries());
        lastBody = body;
        return responsesHandler(body);
      }
      if (url.pathname === "/token") {
        // OAuth refresh endpoint, used by the oauth-seeded provider test.
        tokenHits += 1;
        return Response.json({
          access_token: "refreshed-token",
          refresh_token: "refresh-2",
          expires_in: 3600,
        });
      }
      return new Response("not found", { status: 404 });
    },
  });
  return {
    port: server.port,
    stop: () => server.stop(true),
    lastResponsesHeaders: () => lastHeaders,
    lastResponsesBody: () => lastBody,
    tokenHits: () => tokenHits,
  };
}

// ---------------------------------------------------------------------------
// config builders
// ---------------------------------------------------------------------------

export function yamlList(values: string[]): string {
  return values.map((v) => `      - ${v}`).join("\n");
}

export function mockConfig(
  mockBase: string,
  opts: { apiKeys?: string[]; activeModel?: string; autoModel?: string } = {},
): string {
  const apiKeys =
    opts.apiKeys && opts.apiKeys.length
      ? `  api_keys:\n${opts.apiKeys.map((k) => `    - ${k}`).join("\n")}`
      : "  api_keys: []";
  // Providers get keys only from auth.json (see mock.test.ts, which seeds
  // `mock_openai`/`mock_anthropic` there so the proxy sees them as connected).
  const active = opts.activeModel
    ? `  active_model: ${opts.activeModel}\n`
    : "";
  const auto = opts.autoModel ? `    auto_model: ${opts.autoModel}\n` : "";
  return `
server:
  host: 127.0.0.1
  port: 0
${apiKeys}
  passthrough_keys: false

providers:
  - name: mock_openai
    base_url: ${mockBase}
    format: openai
${auto}  - name: mock_anthropic
    base_url: ${mockBase}
    format: anthropic
  - name: mock_responses
    base_url: ${mockBase}
    format: openai-responses
  - name: mock_oauth
    base_url: ${mockBase}
    format: openai-responses
    oauth:
      token_url: ${mockBase}/token
      token_encoding: form
      account_id_header: chatgpt-account-id

routes:
  - model: claude-via-openai
    provider: mock_openai
    upstream_model: gpt-4o
  - model: gpt-via-anthropic
    provider: mock_anthropic
    upstream_model: claude-sonnet-4-5
  - model: err
    provider: mock_openai
    upstream_model: gpt-error
  - model: claude-via-responses
    provider: mock_responses
    upstream_model: gpt-6.1-sol-medium
  - model: gpt-via-responses
    provider: mock_responses
    upstream_model: gpt-6.1-sol-medium
  - model: resp-native
    provider: mock_responses
    upstream_model: gpt-6.1-sol-medium
  - model: oauth-via-responses
    provider: mock_oauth
    upstream_model: gpt-6.1-sol-medium

defaults:
  provider: ""
${active}`;
}
