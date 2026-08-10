import { type ButtonHTMLAttributes, forwardRef } from "react";
import { cn } from "./cn";

type Variant = "default" | "secondary" | "ghost" | "destructive" | "outline";
type Size = "sm" | "md";

const variants: Record<Variant, string> = {
  default: "bg-accent text-accent-fg hover:opacity-90",
  secondary: "bg-surface0 text-text hover:bg-surface1",
  ghost: "bg-transparent text-text hover:bg-surface0",
  destructive: "bg-red text-base hover:opacity-90",
  outline: "border border-surface1 bg-transparent text-text hover:bg-surface0",
};

const sizes: Record<Size, string> = {
  sm: "h-7 px-2.5 text-xs",
  md: "h-9 px-4 text-sm",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant = "default", size = "md", type = "button", ...props }, ref) => (
    <button
      ref={ref}
      type={type}
      className={cn(
        "inline-flex items-center justify-center gap-1.5 rounded-md font-medium",
        "transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent",
        "disabled:pointer-events-none disabled:opacity-50",
        variants[variant],
        sizes[size],
        className,
      )}
      {...props}
    />
  ),
);
Button.displayName = "Button";
