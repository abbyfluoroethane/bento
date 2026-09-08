// Run against the fake dev server with BENTO_DEV_CREATE_ERROR=no-placement.
// Like check.js, this needs Playwright and accepts BASE and OUT (TESTING.md).
const { chromium } = require("playwright");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const BASE = process.env.BASE || "http://127.0.0.1:18080";
const OUT = process.env.OUT || "shots";

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const browser = await chromium.launch();
  try {
    for (const viewport of [{ width: 1380, height: 900 }, { width: 390, height: 844 }]) {
      const page = await browser.newPage({ viewport });
      const errors = [];
      page.on("pageerror", error => errors.push(error.message));
      await page.goto(BASE + "/new", { waitUntil: "networkidle" });
      await page.fill("#name", "kept-name");
      await page.fill("#vcpu", "3");
      await page.fill("#memory_gib", "2.5");
      await page.fill("#disk_gib", "17");
      await page.check("#public");
      await page.check("#nested");
      const submit = async status => {
        const [response] = await Promise.all([
          page.waitForResponse(r => r.url() === BASE + "/new" && r.request().method() === "POST"),
          page.click('form[data-form-errors] button[type="submit"]'),
        ]);
        assert.equal(response.status(), status);
      };
      await submit(409);
      await page.waitForSelector('.alert[role="alert"]');
      const message = await page.locator('.alert[role="alert"]').innerText();
      assert.match(message, /runner-a\.example\.org: has 0 MiB left/);
      assert.match(message, /runner-b\.example\.org: last seen unreachable/);
      for (const [id, value] of Object.entries({ name: "kept-name", vcpu: "3", memory_gib: "2.5", disk_gib: "17" })) {
        assert.equal(await page.inputValue("#" + id), value);
      }
      assert.equal(await page.inputValue('input[name="image"]'), "debian-13");
      for (const id of ["public", "ksm", "nested"]) assert(await page.isChecked("#" + id));
      assert.equal(await page.locator(".toast").count(), 0, "no generic error toast");
      assert.equal(new URL(page.url()).pathname, "/new");
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      await page.screenshot({ path: `${OUT}/create-conflict-${viewport.width}.png`, fullPage: true });

      // A retry reaches the server and displays its field validation.
      await page.locator('input[name="image"]').evaluate(el => { el.value = ""; });
      await submit(400);
      await page.waitForFunction(() => document.querySelector(".alert")?.textContent.includes("choose an image"));
      assert.equal(await page.inputValue("#name"), "kept-name");

      // Unrelated or unexpected error responses retain the fallback toast.
      await page.locator('input[name="image"]').evaluate(el => { el.value = "debian-13"; });
      await page.route("**/new", route => route.request().method() === "POST"
        ? route.fulfill({ status: 500, contentType: "text/plain", body: "upstream failure" })
        : route.continue());
      await submit(500);
      await page.waitForFunction(() => document.querySelector(".toast")?.textContent.includes("500"));
      assert.equal(await page.inputValue("#name"), "kept-name");
      assert.deepEqual(errors, []);
      await page.close();
    }
    console.log("PASS: create refusals display runner reasons and preserve inputs on desktop and mobile; validation retries and 500 fallback work");
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
