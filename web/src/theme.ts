// Theme handling (SPEC 14.2): follow the OS preference on first load,
// offer a manual override, store the override in localStorage.

export type Theme = "system" | "light" | "dark";

const KEY = "bento-theme";

export function storedTheme(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark") return v;
  } catch {
    // localStorage unavailable; fall through to system.
  }
  return "system";
}

export function applyTheme(theme: Theme): void {
  const dark =
    theme === "dark" ||
    (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
}

export function setTheme(theme: Theme): void {
  try {
    if (theme === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, theme);
  } catch {
    // Ignore; the class still toggles for this page load.
  }
  applyTheme(theme);
}

// Keep "system" live when the OS preference changes.
export function watchSystemTheme(): void {
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (storedTheme() === "system") applyTheme("system");
  });
}
