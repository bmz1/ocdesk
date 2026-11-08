// Spike: act as opencode's *desktop browser* for one session, the way
// opencode's own Electron app does (packages/gui-plugins/src/browser/pane.ts),
// so the agent's built-in browser tools (`tools.browser.*` in code mode,
// @opencode/plugin-browser, opencode 2.2.6) run against a Chromium we own.
//
// Protocol (rpc id "experimental.browser", version 4):
//   - subscribe to GET /api/event; wait for {type:"server.connected"}
//   - POST /api/rpc/experimental.browser/attach {input:{sessionID,connectionID,version:4}}
//     stays pending for the attachment's lifetime; resolves "closed"|"replaced"
//   - event rpc.experimental.browser.control {type:"attached",connectionID,version:4}
//       -> POST state {input:{sessionID,connectionID,state:{tabs,focusedTabID}}}
//   - event control {type:"command",connectionID,requestID}
//       -> POST command {input:{sessionID,connectionID,requestID}} -> {output:{action,files,...}}
//       -> run it, POST result {input:{...,requestID,outcome:{type:"success",result:{value,files}}}}
//          or outcome {type:"failure",code,message} ("unsupported" for operations we lack)
//   - event control {type:"cancel",...}
//
// Usage: OPENCODE_URL=http://127.0.0.1:PORT OPENCODE_PASSWORD=… DIR=/repo SESSION=ses_… \
//        node dev/browser-attach-spike.mjs
// Playwright (preinstalled in the dev sandbox) drives Chromium in-process
// (pipe transport, no debugging port). The real thing in ocdesk would be
// Rust over CDP; this only proves the protocol.

import { chromium } from "playwright";
import { randomUUID } from "node:crypto";

const URL_ = process.env.OPENCODE_URL;
const DIR = process.env.DIR;
const SESSION = process.env.SESSION;
const auth = "Basic " + Buffer.from(`opencode:${process.env.OPENCODE_PASSWORD ?? ""}`).toString("base64");
const loc = `location[directory]=${encodeURIComponent(DIR)}`;
const connectionID = randomUUID();
const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);

async function rpc(method, input) {
  const res = await fetch(`${URL_}/api/rpc/experimental.browser/${method}?${loc}`, {
    method: "POST",
    headers: { authorization: auth, "content-type": "application/json" },
    body: JSON.stringify({ input }),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method}: ${res.status} ${text}`);
  return text ? JSON.parse(text).output : undefined;
}

// ---- browser side -------------------------------------------------------
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
const tabs = new Map(); // tabID -> { page, generation, console: [] }
let focused = null;

const tabInfo = async (id) => {
  const t = tabs.get(id);
  return {
    id,
    url: t.page.url(),
    title: (await t.page.title().catch(() => "")).slice(0, 2048),
    loading: false,
    canGoBack: false,
    canGoForward: false,
    generation: t.generation,
  };
};
const state = async () => ({
  tabs: await Promise.all([...tabs.keys()].map(tabInfo)),
  focusedTabID: focused,
});
const publish = async () => rpc("state", { sessionID: SESSION, connectionID, state: await state() });
const tab = (id) => {
  const t = tabs.get(id);
  if (!t) throw Object.assign(new Error(`Unknown tab ${id}; call browser.tabs.list.`), { code: "not_found" });
  return t;
};
const ref = (page, r) => page.locator(`aria-ref=${String(r).replace(/^@/, "")}`);

const ops = {
  "tabs.list": async () => state(),
  "tabs.open": async (a) => {
    const id = `tab_${randomUUID()}`;
    const page = await context.newPage();
    const t = { page, generation: 0, console: [] };
    page.on("console", (m) =>
      t.console.push({
        id: `c${t.console.length + 1}`,
        timestampMs: Date.now(),
        level: { warning: "warning", error: "error", debug: "debug" }[m.type()] ?? "info",
        text: m.text().slice(0, 100000),
        textTruncated: false,
      }),
    );
    tabs.set(id, t);
    if (a.focus !== false) focused = id;
    if (a.url && a.url !== "about:blank") await page.goto(a.url, { waitUntil: "load" });
    // Inventory first: the server looks tabs up only in `state`.
    await publish();
    return tabInfo(id);
  },
  "tabs.focus": async (a) => {
    tab(a.tabID);
    focused = a.tabID;
    await publish();
    return tabInfo(a.tabID);
  },
  "tabs.close": async (a) => {
    await tab(a.tabID).page.close();
    tabs.delete(a.tabID);
    if (focused === a.tabID) focused = null;
    await publish();
    return state();
  },
  navigate: async (a) => {
    const t = tab(a.tabID);
    await t.page.goto(a.url, { waitUntil: "load" });
    t.generation++;
    await publish();
    return tabInfo(a.tabID);
  },
  snapshot: async (a) => {
    const content = await tab(a.tabID).page.ariaSnapshot({ mode: "ai" });
    return { tab: await tabInfo(a.tabID), content: content.slice(0, 100000), truncated: content.length > 100000 };
  },
  click: async (a) => {
    await ref(tab(a.tabID).page, a.ref).click({ timeout: 10000 });
    return tabInfo(a.tabID);
  },
  fill: async (a) => {
    await ref(tab(a.tabID).page, a.ref).fill(a.text, { timeout: 10000 });
    return tabInfo(a.tabID);
  },
  press: async (a) => {
    await tab(a.tabID).page.keyboard.press(a.key);
    return tabInfo(a.tabID);
  },
  wait: async (a) => {
    const page = tab(a.tabID).page;
    const timeout = a.timeoutMs ?? 10000;
    if (a.condition === "load") await page.waitForLoadState("load", { timeout });
    else if (a.condition === "text") await page.getByText(a.text).first().waitFor({ timeout });
    else await page.getByText(a.text).first().waitFor({ state: "detached", timeout });
    return tabInfo(a.tabID);
  },
  screenshot: async (a) => {
    const page = tab(a.tabID).page;
    const type = a.format === "jpeg" ? "jpeg" : "png";
    const data = await (a.ref ? ref(page, a.ref) : page).screenshot({ type, fullPage: !!a.fullPage });
    return {
      value: { tab: await tabInfo(a.tabID) },
      files: [{ id: `file_${randomUUID()}`, name: `screenshot.${type}`, mime: `image/${type}`, data: data.toString("base64") }],
    };
  },
  console: async (a) => {
    const all = tab(a.tabID).console.filter((m) => !a.level || m.level === a.level);
    const limit = a.limit ?? 100;
    return { tab: await tabInfo(a.tabID), messages: all.slice(-limit), truncated: all.length > limit, dropped: 0 };
  },
};

async function run(requestID) {
  const command = await rpc("command", { sessionID: SESSION, connectionID, requestID });
  const { type, ...args } = command.action;
  log("command", type, JSON.stringify(args).slice(0, 160));
  let outcome;
  const op = ops[type];
  if (!op) {
    outcome = { type: "failure", code: "unsupported", message: `This desktop does not support ${type} yet.` };
  } else {
    try {
      const out = await op(args);
      const result = out && "files" in out && "value" in out ? out : { value: out, files: [] };
      outcome = { type: "success", result };
    } catch (e) {
      outcome = { type: "failure", code: e.code ?? "failed", message: String(e.message).slice(0, 2000) };
    }
  }
  log("result", type, outcome.type, outcome.type === "failure" ? outcome.message : "");
  await rpc("result", { sessionID: SESSION, connectionID, requestID, outcome });
}

// ---- server side --------------------------------------------------------
const res = await fetch(`${URL_}/api/event`, { headers: { authorization: auth, accept: "text/event-stream" } });
const decoder = new TextDecoder();
let buf = "";
let attaching = null;
for await (const chunk of res.body) {
  buf += decoder.decode(chunk, { stream: true });
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line.startsWith("data:")) continue;
    const ev = JSON.parse(line.slice(5));
    if (ev.type === "server.connected" && !attaching) {
      log("connected; attaching to", SESSION, "as", connectionID);
      attaching = rpc("attach", { sessionID: SESSION, connectionID, version: 4 }).then(
        (out) => (log("attach ended:", out), process.exit(0)),
        (e) => (log("attach failed:", e.message), process.exit(1)),
      );
      continue;
    }
    if (ev.type !== "rpc.experimental.browser.control" || ev.data?.connectionID !== connectionID) continue;
    const c = ev.data;
    if (c.type === "attached") {
      log("attached (version", c.version + ")");
      await publish();
    } else if (c.type === "command") {
      run(c.requestID).catch((e) => log("command failed:", e.message));
    } else if (c.type === "cancel") {
      log("cancel", c.requestID);
    }
  }
}
