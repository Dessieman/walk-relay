// Walk Relay gatekeeper.
//
// The only thing that can touch the group's R2 bucket. Apps authenticate with a
// personal token (never shared in links); friends join with short, single-use,
// expiring invites. The Worker only allows Walk Relay's own save files, caps
// their size, and never lets clients delete history.

const MAX_OBJECT_BYTES = 25 * 1024 * 1024;
const INVITE_TTL_MS = 48 * 60 * 60 * 1000;
const MAX_OPEN_INVITES = 20;
const HISTORY_KEEP = 20;
const BOOTSTRAP_MAX_AGE_MS = 10 * 60 * 1000;
const AUTH_CACHE_MS = 60 * 1000;
const INVITE_ALPHABET = "23456789ABCDEFGHJKMNPQRSTVWXYZ"; // no 0/O, 1/I/L, U
const INVITE_RE = /^[23456789ABCDEFGHJKMNPQRSTVWXYZ]{8}$/;

// Objects a client may read/write, and what they may list.
const OBJECT_RE =
  /^v1\/(journeys\/[0-9a-f]+\.zip|(history|conflicts)\/[0-9a-f]+\/\d{13}-[0-9a-f]+\.zip|presence\/[0-9a-f]+\.json)$/;
const LIST_RE = /^v1\/(journeys|history|conflicts|presence)\/[0-9a-f/-]*(\.(zip|json))?$/;
const HISTORY_KEY_RE = /^(v1\/history\/[0-9a-f]+\/)\d{13}-[0-9a-f]+\.zip$/;

const authCache = new Map(); // token hash -> { member, until }

export default {
  async fetch(request, env) {
    try {
      return await route(request, env);
    } catch (e) {
      return json({ error: "internal error" }, 500);
    }
  },
};

async function route(request, env) {
  const url = new URL(request.url);
  const path = url.pathname;
  const method = request.method;

  // ---- public
  if (method === "GET" && (path === "/" || path === "/health")) return json({ ok: true, app: "walk-relay" });
  if (method === "GET" && path.startsWith("/j/")) return invitePage(url, path.slice(3));
  if (method === "POST" && path === "/redeem") return redeem(request, env);
  if (method === "POST" && path === "/setup") return setup(request, env);

  // ---- members only
  const member = await authenticate(request, env);
  if (!member) return json({ error: "not a member of this group (or removed)" }, 401);

  if (method === "GET" && path === "/me") return json(publicMember(member));
  if (method === "POST" && path === "/invites") return createInvite(env, member, url);
  if (method === "GET" && path === "/members") return listMembers(env);
  const rm = path.match(/^\/members\/([0-9a-f]{12})\/remove$/);
  if (method === "POST" && rm) return removeMember(env, member, rm[1]);

  if (method === "GET" && path === "/l") return listObjects(env, url.searchParams.get("prefix") || "");
  if (path.startsWith("/o/")) {
    const key = decodeURIComponent(path.slice(3));
    if (!OBJECT_RE.test(key)) return json({ error: "not a Walk Relay file" }, 403);
    if (method === "GET") return getObject(env, key);
    if (method === "PUT") return putObject(request, env, key);
    if (method === "DELETE") return deleteObject(env, key);
  }
  return json({ error: "not found" }, 404);
}

// ------------------------------------------------------------------ helpers

function json(body, status = 200, headers = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers },
  });
}

function randomToken() {
  const b = new Uint8Array(32);
  crypto.getRandomValues(b);
  return btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function randomInviteId() {
  const b = new Uint8Array(8);
  crypto.getRandomValues(b);
  // 256 % 30 != 0, so reject the biased tail.
  let out = "";
  while (out.length < 8) {
    for (const x of b) {
      if (out.length < 8 && x < 240) out += INVITE_ALPHABET[x % 30];
    }
    crypto.getRandomValues(b);
  }
  return out;
}

async function sha256hex(text) {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, "0")).join("");
}

async function readJson(bucket, key) {
  const o = await bucket.get(key);
  if (!o) return null;
  try {
    return { value: await o.json(), etag: o.etag, uploaded: o.uploaded };
  } catch {
    return null;
  }
}

async function bodyJson(request) {
  try {
    return await request.json();
  } catch {
    return {};
  }
}

function cleanName(name) {
  const n = String(name || "").trim();
  return n.length >= 1 && n.length <= 32 ? n : null;
}

function publicMember(m) {
  return { id: m.id, name: m.name, owner: !!m.owner, created_at: m.created_at };
}

async function addMember(env, name, owner, createdBy) {
  const token = randomToken();
  const hash = await sha256hex(token);
  const member = { id: hash.slice(0, 12), name, owner, created_at: Date.now(), created_by: createdBy };
  await env.BUCKET.put(`auth/members/${hash}.json`, JSON.stringify(member));
  return { token, member };
}

async function authenticate(request, env) {
  const h = request.headers.get("authorization") || "";
  const m = h.match(/^Bearer\s+([A-Za-z0-9_-]{20,})$/);
  if (!m) return null;
  const hash = await sha256hex(m[1]);
  const cached = authCache.get(hash);
  if (cached && cached.until > Date.now()) return cached.member;
  const rec = await readJson(env.BUCKET, `auth/members/${hash}.json`);
  const member = rec ? { ...rec.value, hash } : null;
  if (member) authCache.set(hash, { member, until: Date.now() + AUTH_CACHE_MS });
  else authCache.delete(hash);
  return member;
}

// ------------------------------------------------------------------ joining

// First owner: the app proves it controls the bucket (it holds the old R2 key)
// by writing auth/bootstrap/<nonce> directly; we check and consume it.
async function setup(request, env) {
  const { nonce, name } = await bodyJson(request);
  const n = cleanName(name);
  if (!n || !/^[0-9a-f]{32}$/.test(String(nonce || ""))) return json({ error: "bad request" }, 400);
  const key = `auth/bootstrap/${nonce}`;
  const o = await env.BUCKET.head(key);
  if (!o) return json({ error: "couldn't verify you own this bucket" }, 403);
  await env.BUCKET.delete(key);
  if (Date.now() - o.uploaded.getTime() > BOOTSTRAP_MAX_AGE_MS) return json({ error: "setup took too long; try again" }, 403);
  const { token, member } = await addMember(env, n, true, "setup");
  return json({ token, member: publicMember(member) });
}

async function redeem(request, env) {
  const { invite, name } = await bodyJson(request);
  const n = cleanName(name);
  const id = String(invite || "").toUpperCase();
  if (!n || !INVITE_RE.test(id)) return json({ error: "bad request" }, 400);
  const key = `auth/invites/${id}.json`;
  const rec = await readJson(env.BUCKET, key);
  const gone = { error: "This invite link has already been used or has expired. Ask a friend for a new one." };
  if (!rec || rec.value.used || rec.value.expires_at < Date.now()) return json(gone, 410);
  // Claim it: only the first redeemer's conditional write (on the original etag) succeeds.
  const claimed = await env.BUCKET.put(key, JSON.stringify({ ...rec.value, used: true, used_by: n, used_at: Date.now() }), {
    onlyIf: { etagMatches: rec.etag },
  });
  if (!claimed) return json(gone, 410);
  const { token, member } = await addMember(env, n, false, rec.value.created_by);
  return json({ token, member: publicMember(member) });
}

async function createInvite(env, member, url) {
  // Tidy up used/expired invites, and cap how many can be open at once.
  const listed = await env.BUCKET.list({ prefix: "auth/invites/" });
  let open = 0;
  const stale = [];
  for (const o of listed.objects) {
    const rec = await readJson(env.BUCKET, o.key);
    if (!rec || rec.value.used || rec.value.expires_at < Date.now()) stale.push(o.key);
    else open++;
  }
  if (stale.length) await env.BUCKET.delete(stale);
  if (open >= MAX_OPEN_INVITES) return json({ error: "Too many unused invites are open. Wait for some to be used or expire." }, 429);

  for (let attempt = 0; attempt < 5; attempt++) {
    const id = randomInviteId();
    const expires_at = Date.now() + INVITE_TTL_MS;
    const body = JSON.stringify({ created_by: member.name, created_at: Date.now(), expires_at, used: false });
    const put = await env.BUCKET.put(`auth/invites/${id}.json`, body, { onlyIf: new Headers({ "if-none-match": "*" }) });
    if (put) return json({ id, url: `${url.origin}/j/${id}`, expires_at });
  }
  return json({ error: "couldn't create an invite; try again" }, 500);
}

async function listMembers(env) {
  const out = [];
  let cursor;
  do {
    const page = await env.BUCKET.list({ prefix: "auth/members/", cursor });
    for (const o of page.objects) {
      const rec = await readJson(env.BUCKET, o.key);
      if (rec) out.push(publicMember(rec.value));
    }
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  out.sort((a, b) => a.created_at - b.created_at);
  return json(out);
}

async function removeMember(env, member, id) {
  if (!member.owner) return json({ error: "Only the group owner can remove members." }, 403);
  if (member.id === id) return json({ error: "You can't remove yourself." }, 400);
  const page = await env.BUCKET.list({ prefix: "auth/members/" });
  for (const o of page.objects) {
    const hash = o.key.slice("auth/members/".length, -".json".length);
    if (hash.slice(0, 12) === id) {
      await env.BUCKET.delete(o.key);
      authCache.delete(hash);
      return json({ ok: true });
    }
  }
  return json({ error: "no such member" }, 404);
}

// ------------------------------------------------------------------ files

async function listObjects(env, prefix) {
  if (!LIST_RE.test(prefix)) return json({ error: "not a Walk Relay folder" }, 403);
  const out = [];
  let cursor;
  do {
    const page = await env.BUCKET.list({ prefix, cursor, limit: 1000 });
    for (const o of page.objects) out.push({ key: o.key, etag: o.etag, size: o.size });
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  return json(out);
}

async function getObject(env, key) {
  const o = await env.BUCKET.get(key);
  if (!o) return json({ error: "not found" }, 404);
  return new Response(o.body, {
    headers: { etag: `"${o.etag}"`, "content-type": "application/octet-stream", "cache-control": "no-store" },
  });
}

async function putObject(request, env, key) {
  const declared = Number(request.headers.get("content-length") || "0");
  if (declared > MAX_OBJECT_BYTES) return json({ error: "file too large" }, 413);
  const body = await request.arrayBuffer();
  if (body.byteLength > MAX_OBJECT_BYTES) return json({ error: "file too large" }, 413);

  const ifMatch = request.headers.get("if-match");
  const ifNoneMatch = request.headers.get("if-none-match");
  let onlyIf;
  if (ifMatch) onlyIf = { etagMatches: ifMatch.replace(/^W\//, "").replace(/"/g, "") };
  else if (ifNoneMatch === "*") onlyIf = new Headers({ "if-none-match": "*" });

  const put = await env.BUCKET.put(key, body, onlyIf ? { onlyIf } : undefined);
  if (!put) return json({ error: "changed by someone else" }, 412);

  const h = key.match(HISTORY_KEY_RE);
  if (h) await pruneHistory(env, h[1]);
  return json({ etag: put.etag }, 200, { etag: `"${put.etag}"` });
}

async function pruneHistory(env, prefix) {
  const page = await env.BUCKET.list({ prefix, limit: 1000 });
  const keys = page.objects.map((o) => o.key).sort();
  const excess = keys.slice(0, Math.max(0, keys.length - HISTORY_KEEP));
  if (excess.length) await env.BUCKET.delete(excess);
}

async function deleteObject(env, key) {
  if (!key.startsWith("v1/journeys/")) return json({ error: "history can't be deleted" }, 403);
  await env.BUCKET.delete(key);
  return new Response(null, { status: 204 });
}

// ------------------------------------------------------------------ invite page

function invitePage(url, id) {
  const ok = INVITE_RE.test(id.toUpperCase());
  const deep = `walkrelay://join#${url.host}/${id.toUpperCase()}`;
  const card = "https://dessieman.github.io/walk-relay/join/card.png";
  const html = `<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex"><meta name="referrer" content="no-referrer">
<title>You're invited to a Big Walk group</title>
<meta name="theme-color" content="#2e7d5b">
<meta property="og:type" content="website"><meta property="og:site_name" content="Walk Relay">
<meta property="og:title" content="You're invited to a Big Walk group">
<meta property="og:description" content="Walk Relay keeps your group's Big Walk saves in sync, so anyone can host the next walk.">
<meta property="og:image" content="${card}"><meta property="og:image:width" content="1200"><meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<style>
:root{--bg:#f6f3ea;--card:#fffdf7;--ink:#1f2a24;--muted:#6b766f;--line:#e3ddcd;--accent:#2e7d5b;--accent-ink:#fff;color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--bg:#141a17;--card:#1c2420;--ink:#e9eee9;--muted:#93a098;--line:#2c3731;--accent:#4fb386;--accent-ink:#0d1411}}
*{box-sizing:border-box}body{margin:0;padding:32px 16px;background:var(--bg);color:var(--ink);font:15px/1.5 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
main{max-width:520px;margin:0 auto}.card{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:22px;margin-bottom:14px}
h1{font-size:22px;margin:0 0 6px}h2{font-size:15px;margin:0 0 8px}p{margin:0 0 12px}.muted{color:var(--muted);font-size:13.5px}
.btn{display:inline-block;padding:11px 18px;border-radius:10px;border:1px solid var(--accent);background:var(--accent);color:var(--accent-ink);font-weight:600;text-decoration:none}
ol{margin:0 0 12px;padding-left:20px}a{color:var(--accent)}.btn{color:var(--accent-ink)}
</style></head><body><main>
${
  ok
    ? `<section class="card"><h1>You're invited to a Big Walk group</h1>
<p class="muted">Walk Relay keeps your group's Big Walk saves in sync, so anyone can host the next session.</p>
<p><a class="btn" href="${deep}">Open in Walk Relay</a></p>
<p class="muted">This invite works once and expires 48 hours after it was made.</p></section>
<section class="card"><h2>Don't have Walk Relay yet?</h2><ol>
<li><a href="https://github.com/Dessieman/walk-relay/releases/latest">Download it</a>: the <strong>.exe</strong> for Windows or the <strong>.dmg</strong> for Mac.</li>
<li>Install it and open it once.</li><li>Come back here and click <strong>Open in Walk Relay</strong>.</li></ol>
<p class="muted">If the button doesn't do anything, paste this page's address into Walk Relay's setup screen.</p></section>`
    : `<section class="card"><h1>This invite link isn't right</h1><p>Ask your friend to send a new one from Walk Relay.</p></section>`
}
</main></body></html>`;
  return new Response(html, {
    status: ok ? 200 : 404,
    headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "referrer-policy": "no-referrer" },
  });
}
