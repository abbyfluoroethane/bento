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
            <h1>
              <span
                role="img"
                aria-label="bento"
                className={cn(
                  "block h-6 aspect-[722/136] bg-text",
                  "[mask-image:url(/branding/wordmark.png)] [mask-repeat:no-repeat] [mask-size:contain] [mask-position:left_center]",
                  "[-webkit-mask-image:url(/branding/wordmark.png)] [-webkit-mask-repeat:no-repeat] [-webkit-mask-size:contain] [-webkit-mask-position:left_center]",
                )}
              />
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
