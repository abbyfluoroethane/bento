import { useState } from "react";
import { setTheme, storedTheme, type Theme } from "../theme";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "./ui/dropdown-menu";
import { Button } from "./ui/button";

const labels: Record<Theme, string> = {
  system: "System",
  light: "Latte",
  dark: "Mocha",
};

// The manual theme override of SPEC 14.2, persisted in localStorage.
export function ThemeToggle() {
  const [theme, setThemeState] = useState<Theme>(storedTheme());
  const pick = (t: Theme) => {
    setTheme(t);
    setThemeState(t);
  };
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="sm" aria-label="Color theme">
          Theme: {labels[theme]}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent>
        {(Object.keys(labels) as Theme[]).map((t) => (
          <DropdownMenuItem key={t} onSelect={() => pick(t)}>
            {labels[t]}
            {theme === t && <span className="ml-auto pl-4 text-accent">•</span>}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
