// Dialog built on the Radix primitive (SPEC 14.1: never a plain div —
// Radix supplies focus management and keyboard navigation).
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { type ReactNode } from "react";
import { cn } from "./cn";

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;

export function DialogContent({
  title,
  description,
  children,
  wide,
}: {
  title: string;
  description?: ReactNode;
  children?: ReactNode;
  wide?: boolean;
}) {
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Overlay className="fixed inset-0 z-40 bg-crust/60 backdrop-blur-[2px]" />
      <DialogPrimitive.Content
        className={cn(
          "fixed left-1/2 top-1/2 z-50 w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2",
          wide ? "max-w-lg" : "max-w-md",
          "rounded-lg border border-surface1 bg-mantle p-5 shadow-xl",
          "focus:outline-none",
        )}
      >
        <DialogPrimitive.Title className="font-mono text-base font-semibold">
          {title}
        </DialogPrimitive.Title>
        {description !== undefined && (
          <DialogPrimitive.Description asChild>
            <div className="mt-2 text-sm text-subtext0">{description}</div>
          </DialogPrimitive.Description>
        )}
        {children}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  );
}

export function DialogFooter({ children }: { children: ReactNode }) {
  return <div className="mt-5 flex justify-end gap-2">{children}</div>;
}
