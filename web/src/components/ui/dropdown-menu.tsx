// Dropdown menu built on the Radix primitive (SPEC 14.1): arrow keys,
// typeahead, and focus handling come from Radix, not from a div.
import * as Menu from "@radix-ui/react-dropdown-menu";
import { type ReactNode } from "react";
import { cn } from "./cn";

export const DropdownMenu = Menu.Root;
export const DropdownMenuTrigger = Menu.Trigger;

export function DropdownMenuContent({ children }: { children: ReactNode }) {
  return (
    <Menu.Portal>
      <Menu.Content
        align="end"
        sideOffset={4}
        className={cn(
          "z-50 min-w-44 rounded-md border border-surface1 bg-mantle p-1 shadow-lg",
          "text-sm text-text",
        )}
      >
        {children}
      </Menu.Content>
    </Menu.Portal>
  );
}

export function DropdownMenuItem({
  children,
  onSelect,
  disabled,
  destructive,
}: {
  children: ReactNode;
  onSelect: () => void;
  disabled?: boolean;
  destructive?: boolean;
}) {
  return (
    <Menu.Item
      disabled={disabled}
      onSelect={onSelect}
      className={cn(
        "flex cursor-default select-none items-center rounded px-2 py-1.5 outline-none",
        "data-[highlighted]:bg-surface0",
        "data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        destructive && "text-red data-[highlighted]:bg-red data-[highlighted]:text-base",
      )}
    >
      {children}
    </Menu.Item>
  );
}

export function DropdownMenuSeparator() {
  return <Menu.Separator className="my-1 h-px bg-surface1" />;
}
