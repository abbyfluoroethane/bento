// Tiny class-name join helper; keeps the bundle free of clsx/tailwind-merge.
export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}
