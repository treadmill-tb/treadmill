import createFetchClient from "openapi-fetch";
import createClient from "openapi-react-query";

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

export const client = createFetchClient<paths>({
  baseUrl: `${API_ORIGIN}/api/v1`,
});

client.use({
  onRequest({ request }) {
    const token = getToken();
    if (token !== null) {
      request.headers.set("Authorization", `Bearer ${token}`);
    }
    return request;
  },
  onResponse({ response }) {
    // Expired/revoked session: drop the token and start over at the login
    // page. The login flow itself never sees a 401 (its routes are
    // unauthenticated).
    if (response.status === 401 && getToken() !== null) {
      clearToken();
      window.location.assign("/login");
    }
    return response;
  },
});

export const $api = createClient(client);
