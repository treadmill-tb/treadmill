import { Moon, Sun, SunMoon, type LucideIcon } from "lucide-react";
import { useSyncExternalStore } from "react";

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

const ICONS: Record<Theme, LucideIcon> = {
  light: Sun,
  auto: SunMoon,
  dark: Moon,
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
      {THEMES.map((t) => {
        const Icon = ICONS[t];
        return (
          <label key={t} title={LABELS[t]}>
            <input
              type="radio"
              name="theme"
              value={t}
              aria-label={LABELS[t]}
              checked={theme === t}
              onChange={() => applyTheme(t)}
            />
            <Icon size={16} aria-hidden="true" />
          </label>
        );
      })}
    </div>
  );
}
