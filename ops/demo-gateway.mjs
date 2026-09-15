#!/usr/bin/env bun
// Found the demo user's Identity account and publish authenticated gateway
// routes on the running demo node:
//   site — network-hosted static bytes in DuckFS
//   app  — user-hosted loopback HTTP
//   board — the reference kanban app (network audience, WebSocket)
// Every gateway op is a USER-signed frame over /v1/submit/frame: the frame's
// signer is the op origin, which the gateway resolves to the account through
// identity `OfKey`. The frameless /v1/submit lane stamps the node key, and a
// node key never resolves to an account.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

// Consensus rejects these handles — a seed that asks for one aborts the demo.
// Mirrors RESERVED_ROOT_LABELS in crates/duckdns/src/wire.rs (the source
// of truth); app/src/domain/duckdns-client.test.ts pins all three copies.
const RESERVED_ROOT_LABELS = ["net", "agents"];

const INDEX_HTML = `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Ducktape DVD</title><style>
  :root{color-scheme:dark}
  html,body{margin:0;height:100%;background:#05070a;overflow:hidden}
  .stage{position:fixed;inset:0}
  .dvd{position:absolute;top:0;left:0;box-sizing:border-box;width:210px;height:70px;
       display:flex;align-items:center;justify-content:center;gap:.45rem;border-radius:16px;
       font:800 28px/1 system-ui,sans-serif;color:#fff;white-space:nowrap;user-select:none;
       background:radial-gradient(130% 150% at 28% 18%,#7c5cff,#00d4ff);
       box-shadow:0 0 26px rgba(120,150,255,.75),inset 0 0 0 2px rgba(255,255,255,.28);
       animation:mx 6.1s linear infinite alternate, my 8.3s linear infinite alternate, hue 5s linear infinite;
       will-change:left,top,filter}
  .dvd .duck{font-size:34px}
  .dvd small{font-weight:700;font-size:15px;opacity:.9;letter-spacing:.18em}
  @keyframes mx{from{left:0}to{left:calc(100vw - 210px)}}
  @keyframes my{from{top:0}to{top:calc(100vh - 70px)}}
  @keyframes hue{from{filter:hue-rotate(0deg)}to{filter:hue-rotate(360deg)}}
  .tag{position:fixed;left:0;right:0;bottom:16px;text-align:center;
       font:12px/1 system-ui,sans-serif;color:#59616f;letter-spacing:.09em}
  @media (prefers-reduced-motion:reduce){
    .dvd{animation:hue 5s linear infinite;left:calc(50vw - 105px);top:calc(50vh - 35px)}}
</style></head>
<body>
  <div class="stage"><div class="dvd"><span class="duck">🦆</span>DUCK<small>DVD</small></div></div>
  <div class="tag">network-hosted &middot; served from DuckFS by consensus &middot; make demo-seed</div>
</body></html>
`;

async function main() {
  const [url, nodeBin, workdir, chain, requestedHandle = "demo", userKeyArg, password = ""] = Bun.argv.slice(2);
  if (!url || !nodeBin || !workdir || !chain) {
    throw new Error("usage: demo-gateway.mjs <http-url> <node-bin> <workdir> <chain-id> [handle] <user-key> [password]");
  }
  // The signing key is the local user identity demo-seed provisioned
  // (encrypted) — its password unlocks each sign-* call over stdin.
  if (!userKeyArg) {
    throw new Error("usage: demo-gateway.mjs <http-url> <node-bin> <workdir> <chain-id> [handle] <user-key> [password]");
  }
  const userKey = userKeyArg;
  // This boot's operator credential. `/v1/files/commit` is a MUTATING route
  // and the write gate (crates/noded/src/signed_req.rs) refuses it without
  // one; `/v1/query` below is a read and does not need it, but one helper
  // serves both. Read once — the file is minted per boot and demo-seed.sh
  // has already failed fatally if it is missing.
  const operator = readFileSync(`${workdir}/admin.token`, "utf8").trim();

  async function post(path, body) {
    const response = await fetch(`${url}${path}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-ducktape-admin-token": operator,
      },
      body: JSON.stringify(body),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`[gateway] POST ${path} failed [${response.status}]: ${text.slice(0, 400)}`);
    return JSON.parse(text);
  }

  const query = (target, query) => post("/v1/query", { target, query });
  // A user-signed frame, raw bytes to /v1/submit/frame — the frame's verified
  // signer becomes the op's origin (see node_http::submit_frame).
  async function submitFrame(frameHex) {
    const response = await fetch(`${url}/v1/submit/frame`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: Buffer.from(frameHex, "hex"),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`[gateway] POST /v1/submit/frame failed [${response.status}]: ${text.slice(0, 400)}`);
    return JSON.parse(text);
  }
  // Every signing verb unlocks the encrypted key by reading its password as
  // the first stdin line (see load_user_signer); `lines` follow it. Returns
  // stdout's non-empty lines.
  function run(args, lines = []) {
    const input = [password, ...lines].map((line) => `${line}\n`).join("");
    const result = spawnSync(nodeBin, args, { encoding: "utf8", input });
    if (result.status !== 0) {
      const detail = (result.stderr || result.stdout || result.error?.message || "unknown error").trim();
      throw new Error(`[gateway] ${args.slice(0, 2).join(" ")} failed: ${detail}`);
    }
    return result.stdout.trim().split(/\r?\n/).filter(Boolean);
  }
  const sign = (args) => run(args).at(-1);
  // Wrap already-encoded module payloads in frames the user key signs — ONE
  // process for the whole batch, because the unlock is one argon2id pass and
  // the requests are per op. Returns the frame hex lines in request order.
  function signFrames(requests) {
    const base = BigInt(Date.now()) * 1000n;
    const lines = requests.map(({ target, payload }, i) =>
      `${target} ${base + BigInt(i)} ${Buffer.from(JSON.stringify(payload)).toString("hex")}`);
    const frames = run(["user", "sign-frame", "--key", userKey], lines);
    if (frames.length !== lines.length) throw new Error(`[gateway] sign-frame returned ${frames.length} frames for ${lines.length} requests`);
    return frames;
  }

  const nodeBytes = (await query("valset", "validators")).validators[0];
  const nodeHex = Buffer.from(nodeBytes).toString("hex");

  // Reuse the wallet account that owns the model user. Standalone gateway
  // seeding may receive a fresh wallet, so only that case needs Create.
  const userPubHex = sign(["user", "key", "status", "--key", userKey]).split(" ").at(-1);
  const ownerQuery = { of_key: { key: [...Buffer.from(userPubHex, "hex")] } };
  let account = (await query("identity", ownerQuery)).account;
  if (!account) {
    run(["account", "create", "--name", requestedHandle, "--key", userKey, "--node", url]);
    account = (await query("identity", ownerQuery)).account;
  }
  if (!account) throw new Error("[gateway] the demo key founded no account");
  const accountId = account.number;
  console.log(`[gateway] account ${accountId} (${account.name}) founded by the demo key`);

  // Gateway ops, in order: the optional .duck handle, then the routes. All
  // are signed into frames by the same user key below.
  const ops = [];
  let handle = requestedHandle;
  if (/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(handle) && !RESERVED_ROOT_LABELS.includes(handle)) {
    ops.push({ target: "gateway", payload: { set_handle: { handle } } });
  } else {
    handle = null;
    console.log("[gateway] skipped .duck handle (workspace id is not a legal DNS label)");
  }

  const data = Buffer.from(INDEX_HTML);
  // The file table lives off consensus in .manifest.json; the signed route
  // binds only the manifest's SHA-256.
  const manifest = {
    default_path: "index.html",
    files: [{
      path: "index.html",
      mime: "text/html",
      size: data.byteLength,
      sha256: createHash("sha256").update(data).digest("hex"),
    }],
  };
  const manifestBytes = Buffer.from(JSON.stringify(manifest));
  const manifestSha256 = createHash("sha256").update(manifestBytes).digest("hex");
  const put = (path, bytes) => ({
    put: {
      path: `/home/ext:${nodeHex}/.duck/gateway/site/${path}`,
      exec: false,
      meta: {},
      content: { inline: { b64: bytes.toString("base64") } },
    },
  });
  await post("/v1/files/commit", {
    base_snapshot: null,
    message: "seed: gateway site + manifest",
    changes: [put("index.html", data), put(".manifest.json", manifestBytes)],
  });

  // The route statement is signed by a member key of its account (the
  // canonical preimage under GATEWAY_ROUTE_NS); the resulting SetRoute rides
  // a frame the same key signs.
  function publish(statement) {
    const message = JSON.parse(sign([
      "user",
      "sign-gateway-route",
      "--key", userKey,
      "--statement", JSON.stringify(statement),
    ]));
    ops.push({ target: "gateway", payload: message });
  }

  const site = {
    chain_id: chain,
    account_id: accountId,
    name: { label: "site" },
    publisher_node: nodeBytes,
    revision: 1,
    route: {
      target: { kind: "duck_fs", manifest_sha256: manifestSha256 },
      policy: {
        audience: { kind: "owner" },
        methods: ["get", "head"],
        max_request_bytes: 0,
        max_response_bytes: 1 << 20,
        allow_authorization: false,
        allow_upgrade: false,
      },
    },
  };
  // A stale embedded gateway component is a build error, not a condition to
  // survive: a publish rejection fails the demo loudly.
  publish(site);

  publish({
    chain_id: chain,
    account_id: accountId,
    name: { label: "app" },
    publisher_node: nodeBytes,
    revision: 1,
    route: {
      target: { kind: "loopback_http" },
      policy: {
        audience: { kind: "owner" },
        methods: ["get", "head", "post"],
        max_request_bytes: 1 << 20,
        max_response_bytes: 1 << 20,
        allow_authorization: true,
        allow_upgrade: true,
      },
    },
  });

  // The reference app (spec §8): board.<handle>.duck — served by
  // ops/demo-kanban.mjs, reachable by any admitted member, WebSocket-realtime.
  publish({
    chain_id: chain,
    account_id: accountId,
    name: { label: "board" },
    publisher_node: nodeBytes,
    revision: 1,
    route: {
      target: { kind: "loopback_http" },
      policy: {
        audience: { kind: "network" },
        methods: ["get", "head", "post"],
        max_request_bytes: 1 << 20,
        max_response_bytes: 1 << 20,
        allow_authorization: false,
        allow_upgrade: true,
      },
    },
  });

  for (const frame of signFrames(ops)) {
    await submitFrame(frame);
  }

  const routes = await query("gateway", { list: { account_id: accountId } });
  const count = routes.routes?.length ?? 0;
  if (handle) {
    console.log(`[gateway] published ${count} routes on ${handle}.duck: site.${handle}.duck (static), app.${handle}.duck (loopback), board.${handle}.duck (kanban, WS)`);
  } else {
    console.log(`[gateway] published ${count} routes on account ${accountId} (no .duck handle — reach them via the Gateway view)`);
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
