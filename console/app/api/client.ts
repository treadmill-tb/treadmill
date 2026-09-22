import createFetchClient from "openapi-fetch";
import createClient from "openapi-react-query";

import { ApiError } from "./errors";
import type { paths } from "./schema";

/** Switchboard origin ("" = same-origin); login_path values from
 * `/auth/providers` are relative to this. */
export const API_ORIGIN = import.meta.env.VITE_TML_API_URL ?? "";

const TOKEN_KEY = "tml_token";

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

function authorize(request: Request): Request {
  const token = getToken();
  if (token !== null) {
    request.headers.set("Authorization", `Bearer ${token}`);
  }
  return request;
}

/** An expired or revoked session (a 401) drops the token and starts over at
 * the login page. The login flow itself never sees a 401 (its routes are
 * unauthenticated). */
function checkSession(response: Response): void {
  if (response.status === 401 && getToken() !== null) {
    clearToken();
    window.location.assign("/login");
  }
}

/** For direct calls that branch on `response.status` themselves. */
export const client = createFetchClient<paths>({
  baseUrl: `${API_ORIGIN}/api/v1`,
});
client.use({
  onRequest: ({ request }) => authorize(request),
  onResponse({ response }) {
    checkSession(response);
    return response;
  },
});

/** Behind `$api`: every non-2xx response throws an `ApiError`. Left to
 * `openapi-fetch`, an error without a body (most of the switchboard's) comes
 * back as `error: undefined`, which `openapi-react-query` takes for success. */
const throwingClient = createFetchClient<paths>({
  baseUrl: `${API_ORIGIN}/api/v1`,
});
throwingClient.use({
  onRequest: ({ request }) => authorize(request),
  async onResponse({ response }) {
    checkSession(response);
    if (!response.ok) {
      throw await ApiError.from(response);
    }
    return response;
  },
});

export const $api = createClient(throwingClient);
