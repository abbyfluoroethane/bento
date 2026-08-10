// Shared loading / empty / error states (SPEC 14.4: every view has all
// three), plus the instance StateBadge.
import { type ReactNode } from "react";
import { Button } from "./button";
import { cn } from "./cn";

export function Loading({ what }: { what: string }) {
  return (
    <div className="flex items-center gap-2 py-10 text-sm text-subtext0" role="status">
      <span
        aria-hidden
        className="size-4 animate-spin rounded-full border-2 border-surface1 border-t-accent"
      />
      Loading {what}…
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return (
    <div className="rounded-lg border border-dashed border-surface1 px-6 py-10 text-center text-sm text-subtext0">
      {children}
    </div>
  );
}

export function ErrorState({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div
      role="alert"
      className="flex items-center justify-between gap-4 rounded-lg border border-red/40 bg-red/10 px-4 py-3 text-sm"
    >
      <span>
        <span className="font-medium text-red">Error: </span>
        {message}
      </span>
      {onRetry && (
        <Button variant="outline" size="sm" onClick={onRetry}>
          Retry
        </Button>
      )}
    </div>
  );
}

// StateBadge maps the observed state to its Catppuccin color (SPEC 14.2)
// and ALWAYS pairs the color with a text label. The color sits on the dot
// and the border, never on the text itself: Latte Yellow is too low
// contrast for body text on a light base.
const stateClasses: Record<string, { dot: string; border: string }> = {
  running: { dot: "bg-state-running", border: "border-state-running/50" },
  starting: { dot: "bg-state-starting", border: "border-state-starting/60" },
  stopped: { dot: "bg-state-stopped", border: "border-state-stopped/50" },
};
const errorClasses = { dot: "bg-state-error", border: "border-state-error/50" };

export function StateBadge({ state }: { state: string }) {
  const known = state in stateClasses;
  const c = known ? stateClasses[state] : errorClasses;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs text-text",
        c.border,
      )}
    >
      <span aria-hidden className={cn("size-2 rounded-full", c.dot)} />
      {known ? state : `error (${state || "unknown"})`}
    </span>
  );
}
