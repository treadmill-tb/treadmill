/**
 * A non-2xx switchboard response, carrying its status and parsed body.
 *
 * Most switchboard errors are a bare status code with no body. `openapi-fetch`
 * reports those as `error: undefined`, which `openapi-react-query` takes for
 * success, so `$api` throws this instead (see `client.ts`) and every failed
 * query or mutation ends up here.
 */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    /** The JSON-parsed body, the raw text if it isn't JSON, or `undefined`
     * when there is none. */
    readonly body: unknown,
  ) {
    super(`HTTP ${status}`);
    this.name = "ApiError";
  }

  static async from(response: Response): Promise<ApiError> {
    let text = "";
    try {
      text = await response.clone().text();
    } catch {
      // An unreadable body is as good as none.
    }
    if (text === "") return new ApiError(response.status, undefined);
    try {
      return new ApiError(response.status, JSON.parse(text));
    } catch {
      return new ApiError(response.status, text);
    }
  }
}

/** Per-call wording for statuses whose meaning depends on the endpoint. */
export type ErrorMessages = Partial<Record<number, string>>;

/** What a status means when the endpoint gives it no more specific meaning. */
const GENERIC: Record<number, string> = {
  401: "Your session has expired. Sign in again to continue.",
  403: "You don't have permission to do this.",
  404: "It no longer exists.",
  409: "This conflicts with the current state. Reload and try again.",
  429: "Too many requests. Wait a moment and try again.",
  503: "This feature is not enabled on this switchboard.",
};

/** The request bodies the switchboard's framework rejects with a plain-text
 * reason (malformed JSON, wrong content type, a field that fails to parse). */
const REJECTED_REQUEST = new Set([400, 415, 422]);

/**
 * A sentence for the user describing `error`: the call's own wording for its
 * status if given, else what the status generally means, else the body as the
 * switchboard sent it (unknown errors stay visible rather than vanishing).
 */
export function describeError(
  error: unknown,
  messages: ErrorMessages = {},
): string {
  if (error instanceof ApiError) {
    const specific = messages[error.status];
    if (specific !== undefined) return specific;
    const body = typeof error.body === "string" ? error.body.trim() : null;
    if (REJECTED_REQUEST.has(error.status) && body) {
      return `The switchboard rejected the request: ${body}`;
    }
    const generic = GENERIC[error.status];
    if (generic !== undefined) return generic;
    if (error.status >= 500) {
      return `The switchboard failed to handle the request (HTTP ${error.status}).`;
    }
    if (body) return body;
    if (error.body !== undefined) return JSON.stringify(error.body);
    return `The request failed (HTTP ${error.status}).`;
  }
  // `fetch` rejects with a TypeError when the request never got an answer.
  if (error instanceof TypeError) return "Could not reach the switchboard.";
  if (error instanceof Error) return error.message;
  if (typeof error === "string" && error !== "") return error;
  return "The request failed.";
}
