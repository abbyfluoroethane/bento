// Browser check for the dashboard. Start the pages over the test fakes
// with `BENTO_DEV_PORT=18080 cargo test -p bento-api dev_server -- --nocapture`,
// then run `node check.js` from a directory where `npm install playwright`
// has been done (see TESTING.md). It screenshots every page in both
// themes and two viewports, exercises the interactive bits, and reports
// console errors, failed requests, overflow, and layout facts.
const { chromium } = require("playwright");
const fs = require("fs");
const BASE = process.env.BASE || "http://127.0.0.1:18080";
const OUT = process.env.OUT || "shots";
fs.mkdirSync(OUT, { recursive: true });

const pages = [
  ["home", "/"],
  ["new", "/new"],
  ["vm", "/vm/uuid-web"],
  ["vm_settings", "/vm/uuid-web/settings"],
  ["vm_danger", "/vm/uuid-web/danger"],
  ["vm_terminal", "/vm/uuid-web/terminal"],
  ["vm_shared", "/vm/uuid-db/settings"],
  ["account", "/settings/account"],
  ["users", "/settings"],
  ["configuration", "/settings/configuration"],
  ["missing", "/vm/nope"],
];

(async () => {
  const browser = await chromium.launch();
  const problems = [];
  const facts = [];
  for (const [theme, dark] of [["latte", false], ["mocha", true]]) {
    for (const [vp, size] of [["desktop", { width: 1380, height: 900 }], ["mobile", { width: 390, height: 844 }]]) {
      const ctx = await browser.newContext({ viewport: size, colorScheme: dark ? "dark" : "light" });
      const page = await ctx.newPage();
      let current = "";
      page.on("console", (m) => { if ((m.type() === "error" || m.type() === "warning") && !(current === "/vm/nope" && m.text().includes("404"))) problems.push(`${theme}/${vp} console ${m.type()} on ${current}: ${m.text()}`); });
      page.on("pageerror", (e) => problems.push(`${theme}/${vp} pageerror: ${e.message}`));
      page.on("requestfailed", (r) => problems.push(`${theme}/${vp} requestfailed: ${r.url()} ${r.failure()?.errorText}`));
      page.on("response", (r) => { if (r.status() >= 400 && !r.url().includes("/vm/nope")) problems.push(`${theme}/${vp} ${r.status()} ${r.url()}`); });
      for (const [name, path] of pages) {
        current = path;
        await page.goto(BASE + path, { waitUntil: "networkidle" });
        await page.waitForTimeout(400);
        const overflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
        if (overflow) problems.push(`${theme}/${vp} horizontal overflow on ${path}`);
        const isDark = await page.evaluate(() => document.documentElement.classList.contains("dark"));
        if (isDark !== dark) problems.push(`${theme}/${vp} theme class wrong on ${path}`);
        if (vp === "desktop" && theme === "latte") {
          const info = await page.evaluate(() => {
            const r = (sel) => { const el = document.querySelector(sel); if (!el) return null; const b = el.getBoundingClientRect(); return { x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height) }; };
            return { side: r(".side"), content: r(".content"), firstBlock: r(".content > *"), chart: r(".chart"), chartCanvas: r(".chart canvas"), footerBtn: r(".account-menu > .btn"), title: document.title, h1: document.querySelector("h1")?.textContent?.trim() };
          });
          facts.push(`${path}: ${JSON.stringify(info)}`);
        }
        await page.screenshot({ path: `${OUT}/${theme}-${vp}-${name}.png`, fullPage: true });
      }
      await ctx.close();
    }
  }

  // Interactions, desktop light.
  const ctx = await browser.newContext({ viewport: { width: 1380, height: 900 } });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => problems.push(`interact pageerror: ${e.message}`));
  page.on("console", (m) => { if (m.type() === "error") problems.push(`interact console: ${m.text()}`); });
  const step = async (label, fn) => { try { await fn(); facts.push(`ok: ${label}`); } catch (e) { problems.push(`interact ${label}: ${e.message.split("\n")[0]}`); } };

  await page.goto(BASE + "/", { waitUntil: "networkidle" });
  await step("account menu opens upward with Settings and Sign out", async () => {
    await page.click("#account-menu-trigger");
    await page.waitForSelector("#account-menu-popover[aria-hidden=false]", { timeout: 2000 });
    const items = await page.$$eval("#account-menu-menu [role=menuitem]", (els) => els.map((e) => e.textContent.trim()));
    if (!items.includes("Settings") || !items.includes("Sign out")) throw new Error("items: " + items.join(","));
    const pop = await page.$eval("#account-menu-popover", (e) => e.getBoundingClientRect().bottom);
    const btn = await page.$eval("#account-menu-trigger", (e) => e.getBoundingClientRect().top);
    if (pop > btn + 2) throw new Error(`popover bottom ${pop} below trigger top ${btn}`);
    await page.screenshot({ path: `${OUT}/interact-menu.png` });
    await page.keyboard.press("Escape");
  });
  await step("hx-boost navigation to a VM keeps the sidebar and swaps content", async () => {
    await page.click('#vm-list a[href="/vm/uuid-web"]');
    await page.waitForURL("**/vm/uuid-web");
    await page.waitForSelector("h1.vm-title");
    await page.waitForFunction(() => document.querySelector(".chart canvas"));
  });
  await step("state badge fragment polls without error", async () => {
    const [res] = await Promise.all([page.waitForResponse((r) => r.url().includes("/fragments/state"), { timeout: 12000 })]);
    if (res.status() !== 200) throw new Error("status " + res.status());
    await page.waitForTimeout(200);
    if (!(await page.$("#vm-state .badge"))) throw new Error("badge missing after swap");
    // The poll must replace only itself: the shell stays around it.
    if (!(await page.$("#sidebar")) || !(await page.$("h1.vm-title"))) throw new Error("poll replaced the page");
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.waitForResponse((r) => r.url().includes("/fragments/instances"), { timeout: 12000 });
    await page.waitForResponse((r) => r.url().includes("/fragments/sidebar"), { timeout: 17000 });
    await page.waitForTimeout(200);
    if (!(await page.$("#sidebar .vm-group")) || !(await page.$(".tiles")) || !(await page.$("#instances table"))) throw new Error("a fragment poll replaced the front page");
  });
  await step("steppers change values and clamp", async () => {
    await page.goto(BASE + "/new", { waitUntil: "networkidle" });
    await page.click('.stepper:has(#vcpu) [data-step=up]');
    await page.click('.stepper:has(#memory_gib) [data-step=down]');
    const v = await page.$eval("#vcpu", (e) => e.value);
    const m = await page.$eval("#memory_gib", (e) => e.value);
    if (v !== "3" || m !== "1.5") throw new Error(`vcpu=${v} mem=${m}`);
    for (let i = 0; i < 5; i++) await page.click('.stepper:has(#vcpu) [data-step=down]');
    if ((await page.$eval("#vcpu", (e) => e.value)) !== "1") throw new Error("did not clamp at 1");
  });
  await step("create form posts, redirects to the new VM, and shows a toast", async () => {
    await page.fill("#name", "fresh");
    await page.click('button[type=submit]:has-text("Create VM")');
    await page.waitForURL("**/vm/uuid-fresh**");
    await page.waitForSelector(".toast", { timeout: 3000 });
    const t = await page.$eval(".toast h2", (e) => e.textContent);
    if (!t.includes("Created fresh")) throw new Error("toast: " + t);
    if (page.url().includes("toast=")) throw new Error("toast param not cleaned: " + page.url());
    await page.screenshot({ path: `${OUT}/interact-created.png` });
  });
  await step("settings rename asks for confirmation, then saves", async () => {
    await page.goto(BASE + "/vm/uuid-web/settings", { waitUntil: "networkidle" });
    await page.fill("#name", "web2");
    await page.click('button[type=submit]:has-text("Save changes")');
    await page.waitForFunction(() => document.getElementById("rename-dialog")?.open, null, { timeout: 5000 });
    const txt = await page.$eval("#rename-dialog", (e) => e.textContent);
    if (!txt.includes("web2@bento.example")) throw new Error("dialog text lacks new name");
    await page.screenshot({ path: `${OUT}/interact-rename.png` });
    await page.click("[data-rename-go]");
    await page.waitForSelector(".toast", { timeout: 3000 });
    const t = await page.$eval(".toast h2", (e) => e.textContent);
    if (!t.includes("Saved name")) throw new Error("toast: " + t);
    if (!(await page.$eval("h1.vm-title", (e) => e.textContent)).includes("web2")) throw new Error("title not renamed");
  });
  await step("delete dialog arms only on the exact name", async () => {
    await page.goto(BASE + "/vm/uuid-web/danger", { waitUntil: "networkidle" });
    await page.click('button:has-text("Delete VM")');
    await page.waitForFunction(() => document.getElementById("delete-dialog")?.open, null, { timeout: 5000 });
    if (!(await page.$eval("[data-armed]", (e) => e.disabled))) throw new Error("armed too early");
    await page.fill("#confirm", "web");
    if (!(await page.$eval("[data-armed]", (e) => e.disabled))) throw new Error("armed on old name");
    await page.fill("#confirm", "web2");
    if (await page.$eval("[data-armed]", (e) => e.disabled)) throw new Error("not armed on exact name");
    await page.screenshot({ path: `${OUT}/interact-delete.png` });
    await page.click("[data-armed]");
    await page.waitForURL(BASE + "/");
    await page.waitForSelector(".toast");
    if (await page.$('#vm-list a[href="/vm/uuid-web"]')) throw new Error("deleted VM still in sidebar");
  });
  await step("theme picker switches and persists", async () => {
    await page.goto(BASE + "/settings/account", { waitUntil: "networkidle" });
    await page.click(".theme-toggle");
    if (!(await page.evaluate(() => document.documentElement.classList.contains("dark")))) throw new Error("not dark");
    await page.reload({ waitUntil: "networkidle" });
    if (!(await page.evaluate(() => document.documentElement.classList.contains("dark")))) throw new Error("not persisted");
    await page.screenshot({ path: `${OUT}/interact-account-dark.png`, fullPage: true });
    await page.click(".theme-toggle");
    await page.evaluate(() => localStorage.removeItem("themeMode"));
  });
  await step("keyboard: tab reaches the account menu and Enter opens it", async () => {
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.focus("#account-menu-trigger");
    await page.keyboard.press("Enter");
    await page.waitForSelector("#account-menu-popover[aria-hidden=false]", { timeout: 2000 });
    await page.keyboard.press("Escape");
  });
  await step("machine tree collapses from the button group and stays collapsed", async () => {
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.click("[data-vm-tree-toggle]");
    if (!(await page.$eval("#vm-tree", (e) => e.hidden))) throw new Error("tree still shown");
    await page.reload({ waitUntil: "networkidle" });
    if (!(await page.$eval("#vm-tree", (e) => e.hidden))) throw new Error("collapse not remembered");
    await page.click("[data-vm-tree-toggle]");
    if (await page.$eval("#vm-tree", (e) => e.hidden)) throw new Error("tree did not reopen");
    await page.click('.vm-group > a[href="/"]');
    await page.waitForURL(BASE + "/");
  });
  await step("mobile sidebar toggle", async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    const before = await page.$eval("#sidebar", (e) => e.getBoundingClientRect().right);
    if (before > 0) throw new Error("sidebar visible before toggle: right=" + before);
    await page.click("[data-toggle-sidebar]");
    await page.waitForTimeout(300);
    const after = await page.$eval("#sidebar", (e) => e.getBoundingClientRect().right);
    if (after <= 0) throw new Error("sidebar did not open");
    await page.screenshot({ path: `${OUT}/interact-mobile-sidebar.png` });
  });
  await ctx.close();
  await browser.close();
  fs.writeFileSync(`${OUT}/report.txt`, ["PROBLEMS", ...problems, "", "FACTS", ...facts].join("\n"));
  console.log(problems.length + " problems, " + facts.length + " facts; see " + OUT + "/report.txt");
})().catch((e) => { console.error("FATAL", e); process.exit(1); });
