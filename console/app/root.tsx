import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { Links, Meta, Outlet, Scripts, ScrollRestoration } from "react-router";

import { THEME_SCRIPT, ThemeSwitch } from "./components/theme-switch";

import "@picocss/pico/css/pico.blue.min.css";
import "./app.css";

export function Layout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <meta charSet="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>Treadmill</title>
        <script dangerouslySetInnerHTML={{ __html: THEME_SCRIPT }} />
        <Meta />
        <Links />
      </head>
      <body>
        {children}
        <Footer />
        <ScrollRestoration />
        <Scripts />
      </body>
    </html>
  );
}

function Footer() {
  return (
    <footer className="container muted">
      <ThemeSwitch />
      <span>
        The Treadmill Distributed Hardware Testbed — Console Version{" "}
        <span className="mono">
          {import.meta.env.VITE_TML_CONSOLE_REV ?? "unknown"}
        </span>
      </span>
    </footer>
  );
}

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 401s log the session out via the client middleware; retrying other
      // errors mostly delays the error state the user should see.
      retry: false,
      staleTime: 5_000,
    },
  },
});

// Prerendered into index.html (SPA mode), shown until the JS modules have
// loaded and the app has hydrated.
export function HydrateFallback() {
  return (
    <main className="container loading-page">
      <p aria-busy="true">Loading Treadmill Console…</p>
    </main>
  );
}

export default function Root() {
  return (
    <QueryClientProvider client={queryClient}>
      <Outlet />
    </QueryClientProvider>
  );
}
