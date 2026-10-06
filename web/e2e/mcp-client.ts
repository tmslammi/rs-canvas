import { expect, type APIRequestContext } from "@playwright/test";

// A tiny MCP Streamable HTTP client for e2e tests: the same endpoint opencode
// uses, driven directly.

export interface ToolResult {
  isError?: boolean;
  content?: {
    type: string;
    text?: string;
    data?: string;
    mimeType?: string;
  }[];
}

export interface McpClient {
  /** Call a tool; fails the test if the tool reports an error. */
  call(name: string, args: object): Promise<ToolResult>;
  /** Call a tool and return its text content. */
  text(name: string, args: object): Promise<string>;
}

export function mcpClient(request: APIRequestContext, base: string): McpClient {
  let session: string | undefined;

  const post = async (body: object) => {
    const response = await request.post(`${base}/mcp`, {
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json, text/event-stream",
        ...(session ? { "Mcp-Session-Id": session } : {}),
      },
      data: body,
    });
    expect(response.ok()).toBe(true);
    return response;
  };

  const call = async (name: string, args: object): Promise<ToolResult> => {
    if (!session) {
      const initialized = await post({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-11-25",
          capabilities: {},
          clientInfo: { name: "playwright", version: "0" },
        },
      });
      session = initialized.headers()["mcp-session-id"];
      expect(session).toBeTruthy();
      await post({ jsonrpc: "2.0", method: "notifications/initialized" });
    }
    const response = await post({
      jsonrpc: "2.0",
      id: 2,
      method: "tools/call",
      params: { name, arguments: args },
    });
    const frame = parseSse(await response.text());
    expect(frame.result?.isError, JSON.stringify(frame.result)).not.toBe(true);
    return frame.result ?? {};
  };

  const text = async (name: string, args: object) => {
    const result = await call(name, args);
    return result.content?.find((block) => block.type === "text")?.text ?? "";
  };

  return { call, text };
}

/** Parse a tool result's text as JSON. */
export function toolJson(result: ToolResult): Record<string, unknown> {
  const text = result.content?.find((block) => block.type === "text")?.text;
  expect(text, "tool returned no text").toBeTruthy();
  return JSON.parse(text!);
}

/** The element count from a `canvas_read` scene graph. */
export function elementCount(described: string): number {
  const match = described.match(/(\d+) elements/);
  expect(match, `unexpected scene graph: ${described}`).toBeTruthy();
  return Number(match![1]);
}

function parseSse(body: string): { result?: ToolResult } {
  const data = body
    .split("\n")
    .filter((line) => line.startsWith("data: "))
    .map((line) => line.slice("data: ".length))
    .filter((line) => line.trim().length > 0);
  expect(data.length, "MCP server sent no data frame").toBeGreaterThan(0);
  return JSON.parse(data[data.length - 1]);
}
