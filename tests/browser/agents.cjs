"use strict";

// Run after `cargo build -p magpie-lite`, with Node >= 18 and Playwright available.
// Only the spawned child receives MAGPIE_LITE_HOME. No real Agent homes are used.
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");
const { spawn } = require("node:child_process");
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || "playwright");
const TOKEN = "sandbox-browser-gateway-token";
const KEY = "sandbox-provider-private-key";
const ORIGINAL_SECRET = "sandbox-original-private-key";
const names = { claude: "Claude Code", codex: "Codex", crush: "Crush", gemini: "Gemini CLI", goose: "Goose", opencode: "OpenCode", zed: "Zed" };
const binary = process.env.MAGPIE_LITE_BIN || path.resolve(__dirname, "../../target/debug/magpie-lite" + (process.platform === "win32" ? ".exe" : ""));

async function write(file, data) {
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, data);
}
function baseline(agent, index) {
  if (agent === "codex") return Buffer.from("# preserve comments\r\nmodel = 'original'\r\n[features]\r\nreview = true\r\n");
  if (agent === "goose" && index === 0) return Buffer.from("# original settings\r\nGOOSE_PROVIDER: original\r\nGOOSE_MODEL: prior\r\ncustom: kept\r\n");
  if (agent === "gemini" && index === 1) return Buffer.from("# keep comments\r\nGEMINI_API_KEY=" + ORIGINAL_SECRET + "\r\nUNRELATED=kept\r\n");
  return Buffer.from('{\r\n  "custom": "kept", "original-secret": "' + ORIGINAL_SECRET + '"\r\n}\r\n');
}
async function startGateway(root) {
  const config = path.join(root, "config.toml");
  await write(config, `port=45554\ntoken='${TOKEN}'\ndefault_provider='mock'\n[[provider]]\nid='mock'\nprotocol='chat'\nbase_url='http://127.0.0.1:9/v1'\napi_key='${KEY}'\nmodels=['alpha','beta']\n[[group]]\nid='daily'\nname='Daily'\nroutes=['mock/alpha','mock/beta']\n`);
  const child = spawn(binary, ["--config", config, "serve", "--port", "0"], {
    cwd: root, env: { ...process.env, MAGPIE_LITE_HOME: path.join(root, "home") },
    windowsHide: true, stdio: ["ignore", "pipe", "pipe"]
  });
  try {
    const url = await new Promise((resolve, reject) => {
      let output = "";
      const timer = setTimeout(() => reject(new Error("Gateway startup timed out: " + output)), 10000);
      const ready = chunk => {
        output += chunk;
        const match = output.match(/listening on (http:\/\/127\.0\.0\.1:\d+)/);
        if (match) { clearTimeout(timer); resolve(match[1]); }
      };
      child.stdout.on("data", ready); child.stderr.on("data", ready);
      child.once("error", error => { clearTimeout(timer); reject(error); });
      child.once("exit", code => { clearTimeout(timer); reject(new Error("Gateway exited: " + code + " " + output)); });
    });
    return { child, config, url };
  } catch (error) { child.kill(); throw error; }
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise(resolve => { child.once("exit", resolve); child.kill(); });
}

// Subtests intentionally run serially against a single isolated gateway.
test("Agent console against a real isolated Rust gateway", { timeout: 120000 }, async t => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "magpie-console-"));
  let gateway, browser;
  try {
    gateway = await startGateway(root);
    const api = async (route, body, method = "POST") => {
      const response = await fetch(gateway.url + route, {
        method: body === undefined && method === "POST" ? "GET" : method,
        headers: { Authorization: "Bearer " + TOKEN, "Content-Type": "application/json" },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(5000)
      });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    };
    const statuses = (await api("/agents")).agents;
    const originals = new Map();
    for (const agent of statuses) {
      for (const [index, file] of agent.files.entries()) {
        assert.ok(path.relative(root, file) && !path.relative(root, file).startsWith(".."), "Agent path escaped sandbox");
        const bytes = baseline(agent.id, index);
        await write(file, bytes); originals.set(file, bytes);
      }
    }
    const configBytes = await fs.readFile(gateway.config);
    browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_EXECUTABLE_PATH ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH } : {}) });
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    const page = await context.newPage();
    page.setDefaultTimeout(5000);
    const pageErrors = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    const login = async () => {
      await page.locator("#token").fill(TOKEN);
      await page.locator("#connect").click();
      await page.locator("#console").waitFor({ state: "visible" });
      await page.locator("#auto-refresh").uncheck();
    };
    const manage = async id => { await page.getByRole("button", { name: "Manage " + names[id], exact: true }).click(); };
    const card = id => page.locator(".agent-card").filter({ has: page.getByRole("button", { name: "Manage " + names[id], exact: true }) });
    const preview = async () => {
      await page.locator("#agent-preview-button").click();
      await page.locator("#agent-preview").waitFor({ state: "visible" });
      await page.waitForFunction(() => !document.getElementById("agent-apply").disabled);
      assert.equal(await page.locator("#agent-preview-title").evaluate(node => node === document.activeElement), true);
    };
    const apply = async message => {
      await page.locator("#agent-apply").click();
      await page.waitForFunction(expected => document.getElementById("agent-message").textContent.includes(expected), message);
      await page.waitForFunction(() => !document.getElementById("agent-refresh").disabled);
    };
    const disconnectAgent = async id => {
      await manage(id);
      await page.locator("#agent-action").selectOption("disconnect");
      assert.equal(await page.locator("#agent-model-field").isVisible(), false);
      await preview(); await apply("Managed settings restored; unrelated edits preserved.");
      for (const file of statuses.find(agent => agent.id === id).files) assert.deepEqual(await fs.readFile(file), originals.get(file));
    };
    const safeDom = async () => {
      const visible = await page.locator("body").innerText();
      const values = await page.locator("input, textarea").evaluateAll(nodes => nodes.map(node => node.value).join("\n"));
      for (const secret of [TOKEN, KEY, ORIGINAL_SECRET]) {
        assert.equal(visible.includes(secret), false, "Secret rendered in visible text");
        assert.equal(values.includes(secret), false, "Secret retained in a form");
      }
    };
    await page.goto(gateway.url);
    await login();

    await t.test("seven Agents preview, connect, rewire and restore exact original bytes", async () => {
      assert.equal(await page.locator(".agent-card").count(), 7);
      assert.deepEqual(await page.locator("#agent-model-options option").evaluateAll(nodes => nodes.map(node => node.value)), ["mock/alpha", "mock/beta", "group/daily"]);
      for (const id of Object.keys(names)) {
        await manage(id); await page.locator("#agent-model").fill("mock/alpha"); await preview();
        const files = statuses.find(agent => agent.id === id).files;
        assert.equal(await page.locator("#agent-preview-files li").count(), files.length);
        for (const file of files) assert.deepEqual(await fs.readFile(file), originals.get(file), "Preview wrote an Agent file");
        await safeDom();
        if (id === "claude" && process.env.MAGPIE_LITE_SCREENSHOTS) {
          await fs.mkdir(process.env.MAGPIE_LITE_SCREENSHOTS, { recursive: true });
          await page.locator(".agents-panel").screenshot({ path: path.join(process.env.MAGPIE_LITE_SCREENSHOTS, "agents-desktop.png") });
        }
        await apply("Connected to mock/alpha.");
        assert.equal((await api("/agents")).agents.find(agent => agent.id === id).model, "mock/alpha");
        await page.locator("#agent-model").fill("group/daily");
        assert.equal(await page.locator("#agent-preview").isVisible(), false);
        await preview(); await apply("Connected to group/daily.");
        await disconnectAgent(id);
        assert.match(await card(id).innerText(), /Not connected/);
      }
      assert.deepEqual(await fs.readFile(gateway.config), configBytes, "Agent operations rewrote gateway config");
      assert.equal((await api("/stats")).total_requests, 0, "Agent management created inference usage");
    });

    await t.test("missing files can be created and restoration removes only those files", async () => {
      for (const id of Object.keys(names)) {
        const files = statuses.find(agent => agent.id === id).files;
        for (const file of files) await fs.unlink(file);
        await manage(id); await page.locator("#agent-model").fill("mock/alpha"); await preview();
        assert.ok((await page.locator("#agent-preview-files").innerText()).includes("create"));
        for (const file of files) await assert.rejects(fs.readFile(file), { code: "ENOENT" });
        await apply("Connected to mock/alpha.");
        await page.locator("#agent-action").selectOption("disconnect");
        await preview(); await apply("Managed settings restored; unrelated edits preserved.");
        for (const file of files) {
          await assert.rejects(fs.readFile(file), { code: "ENOENT" });
          await write(file, originals.get(file));
        }
      }
    });

    await t.test("seven Agents preserve unrelated edits through stale preview, rewire and disconnect", async () => {
      for (const id of Object.keys(names)) {
        const files = statuses.find(agent => agent.id === id).files;
        await manage(id); await page.locator("#agent-model").fill("mock/alpha");
        await preview(); await apply("Connected to mock/alpha.");
        for (const [index, file] of files.entries()) {
          const current = await fs.readFile(file, "utf8");
          let edited;
          if (id === "codex") edited = "user_note = 'BROWSER_USER_BEFORE'\r\n" + current;
          else if (id === "goose" && index === 0) edited = "user_note: BROWSER_USER_BEFORE\r\n" + current;
          else if (id === "gemini" && index === 1) edited = current + "# later user comment\r\nUSER_NOTE=BROWSER_USER_BEFORE\r\n";
          else {
            const value = JSON.parse(current);
            value.user_note = "BROWSER_USER_BEFORE";
            value.custom = "user-updated";
            delete value["original-secret"];
            edited = JSON.stringify(value, null, 2);
          }
          await write(file, edited);
        }
        await page.locator("#agent-refresh").click();
        await page.waitForFunction(() => !document.getElementById("agent-refresh").disabled);
        assert.equal((await api("/agents")).agents.find(agent => agent.id === id).drifted, false);
        assert.equal((await card(id).innerText()).includes("Configuration drift"), false);
        await page.locator("#agent-model").fill("mock/beta"); await preview();
        for (const file of files) await write(file, (await fs.readFile(file, "utf8")).replaceAll("BROWSER_USER_BEFORE", "BROWSER_USER_AFTER"));
        const snapshots = await Promise.all(files.map(file => fs.readFile(file)));
        await page.locator("#agent-apply").click();
        await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("Preview again before retrying"));
        for (const [index, file] of files.entries()) assert.deepEqual(await fs.readFile(file), snapshots[index]);
        assert.equal(await page.locator("#agent-preview").isVisible(), false);
        await preview(); await apply("Connected to mock/beta.");
        await page.locator("#agent-action").selectOption("disconnect");
        await preview(); await apply("Managed settings restored; unrelated edits preserved.");
        for (const [index, file] of files.entries()) {
          const restored = await fs.readFile(file, "utf8");
          assert.ok(restored.includes("BROWSER_USER_AFTER"), id + " lost file " + index);
          assert.equal(restored.includes(TOKEN), false);
          if (!(id === "codex" || (id === "goose" && index === 0) || (id === "gemini" && index === 1))) {
            const value = JSON.parse(restored);
            assert.equal(value.custom, "user-updated");
            assert.equal(Object.hasOwn(value, "original-secret"), false);
          }
          await write(file, originals.get(file));
        }
        await safeDom();
      }
      assert.deepEqual(await fs.readFile(gateway.config), configBytes);
      assert.equal((await api("/stats")).total_requests, 0);
    });

    await t.test("ambiguous JSON is rejected without writes or rendering private keys", async () => {
      const file = statuses.find(agent => agent.id === "claude").files[0];
      const duplicate = Buffer.from('{"' + KEY + '":1,"' + KEY + '":2}');
      await write(file, duplicate);
      await manage("claude"); await page.locator("#agent-model").fill("mock/alpha");
      await page.locator("#agent-preview-button").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("could not parse"));
      assert.deepEqual(await fs.readFile(file), duplicate);
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      await safeDom();
      await write(file, originals.get(file));
      await preview(); await apply("Connected to mock/alpha.");
      const applied = await fs.readFile(file);
      const text = applied.toString("utf8");
      const end = text.lastIndexOf("}");
      const ambiguous = text.slice(0, end) + ',"foreign":{"x":1,"x":2}' + text.slice(end);
      await write(file, ambiguous);
      await page.locator("#agent-refresh").click();
      await page.waitForFunction(() => document.querySelector(".agent-card").textContent.includes("Configuration drift"));
      await page.locator("#agent-action").selectOption("disconnect");
      await page.locator("#agent-preview-button").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("cannot be parsed"));
      assert.equal(await fs.readFile(file, "utf8"), ambiguous);
      await safeDom();
      await write(file, applied);
      await preview(); await apply("Managed settings restored; unrelated edits preserved.");
      assert.deepEqual(await fs.readFile(file), originals.get(file));
    });

    await t.test("input/action changes invalidate previews, Escape clears and returns focus", async () => {
      await manage("claude"); await preview();
      await page.evaluate(() => update(false));
      assert.equal(await page.locator("#agent-preview").isVisible(), true, "Unchanged refresh discarded a valid preview");
      await page.locator("#agent-model").fill("mock/beta");
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      assert.equal(await page.locator("#agent-apply").isDisabled(), true);
      await preview(); await page.keyboard.press("Escape");
      assert.equal(await page.locator("#agent-editor").isVisible(), false);
      assert.equal(await page.getByRole("button", { name: "Manage Claude Code", exact: true }).evaluate(node => node === document.activeElement), true);
      await manage("claude");
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      await page.locator("#agent-model").fill("unknown/model");
      await page.locator("#agent-preview-button").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("does not resolve"));
      assert.equal(await page.locator("#agent-model").inputValue(), "unknown/model");
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      await page.locator("#agent-model").fill("mock/alpha");
    });

    await t.test("stale previews cannot write files and connected drift is never overwritten", async () => {
      const file = statuses.find(agent => agent.id === "claude").files[0];
      await preview();
      const edited = Buffer.from('{"user":"edited after preview"}\n');
      await write(file, edited);
      await page.locator("#agent-apply").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("Preview again before retrying"));
      assert.deepEqual(await fs.readFile(file), edited);
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      await preview(); await apply("Connected to mock/alpha.");
      const applied = await fs.readFile(file);
      await write(file, '{"user":"later drift"}\n');
      await page.locator("#agent-refresh").click();
      await page.waitForFunction(() => document.querySelector(".agent-card").textContent.includes("Configuration drift"));
      await page.locator("#agent-action").selectOption("disconnect");
      await page.locator("#agent-preview-button").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("configuration drifted"));
      assert.deepEqual(await fs.readFile(file), Buffer.from('{"user":"later drift"}\n'));
      await write(file, applied);
      await preview(); await apply("Managed settings restored; unrelated edits preserved.");
      assert.deepEqual(await fs.readFile(file), edited);
      await write(file, originals.get(file));
    });

    await t.test("OpenCode overriding JSONC warns but original-path restoration remains available", async () => {
      const file = statuses.find(agent => agent.id === "opencode").files[0];
      await manage("opencode"); await page.locator("#agent-model").fill("mock/alpha");
      await preview(); await apply("Connected to mock/alpha.");
      const override = path.join(path.dirname(file), "opencode.jsonc");
      const overrideBytes = Buffer.from("// user override\n{}\n");
      await write(override, overrideBytes);
      await page.locator("#agent-refresh").click();
      await page.waitForFunction(() => [...document.querySelectorAll(".agent-card")].some(node => node.textContent.includes("OpenCode") && node.textContent.includes("Configuration drift")));
      await disconnectAgent("opencode");
      assert.deepEqual(await fs.readFile(override), overrideBytes);
      await fs.unlink(override);
    });

    await t.test("recovery requires explicit confirmation and restores a synthetic pending disconnect", async () => {
      await manage("claude"); await page.locator("#agent-model").fill("mock/alpha");
      await preview(); await apply("Connected to mock/alpha.");
      const files = statuses.find(agent => agent.id === "claude").files;
      const edited = JSON.parse(await fs.readFile(files[0], "utf8"));
      edited.recovery_note = "BROWSER_RECOVERY_USER";
      await write(files[0], JSON.stringify(edited));
      await page.locator("#agent-model").fill("mock/beta");
      await preview(); await apply("Connected to mock/beta.");
      const journalPath = path.join(root, "agent-state", "claude.json");
      const journal = JSON.parse(await fs.readFile(journalPath, "utf8"));
      journal.phase = "disconnecting";
      journal.rollback = journal.files.map(file => file.applied);
      journal.restore = journal.files.map(file => file.restore_base);
      await write(journalPath, JSON.stringify(journal));
      await page.locator("#agent-refresh").click();
      await page.locator("#agent-recover").waitFor({ state: "visible" });
      assert.equal(await page.locator("#agent-preview-button").isVisible(), false);
      assert.equal(await page.locator("#agent-action").isDisabled(), true);
      page.once("dialog", dialog => dialog.dismiss());
      await page.locator("#agent-recover").click();
      assert.equal(JSON.parse(await fs.readFile(journalPath, "utf8")).phase, "disconnecting");
      page.once("dialog", dialog => dialog.accept());
      await page.locator("#agent-recover").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("Recovery complete."));
      const expected = JSON.parse(originals.get(files[0]).toString("utf8"));
      expected.recovery_note = "BROWSER_RECOVERY_USER";
      assert.deepEqual(JSON.parse(await fs.readFile(files[0], "utf8")), expected);
      assert.equal(await page.locator("#agent-recover").isVisible(), false);
      await safeDom();
      await write(files[0], originals.get(files[0]));
    });

    await t.test("busy operations lock all conflicting workflows and route edits clear previews", async () => {
      await manage("claude");
      let release, started;
      const held = new Promise(resolve => { release = resolve; });
      const arrived = new Promise(resolve => { started = resolve; });
      await page.route("**/agents/claude/preview", async route => {
        const response = await route.fetch(); started(); await held; await route.fulfill({ response });
      });
      await page.locator("#agent-preview-button").click(); await arrived;
      for (const id of ["agent-action", "agent-model", "agent-cancel", "agent-refresh", "provider-new", "group-new", "refresh"]) assert.equal(await page.locator("#" + id).isDisabled(), true, id + " remained active");
      assert.equal(await page.locator("#agent-editor").getAttribute("aria-busy"), "true");
      await page.keyboard.press("Escape"); assert.equal(await page.locator("#agent-editor").isVisible(), true);
      release(); await page.locator("#agent-preview").waitFor({ state: "visible" });
      await page.unroute("**/agents/claude/preview");
      await api("/config/groups/daily", { id: "daily", name: "Edited", routes: ["mock/beta", "mock/alpha"] }, "PUT");
      await page.locator("#agent-refresh").click();
      await page.locator("#agent-preview").waitFor({ state: "hidden" });
      await page.locator("#provider-new").click();
      assert.equal(await page.locator("#agent-editor").isVisible(), false);
      await page.locator("#provider-keys").fill("unsaved-private-key");
      await manage("claude");
      assert.equal(await page.locator("#provider-editor").isVisible(), false);
      assert.equal(await page.locator("#provider-keys").inputValue(), "");
      // Conversely, a provider tool must also block Agent operations. The response
      // is a fixture: this is UI coordination evidence, not vendor conformance.
      await page.locator("#provider-new").click();
      await page.locator("#provider-id").fill("draft");
      await page.locator("#provider-url").fill("http://127.0.0.1:9/v1");
      await page.locator("#provider-key-mode").selectOption("clear");
      let releaseTool, startTool;
      const toolHeld = new Promise(resolve => { releaseTool = resolve; });
      const toolArrived = new Promise(resolve => { startTool = resolve; });
      await page.route("**/config/provider-tools/models", async route => {
        startTool(); await toolHeld;
        await route.fulfill({ status: 200, json: { models: ["draft-model"] } });
      });
      await page.locator("#provider-discover").click(); await toolArrived;
      assert.equal(await page.locator("#agent-refresh").isDisabled(), true);
      assert.equal(await page.getByRole("button", { name: "Manage Claude Code", exact: true }).isDisabled(), true);
      releaseTool();
      await page.waitForFunction(() => document.getElementById("provider-message").textContent.includes("models discovered"));
      await page.unroute("**/config/provider-tools/models");
      await page.locator("#provider-cancel").click();
    });

    await t.test("provider and group CRUD still persist safely and update Agent route suggestions", async () => {
      await page.locator("#provider-new").click();
      await page.locator("#provider-id").fill("extra");
      await page.locator("#provider-protocol").selectOption("responses");
      await page.locator("#provider-url").fill("http://127.0.0.1:9/v1");
      await page.locator("#provider-key-mode").selectOption("clear");
      await page.locator("#provider-models").fill("extra-model");
      await page.locator("#provider-save").click();
      await page.waitForFunction(() => document.getElementById("provider-message").textContent.includes("Provider saved."));
      await page.waitForFunction(() => [...document.querySelectorAll("#agent-model-options option")].some(node => node.value === "extra/extra-model"));
      await page.getByRole("button", { name: "Edit provider extra", exact: true }).click();
      assert.equal(await page.locator("#provider-id").getAttribute("readonly"), "");
      await page.locator("#provider-name").fill("Edited extra");
      await page.locator("#provider-models").fill("edited-model");
      await page.locator("#provider-key-mode").selectOption("replace");
      await page.locator("#provider-keys").fill("replacement-sandbox-private-key");
      await page.locator("#provider-save").click();
      await page.waitForFunction(() => document.getElementById("provider-editor").hidden);
      assert.equal(await page.locator("#provider-keys").inputValue(), "");
      const configuration = await api("/config");
      assert.equal(configuration.providers.find(provider => provider.id === "extra").credential_count, 1);
      assert.equal(JSON.stringify(configuration).includes("replacement-sandbox-private-key"), false);
      assert.ok((await fs.readFile(gateway.config, "utf8")).includes("replacement-sandbox-private-key"));
      await page.locator("#group-new").click();
      await page.locator("#group-id").fill("extra-group");
      await page.locator("#group-routes").fill("extra/edited-model\nmock/alpha");
      await page.locator("#group-save").click();
      await page.waitForFunction(() => document.getElementById("group-message").textContent.includes("Group saved."));
      await page.waitForFunction(() => [...document.querySelectorAll("#agent-model-options option")].some(node => node.value === "group/extra-group"));
      page.once("dialog", dialog => dialog.accept());
      await page.getByRole("button", { name: "Delete provider extra", exact: true }).click();
      await page.waitForFunction(() => !document.getElementById("provider-new").disabled);
      assert.ok((await api("/config")).providers.some(provider => provider.id === "extra"), "Referenced provider was deleted");
      page.once("dialog", dialog => dialog.accept());
      await page.getByRole("button", { name: "Delete group extra-group", exact: true }).click();
      await page.waitForFunction(() => document.getElementById("group-message").textContent.includes("Group deleted"));
      page.once("dialog", dialog => dialog.accept());
      await page.getByRole("button", { name: "Delete provider extra", exact: true }).click();
      await page.waitForFunction(() => document.getElementById("provider-message").textContent.includes("Provider deleted"));
      await page.waitForFunction(() => ![...document.querySelectorAll("#agent-model-options option")].some(node => node.value.startsWith("extra/") || node.value === "group/extra-group"));
      assert.equal((await page.locator("body").innerText()).includes("replacement-sandbox-private-key"), false);
    });

    await t.test("a failed Agent status read is isolated and logout rejects late old-session responses", async () => {
      let release, started, calls = 0;
      const held = new Promise(resolve => { release = resolve; });
      const arrived = new Promise(resolve => { started = resolve; });
      await page.route("**/agents", async route => {
        if (++calls === 1) { const response = await route.fetch(); started(); await held; await route.fulfill({ response }); }
        else await route.fulfill({ status: 503, json: { error: { message: "agent management is not configured" } } });
      });
      const pendingOldRefresh = page.evaluate(() => update(false));
      await arrived;
      await page.locator("#sign-out").click();
      assert.equal(await page.locator("#agent-list").textContent(), "");
      assert.equal(await page.locator("#agent-files").textContent(), "");
      assert.equal(await page.locator("#agent-model-options").textContent(), "");
      await login();
      await page.waitForFunction(() => document.getElementById("agent-load-message").textContent.includes("not configured"));
      release();
      await pendingOldRefresh; // Wait until the old update has actually processed its response.
      assert.match(await page.locator("#agent-load-message").innerText(), /not configured/);
      assert.equal(await page.locator(".agent-card").count(), 0);
      assert.equal(await page.locator("#console").isVisible(), true);
      assert.equal(await page.locator("#provider-new").isEnabled(), true);
      await page.unroute("**/agents");
      await page.locator("#agent-refresh").click();
      await page.waitForFunction(() => document.querySelectorAll(".agent-card").length === 7);
    });

    await t.test("401 during preview clears private UI and rejects the expired session", async () => {
      await manage("claude");
      await page.route("**/agents/claude/preview", route => route.fulfill({ status: 401, json: { error: { message: "unauthorized" } } }));
      await page.locator("#agent-preview-button").click();
      await page.locator("#login").waitFor({ state: "visible" });
      assert.equal(await page.locator("#agent-list").textContent(), "");
      assert.equal(await page.locator("#agent-message").textContent(), "");
      assert.equal(await page.locator("#token").inputValue(), "");
      await page.unroute("**/agents/claude/preview");
      await login();
    });

    await t.test("a bounded request timeout releases controls without applying changes", async () => {
      await manage("claude");
      const file = statuses.find(agent => agent.id === "claude").files[0];
      const before = await fs.readFile(file);
      // Accelerate only the next 30-second deadline; other timers and the
      // subsequent refresh retain their normal semantics.
      await page.evaluate(() => {
        const original = window.setTimeout;
        window.setTimeout = (callback, delay, ...args) => {
          if (delay === 30000) {
            window.setTimeout = original;
            return original(callback, 50, ...args);
          }
          return original(callback, delay, ...args);
        };
      });
      let release;
      const held = new Promise(resolve => { release = resolve; });
      await page.route("**/agents/claude/preview", async route => {
        await held;
        await route.abort().catch(() => {}); // The browser may already have aborted it.
      });
      await page.locator("#agent-preview-button").click();
      await page.waitForFunction(() => document.getElementById("agent-message").textContent.includes("request timed out"));
      release();
      await page.unroute("**/agents/claude/preview");
      assert.equal(await page.locator("#agent-preview").isVisible(), false);
      assert.equal(await page.locator("#agent-preview-button").isEnabled(), true);
      assert.equal(await page.locator("#provider-new").isEnabled(), true);
      assert.deepEqual(await fs.readFile(file), before);
    });

    await t.test("failed initial load never opens the console, and refresh can recover", async () => {
      await page.locator("#sign-out").click();
      await page.route("**/config", route => route.fulfill({ status: 503, json: { error: { message: "configuration unavailable" } } }));
      await page.locator("#token").fill(TOKEN); await page.locator("#connect").click();
      await page.waitForFunction(() => document.getElementById("login-message").textContent.includes("configuration unavailable"));
      assert.equal(await page.locator("#console").isVisible(), false);
      assert.equal(await page.locator("#connect").isEnabled(), true);
      await page.unroute("**/config");
      await login();
    });

    await t.test("390px layout, visible focus, 44px controls and secret-free rendering", async () => {
      await page.setViewportSize({ width: 390, height: 844 });
      await manage("claude"); await page.locator("#agent-model").fill("mock/alpha"); await preview();
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true, "Page horizontally overflows");
      for (const selector of ["#agent-model", "#agent-action", "#agent-apply", "#agent-cancel", "#agent-refresh", ".agent-card .button"]) {
        for (const box of await page.locator(selector).evaluateAll(nodes => nodes.map(node => ({ width: node.getBoundingClientRect().width, height: node.getBoundingClientRect().height })))) {
          assert.ok(box.width >= 44 && box.height >= 44, selector + " is too small");
        }
      }
      await page.locator("#agent-cancel").focus();
      const focus = await page.locator("#agent-cancel").evaluate(node => getComputedStyle(node).outlineWidth);
      assert.notEqual(focus, "0px");
      await safeDom();
      if (process.env.MAGPIE_LITE_SCREENSHOTS) await page.locator("#agent-editor").screenshot({ path: path.join(process.env.MAGPIE_LITE_SCREENSHOTS, "agents-mobile.png") });
      await page.keyboard.press("Escape");
      await page.getByRole("button", { name: "Manage Claude Code", exact: true }).focus();
      await page.evaluate(() => update(false));
      assert.equal(await page.getByRole("button", { name: "Manage Claude Code", exact: true }).evaluate(node => node === document.activeElement), true);
      assert.equal(await page.getByRole("button", { name: "Manage Claude Code", exact: true }).isEnabled(), true);
      assert.deepEqual(pageErrors, [], "Browser runtime errors");
      await safeDom();
    });
  } finally {
    if (browser) await browser.close();
    await stop(gateway?.child);
    // Only remove the exact mkdtemp-created sandbox after stopping the child.
    assert.equal(path.dirname(root), os.tmpdir());
    assert.ok(path.basename(root).startsWith("magpie-console-"));
    await fs.rm(root, { recursive: true, force: true });
  }
});
