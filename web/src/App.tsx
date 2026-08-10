import * as Tabs from "@radix-ui/react-tabs";
import { useState } from "react";
import { InstancesView } from "./views/InstancesView";
import { ImagesView } from "./views/ImagesView";
import { KeysView } from "./views/KeysView";
import { AccountView } from "./views/AccountView";
import { ThemeToggle } from "./components/ThemeToggle";
import { cn } from "./components/ui/cn";

const tabs = [
  { id: "instances", label: "Instances" },
  { id: "images", label: "Images" },
  { id: "keys", label: "SSH keys" },
  { id: "account", label: "Account" },
];

export function App() {
  const [tab, setTab] = useState("instances");
  return (
    <Tabs.Root value={tab} onValueChange={setTab}>
      <div className="mx-auto max-w-5xl px-4 pb-16">
        <header className="flex items-center justify-between gap-4 py-5">
          <div className="flex items-center gap-6">
            <h1 className="text-lg font-semibold tracking-tight">
              <span aria-hidden>🍱</span> bento
            </h1>
            <Tabs.List aria-label="Sections" className="flex gap-1">
              {tabs.map((t) => (
                <Tabs.Trigger
                  key={t.id}
                  value={t.id}
                  className={cn(
                    "rounded-md px-3 py-1.5 text-sm text-subtext0 hover:bg-surface0 hover:text-text",
                    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent",
                    "data-[state=active]:bg-surface0 data-[state=active]:font-medium data-[state=active]:text-text",
                  )}
                >
                  {t.label}
                </Tabs.Trigger>
              ))}
            </Tabs.List>
          </div>
          <ThemeToggle />
        </header>
        <main>
          <Tabs.Content value="instances">
            <InstancesView />
          </Tabs.Content>
          <Tabs.Content value="images">
            <ImagesView />
          </Tabs.Content>
          <Tabs.Content value="keys">
            <KeysView />
          </Tabs.Content>
          <Tabs.Content value="account">
            <AccountView />
          </Tabs.Content>
        </main>
      </div>
    </Tabs.Root>
  );
}
