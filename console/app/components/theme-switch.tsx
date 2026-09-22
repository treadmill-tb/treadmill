import { useSyncExternalStore, type ReactNode } from "react";

const THEME_KEY = "tml_theme";
const THEMES = ["light", "auto", "dark"] as const;
type Theme = (typeof THEMES)[number];

export const THEME_SCRIPT = `try{const t=localStorage.getItem("${THEME_KEY}");if(t==="light"||t==="dark")document.documentElement.dataset.theme=t}catch{}`;

function currentTheme(): Theme {
  return (
    THEMES.find((t) => t === document.documentElement.dataset.theme) ?? "auto"
  );
}

function subscribe(onChange: () => void) {
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
  return () => observer.disconnect();
}

function applyTheme(theme: Theme) {
  if (theme === "auto") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = theme;
  }
  try {
    if (theme === "auto") {
      localStorage.removeItem(THEME_KEY);
    } else {
      localStorage.setItem(THEME_KEY, theme);
    }
  } catch {
    // Without storage the choice lasts until the page is reloaded.
  }
}

function Icon({ children }: { children: ReactNode }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="16"
      height="16"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

const ICONS: Record<Theme, ReactNode> = {
  light: (
    <Icon>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
    </Icon>
  ),
  auto: (
    <Icon>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 3a9 9 0 0 0 0 18z" fill="currentColor" />
    </Icon>
  ),
  dark: (
    <Icon>
      <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
    </Icon>
  ),
};

const LABELS: Record<Theme, string> = {
  light: "Light theme",
  auto: "Theme from browser settings",
  dark: "Dark theme",
};

export function ThemeSwitch() {
  const theme = useSyncExternalStore(subscribe, currentTheme, () => "auto");
  return (
    <div className="theme-switch" role="radiogroup" aria-label="Theme">
      {THEMES.map((t) => (
        <label key={t} title={LABELS[t]}>
          <input
            type="radio"
            name="theme"
            value={t}
            aria-label={LABELS[t]}
            checked={theme === t}
            onChange={() => applyTheme(t)}
          />
          {ICONS[t]}
        </label>
      ))}
    </div>
  );
}
