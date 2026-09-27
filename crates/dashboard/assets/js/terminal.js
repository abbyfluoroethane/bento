// The web terminal (SPEC 14.6): a ghostty-web terminal joined to a guest
// SSH session through a WebSocket. It is the same as `ssh`, not a serial
// console. No build step: this file is an ES module that the Terminal tab
// loads, and it is the only page that loads ghostty-web (SPEC 14.1).
import { init, Terminal, FitAddon } from "./ghostty-web.js";

const START_LABEL = "Start and connect";
const RECONNECT_LABEL = "Reconnect";
const REFUSED =
  "Could not connect. Check that you can open this VM, then reconnect.";

// Close codes of SPEC 14.6. 4409 is the answer to a connect without
// start=1 for a VM that is not running.
const NORMAL = 1000;
const FAILURE = 1011;
const ABNORMAL = 1006;
const NOT_RUNNING = 4409;

// Theme keys of ghostty-web, and the custom property that gives each one.
// The tokens are the Catppuccin palette of SPEC 14.2, set in app.css, so
// a change to the palette stays in one place.
const THEME_TOKENS = {
  foreground: "--foreground",
  background: "--crust",
  cursor: "--term-cursor",
  cursorAccent: "--crust",
  selectionBackground: "--term-selection",
  selectionForeground: "--foreground",
  black: "--term-black",
  red: "--term-red",
  green: "--term-green",
  yellow: "--term-yellow",
  blue: "--term-blue",
  magenta: "--term-magenta",
  cyan: "--term-cyan",
  white: "--term-white",
  brightBlack: "--term-bright-black",
  brightRed: "--term-bright-red",
  brightGreen: "--term-bright-green",
  brightYellow: "--term-bright-yellow",
  brightBlue: "--term-bright-blue",
  brightMagenta: "--term-bright-magenta",
  brightCyan: "--term-bright-cyan",
  brightWhite: "--term-bright-white",
};

const encoder = new TextEncoder();
const sessions = new Set();
let ready = null;

// Load the WebAssembly parser and the mono font once. The terminal
// measures its cell size when it opens, so the font must be ready first
// (SPEC 14.3).
function whenReady() {
  if (!ready) {
    const font = mono(document.documentElement);
    ready = Promise.all([
      init(),
      // A font that does not load is not fatal: the terminal then uses
      // the next family in the list.
      document.fonts
        ? document.fonts.load(`14px ${font}`).then(() => document.fonts.ready).catch(() => {})
        : null,
    ]).catch((error) => {
      ready = null;
      throw error;
    });
  }
  return ready;
}

function mono(element) {
  const value = getComputedStyle(element).getPropertyValue("--font-mono").trim();
  return value || "monospace";
}

// ghostty-web reads only #rrggbb and rgb(r, g, b). The tokens are hex or
// hsl(), so let the browser resolve each one, then let a canvas write it
// as hex.
function readTheme(element) {
  const probe = document.createElement("span");
  probe.hidden = true;
  element.append(probe);
  const context = document.createElement("canvas").getContext("2d");
  const theme = {};
  for (const [key, token] of Object.entries(THEME_TOKENS)) {
    probe.style.color = "";
    probe.style.color = `var(${token})`;
    const color = getComputedStyle(probe).color;
    if (context) {
      context.fillStyle = "#000000";
      context.fillStyle = color;
      theme[key] = context.fillStyle;
    } else {
      theme[key] = color;
    }
  }
  probe.remove();
  return theme;
}

function describeClose(event) {
  // Drop a final full stop; the status line adds its own.
  const reason = (event.reason || "").trim().replace(/[.\s]+$/, "");
  switch (event.code) {
    case NORMAL:
      return reason ? `the session ended (${reason})` : "the session ended";
    case NOT_RUNNING:
      return reason || "the VM is not running";
    case FAILURE:
      return reason || "the server could not open the session";
    case ABNORMAL:
      return null;
    default:
      return reason || (event.code ? `code ${event.code}` : null);
  }
}

function mount(root) {
  if (root.dataset.terminalMounted) return;
  const fallback = root.querySelector("[data-terminal-fallback]");
  const live = root.querySelector("[data-terminal-live]");
  const status = root.querySelector("[data-terminal-status]");
  const button = root.querySelector("[data-terminal-connect]");
  const screen = root.querySelector("[data-terminal-screen]");
  if (!live || !status || !button || !screen) return;
  root.dataset.terminalMounted = "true";

  const session = {
    root,
    status,
    button,
    screen,
    uuid: root.dataset.uuid,
    // Each connect and each leave adds one. A connect that finds a
    // different number after it waits was replaced, so it stops.
    generation: 0,
    socket: null,
    term: null,
    host: null,
  };
  sessions.add(session);

  if (fallback) fallback.hidden = true;
  live.hidden = false;
  button.addEventListener("click", () => connect(session, true));

  // A page load must not start a VM (SPEC 14.6). Connect by itself only
  // when the VM is running; else wait for a click on the button.
  if (root.dataset.state === "running") {
    connect(session, false);
  } else {
    setStatus(session, `The VM is ${root.dataset.state || "not running"}.`);
    showButton(session, START_LABEL);
  }
}

function setStatus(session, text) {
  session.status.textContent = text;
}

function showButton(session, label) {
  session.button.textContent = label;
  session.button.hidden = false;
}

function closeSocket(session, reason) {
  const socket = session.socket;
  session.socket = null;
  if (socket && socket.readyState < WebSocket.CLOSING) {
    socket.close(NORMAL, reason);
  }
}

function disposeTerm(session) {
  if (session.term) session.term.dispose();
  if (session.host) session.host.remove();
  session.term = null;
  session.host = null;
}

async function connect(session, start) {
  const generation = ++session.generation;
  closeSocket(session, "reconnect");
  disposeTerm(session);
  session.button.hidden = true;
  setStatus(session, "Connecting…");

  try {
    await whenReady();
  } catch (error) {
    if (generation !== session.generation) return;
    setStatus(session, `Disconnected: the terminal did not load (${error.message || error}).`);
    showButton(session, RECONNECT_LABEL);
    return;
  }
  if (generation !== session.generation || !session.root.isConnected) return;

  // A new terminal for each connection: it takes the palette again, so a
  // reconnect after a theme switch shows the new colors. ghostty-web does
  // not change the colors of a terminal that is open.
  const host = document.createElement("div");
  host.className = "terminal-host";
  session.screen.append(host);
  const term = new Terminal({
    fontFamily: mono(session.screen),
    fontSize: 14,
    theme: readTheme(session.screen),
    cursorBlink: true,
    scrollback: 10000,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();
  fit.observeResize();
  session.term = term;
  session.host = host;

  // The first size goes in the URL; the server asks for a PTY of that
  // size (SPEC 14.6).
  const query = new URLSearchParams({ cols: String(term.cols), rows: String(term.rows) });
  if (start) query.set("start", "1");
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${scheme}//${location.host}/vm/${encodeURIComponent(session.uuid)}/terminal/ws?${query}`;
  let socket;
  try {
    socket = new WebSocket(url);
  } catch (error) {
    setStatus(session, REFUSED);
    showButton(session, RECONNECT_LABEL);
    return;
  }
  socket.binaryType = "arraybuffer";
  session.socket = socket;
  let sent = { cols: term.cols, rows: term.rows };

  const sendSize = (cols, rows) => {
    if (socket.readyState !== WebSocket.OPEN) return;
    if (cols === sent.cols && rows === sent.rows) return;
    sent = { cols, rows };
    socket.send(JSON.stringify({ type: "resize", cols, rows }));
  };

  // Keyboard input goes as binary frames of raw bytes (SPEC 14.6).
  term.onData((data) => {
    if (socket.readyState === WebSocket.OPEN) socket.send(encoder.encode(data));
  });
  term.onResize(({ cols, rows }) => sendSize(cols, rows));

  socket.addEventListener("open", () => {
    if (session.socket !== socket) return;
    setStatus(session, "Connected");
    // The container can change size between the URL and the upgrade.
    sendSize(term.cols, term.rows);
    term.focus();
  });

  socket.addEventListener("message", (event) => {
    if (session.socket !== socket) return;
    if (event.data instanceof ArrayBuffer) term.write(new Uint8Array(event.data));
  });

  // No automatic reconnect (SPEC 14.6): show why, and give a button.
  socket.addEventListener("close", (event) => {
    if (session.socket !== socket) return;
    session.socket = null;
    term.options.disableStdin = true;
    const reason = describeClose(event);
    setStatus(session, reason ? `Disconnected: ${reason}.` : REFUSED);
    showButton(session, event.code === NOT_RUNNING ? START_LABEL : RECONNECT_LABEL);
  });
}

// The user left the page. Close the socket, so that the server closes the
// guest session, and free the terminal (SPEC 14.6).
function leave(session, reason) {
  session.generation++;
  closeSocket(session, reason);
  disposeTerm(session);
  sessions.delete(session);
  delete session.root.dataset.terminalMounted;
}

function leaveWithin(element, reason) {
  for (const session of [...sessions]) {
    if (!session.root.isConnected || (element && element.contains(session.root))) {
      leave(session, reason);
    }
  }
}

function mountWithin(element) {
  if (!element || !element.querySelectorAll) return;
  if (element.matches && element.matches("[data-terminal]")) mount(element);
  element.querySelectorAll("[data-terminal]").forEach(mount);
}

// HTMX boosted navigation replaces the page body without a page load, so
// the pagehide event does not come. A swap into an element that holds the
// terminal, a history save, or a history restore means the user left.
document.addEventListener("htmx:beforeSwap", (event) => {
  leaveWithin(event.detail && event.detail.target, "page left");
});
document.addEventListener("htmx:beforeHistorySave", () => leaveWithin(document.body, "page left"));
document.addEventListener("htmx:historyRestore", () => leaveWithin(null, "page left"));
// A later boosted swap can bring a terminal back. The module runs only
// once in a document, so mount again from the HTMX load event.
document.addEventListener("htmx:load", (event) => mountWithin(event.detail && event.detail.elt));

// A real page unload, or the back-forward cache. If the browser shows the
// page again from that cache, the session is closed; show that, and let
// the user reconnect.
window.addEventListener("pagehide", () => {
  for (const session of [...sessions]) {
    session.generation++;
    closeSocket(session, "page left");
    if (session.term) session.term.options.disableStdin = true;
    setStatus(session, "Disconnected: you left the page.");
    showButton(session, RECONNECT_LABEL);
  }
});

mountWithin(document.body);
