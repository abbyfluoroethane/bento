// Bento dashboard client. Everything here is optional: the pages work
// without it. It adds the theme switch, the charts, the steppers, the
// rename confirmation, and re-initialises after HTMX swaps.
(function () {
  "use strict";

  // --- Theme (SPEC 14.2) -------------------------------------------------
  // Basecoat's switcher (window.basecoat.theme) owns the class and the
  // stored choice (localStorage.themeMode). This follows the OS while no
  // choice is stored, and redraws the charts whenever the class flips.
  function stored() { try { return localStorage.getItem("themeMode"); } catch (e) { return null; } }
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function (e) {
    if (!stored()) document.documentElement.classList.toggle("dark", e.matches);
  });
  new MutationObserver(function () {
    document.querySelectorAll(".chart[data-src]").forEach(function (el) { if (el._plot) render(el, el._data); });
  }).observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });

  // --- Charts (uPlot) ----------------------------------------------------
  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  // One unit per axis: GiB when the scale reaches a GiB, else MiB.
  function fmtValue(kind, v, max) {
    if (v == null) return "–";
    if (kind === "pct") return Math.round(v) + "%";
    if ((max || v) >= 1024) return (v / 1024).toFixed(1).replace(/\.0$/, "") + " GiB";
    return Math.round(v) + " MiB";
  }
  function render(el, data) {
    var kind = el.dataset.kind || "pct";
    var max = kind === "pct" ? 100 : Number(el.dataset.max) || null;
    var series = data[el.dataset.series];
    if (!series || !series.at.length) {
      el.innerHTML = '<div class="chart-empty">No samples yet.</div>';
      return;
    }
    if (max == null) max = Math.max.apply(null, series.value) * 1.1;
    var color = cssVar("--chart-1");
    var grid = cssVar("--border");
    var ink = cssVar("--muted-foreground");
    var opts = {
      width: el.clientWidth || 600,
      height: el.clientHeight || 160,
      cursor: { points: { size: 8 } },
      legend: { show: false },
      scales: { x: { time: true }, y: { range: [0, max] } },
      axes: [
        { stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid }, font: "11px IBM Plex Sans", space: 80 },
        { stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid }, font: "11px IBM Plex Sans", size: 64, gap: 6,
          values: function (u, vals) { return vals.map(function (v) { return fmtValue(kind, v, max); }); } }
      ],
      series: [
        {},
        { stroke: color, width: 2, fill: color + "22", points: { show: false }, value: function (u, v) { return fmtValue(kind, v, max); } }
      ]
    };
    if (el._plot) { el._plot.destroy(); }
    el.innerHTML = "";
    el._data = data;
    el._plot = new uPlot(opts, [series.at, series.value], el);
    var now = el.closest(".chart-card") && el.closest(".chart-card").querySelector(".now");
    if (now) now.textContent = fmtValue(kind, series.value[series.value.length - 1], max);
  }
  function loadChart(el) {
    if (typeof uPlot === "undefined") return;
    if (el.dataset.json) {
      // Samples embedded in the page (previews, or a server that inlines them).
      try { render(el, JSON.parse(el.dataset.json)); } catch (e) { el.innerHTML = '<div class="chart-empty">Bad sample data.</div>'; }
      return;
    }
    fetch(el.dataset.src, { credentials: "same-origin", headers: { "Accept": "application/json" } })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(r.status); })
      .then(function (data) {
        render(el, data);
        var badge = el.closest(".chart-card") && el.closest(".chart-card").querySelector(".sample");
        if (badge) badge.hidden = !data.placeholder;
      })
      .catch(function () { el.innerHTML = '<div class="chart-empty">Could not load samples.</div>'; });
  }
  var timers = [];
  function initCharts(root) {
    (root || document).querySelectorAll(".chart[data-src]").forEach(function (el) {
      watch(el);
      if (el._timer) return;
      loadChart(el);
      if (el.dataset.json) { el._timer = -1; return; }
      el._timer = setInterval(function () { if (document.contains(el)) loadChart(el); else clearInterval(el._timer); }, 30000);
      timers.push(el._timer);
    });
  }
  // Plots follow their container, not the other way round: the card sets
  // the size, and the plot is resized to it.
  var resizeTimer = null;
  function fitPlots() {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(function () {
      document.querySelectorAll(".chart[data-src]").forEach(function (el) {
        if (el._plot && el.clientWidth) el._plot.setSize({ width: el.clientWidth, height: el.clientHeight || 160 });
      });
    }, 100);
  }
  window.addEventListener("resize", fitPlots);
  var observer = window.ResizeObserver ? new ResizeObserver(fitPlots) : null;
  function watch(el) { if (observer && !el._watched) { observer.observe(el); el._watched = true; } }

  // --- Sidebar: the machine tree toggle, remembered per browser ----------
  function applyTree() {
    var collapsed = false;
    try { collapsed = localStorage.getItem("vmTree") === "closed"; } catch (e) {}
    var tree = document.getElementById("vm-tree");
    var button = document.querySelector("[data-vm-tree-toggle]");
    if (tree) tree.hidden = collapsed;
    if (button) button.setAttribute("aria-expanded", String(!collapsed));
  }
  document.addEventListener("click", function (event) {
    var button = event.target.closest("[data-vm-tree-toggle]");
    if (!button) return;
    var open = button.getAttribute("aria-expanded") !== "true";
    try { localStorage.setItem("vmTree", open ? "open" : "closed"); } catch (e) {}
    applyTree();
  });

  // --- Sidebar on small screens ------------------------------------------
  document.addEventListener("click", function (event) {
    var side = document.getElementById("sidebar");
    if (!side) return;
    if (event.target.closest("[data-toggle-sidebar]")) { side.classList.toggle("open"); return; }
    if (side.classList.contains("open") && !side.contains(event.target)) side.classList.remove("open");
  });

  // --- Steppers ----------------------------------------------------------
  document.addEventListener("click", function (event) {
    var button = event.target.closest("[data-step]");
    if (!button) return;
    var input = button.closest(".stepper").querySelector("input");
    var step = Number(input.step) || 1;
    var value = Number(input.value) || 0;
    value += button.dataset.step === "up" ? step : -step;
    var min = input.min !== "" ? Number(input.min) : -Infinity;
    var max = input.max !== "" ? Number(input.max) : Infinity;
    value = Math.min(max, Math.max(min, value));
    input.value = Number.isInteger(step) ? String(value) : value.toFixed(1).replace(/\.0$/, "");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });

  // --- Settings: confirm a rename before saving (SPEC 7.3, 14.4) --------
  // Capture phase: HTMX's boosted-form listener sits on the form itself and
  // would otherwise send the request before this runs.
  document.addEventListener("submit", function (event) {
    var form = event.target;
    if (!form.matches("[data-confirm-rename]")) return;
    var input = form.querySelector('input[name="name"]');
    if (!input || input.value === input.dataset.current || form.dataset.renameConfirmed) return;
    var dialog = document.getElementById("rename-dialog");
    if (!dialog) return;
    event.preventDefault();
    event.stopPropagation();
    dialog.querySelectorAll("[data-new-name]").forEach(function (el) { el.textContent = input.value; });
    dialog.showModal();
    dialog.querySelector("[data-rename-go]").onclick = function () {
      form.dataset.renameConfirmed = "1";
      dialog.close();
      form.requestSubmit();
    };
  }, true);

  // --- Delete: arm the button on a typed match ---------------------------
  document.addEventListener("input", function (event) {
    var input = event.target;
    if (!input.matches("[data-confirm-name]")) return;
    // The button may sit outside the form (dialog footer) and be linked by
    // its form attribute, so look through the form's elements, not its tree.
    var button = input.form && Array.prototype.find.call(input.form.elements, function (e) { return e.hasAttribute("data-armed"); });
    if (button) button.disabled = input.value.trim() !== input.dataset.confirmName;
  });

  // --- Toasts: show, then drop the query string that carried them --------
  function cleanUrl() {
    if (!window.history.replaceState) return;
    var url = new URL(window.location.href);
    if (url.searchParams.has("toast") || url.searchParams.has("warn")) {
      url.searchParams.delete("toast");
      url.searchParams.delete("warn");
      window.history.replaceState({}, "", url.pathname + (url.search || ""));
    }
  }

  // --- Boot, and re-boot after HTMX swaps --------------------------------
  function boot(root) {
    applyTree();
    initCharts(root);
    cleanUrl();
  }
  document.addEventListener("DOMContentLoaded", function () { boot(document); });
  document.addEventListener("htmx:afterSettle", function (event) {
    boot(event.target);
    if (window.basecoat && window.basecoat.initAll) window.basecoat.initAll();
  });
  document.addEventListener("htmx:pushedIntoHistory", cleanUrl);
  document.addEventListener("htmx:replacedInHistory", cleanUrl);
  document.addEventListener("htmx:responseError", function (event) {
    var toaster = document.getElementById("toaster");
    if (!toaster) return;
    var status = event.detail.xhr ? event.detail.xhr.status : "";
    var el = document.createElement("div");
    el.className = "toast";
    el.setAttribute("role", "alert");
    el.setAttribute("aria-atomic", "true");
    el.setAttribute("aria-hidden", "false");
    el.dataset.category = "error";
    el.innerHTML = '<div class="toast-content"><section><h2>Request failed</h2><p>The server answered ' + status + '. Reload the page.</p></section></div>';
    toaster.appendChild(el);
  });
})();
