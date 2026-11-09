import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { Miniflare } from "miniflare";

let mf, bucket;
const BASE = "https://walkrelay.example.workers.dev";

before(async () => {
  mf = new Miniflare({ modules: true, scriptPath: new URL("../src/index.js", import.meta.url).pathname, r2Buckets: ["BUCKET"], compatibilityDate: "2025-06-01" });
  bucket = await mf.getR2Bucket("BUCKET");
});
after(async () => mf.dispose());

const req = (path, opts = {}) => mf.dispatchFetch(BASE + path, opts);
const auth = (token, extra = {}) => ({ authorization: `Bearer ${token}`, ...extra });

async function owner(name = "Chris") {
  const nonce = [...crypto.getRandomValues(new Uint8Array(16))].map((x) => x.toString(16).padStart(2, "0")).join("");
  await bucket.put(`auth/bootstrap/${nonce}`, ""); // what the app does with the old R2 key
  const r = await req("/setup", { method: "POST", body: JSON.stringify({ nonce, name }) });
  assert.equal(r.status, 200);
  return (await r.json()).token;
}
async function invite(token) {
  const r = await req("/invites", { method: "POST", headers: auth(token) });
  assert.equal(r.status, 200);
  return r.json();
}
async function redeem(id, name) {
  return req("/redeem", { method: "POST", body: JSON.stringify({ invite: id, name }) });
}

test("setup needs proof of bucket ownership, and the proof is single-use", async () => {
  const bad = await req("/setup", { method: "POST", body: JSON.stringify({ nonce: "0".repeat(32), name: "Eve" }) });
  assert.equal(bad.status, 403);
  const t = await owner();
  const me = await (await req("/me", { headers: auth(t) })).json();
  assert.equal(me.name, "Chris");
  assert.equal(me.owner, true);
});

test("invites are short, work exactly once, and give a personal token", async () => {
  const t = await owner();
  const inv = await invite(t);
  assert.match(inv.id, /^[23456789ABCDEFGHJKMNPQRSTVWXYZ]{8}$/);
  assert.equal(inv.url, `${BASE}/j/${inv.id}`);
  const r1 = await redeem(inv.id, "Dakota");
  assert.equal(r1.status, 200);
  const { token, member } = await r1.json();
  assert.equal(member.owner, false);
  assert.equal((await redeem(inv.id, "Eve")).status, 410, "second use must fail");
  assert.equal((await req("/me", { headers: auth(token) })).status, 200);
  // lowercase invite ids are accepted too (people retype them)
  const inv2 = await invite(t);
  assert.equal((await redeem(inv2.id.toLowerCase(), "Sam")).status, 200);
});

test("simultaneous redemptions of one invite: only one wins", async () => {
  const t = await owner();
  const inv = await invite(t);
  const rs = await Promise.all([redeem(inv.id, "A"), redeem(inv.id, "B"), redeem(inv.id, "C")]);
  assert.equal(rs.filter((r) => r.status === 200).length, 1, rs.map((r) => r.status).join(","));
});

test("expired invites are refused", async () => {
  const t = await owner();
  const inv = await invite(t);
  const key = `auth/invites/${inv.id}.json`;
  const v = await (await bucket.get(key)).json();
  await bucket.put(key, JSON.stringify({ ...v, expires_at: Date.now() - 1 }));
  assert.equal((await redeem(inv.id, "Late")).status, 410);
});

test("no token, a junk token, or a removed member: no access", async () => {
  assert.equal((await req("/l?prefix=v1/journeys/")).status, 401);
  assert.equal((await req("/l?prefix=v1/journeys/", { headers: auth("x".repeat(40)) })).status, 401);
  const t = await owner();
  const { token, member } = await (await redeem((await invite(t)).id, "Leaver")).json();
  assert.equal((await req("/l?prefix=v1/journeys/", { headers: auth(token) })).status, 200);
  // a non-owner can't remove anyone
  const other = await (await redeem((await invite(t)).id, "Other")).json();
  assert.equal((await req(`/members/${member.id}/remove`, { method: "POST", headers: auth(other.token) })).status, 403);
  assert.equal((await req(`/members/${member.id}/remove`, { method: "POST", headers: auth(t) })).status, 200);
  assert.equal((await req("/l?prefix=v1/journeys/", { headers: auth(token) })).status, 401);
  const names = (await (await req("/members", { headers: auth(t) })).json()).map((m) => m.name);
  assert.ok(!names.includes("Leaver") && names.includes("Other"));
});

test("conditional writes behave like R2's S3 API", async () => {
  const t = await owner();
  const key = "/o/v1/journeys/abcd.zip";
  const create = await req(key, { method: "PUT", headers: auth(t, { "if-none-match": "*" }), body: "one" });
  assert.equal(create.status, 200);
  const e1 = create.headers.get("etag");
  assert.ok(e1);
  assert.equal((await req(key, { method: "PUT", headers: auth(t, { "if-none-match": "*" }), body: "x" })).status, 412, "create-only on existing key");
  const upd = await req(key, { method: "PUT", headers: auth(t, { "if-match": e1 }), body: "two" });
  assert.equal(upd.status, 200);
  const e2 = upd.headers.get("etag");
  assert.equal((await req(key, { method: "PUT", headers: auth(t, { "if-match": e1 }), body: "three" })).status, 412, "stale If-Match");
  const got = await req(key, { headers: auth(t) });
  assert.equal(await got.text(), "two");
  assert.equal(got.headers.get("etag"), e2);
  const listed = await (await req("/l?prefix=v1/journeys/", { headers: auth(t) })).json();
  assert.ok(listed.some((o) => o.key === "v1/journeys/abcd.zip" && `"${o.etag}"` === e2));
  assert.equal((await req("/o/v1/journeys/dead.zip", { headers: auth(t) })).status, 404);
  // listing by an exact file name (used when restoring a version) is allowed
  const exact = await (await req("/l?prefix=v1/journeys/abcd.zip", { headers: auth(t) })).json();
  assert.equal(exact.length, 1);
});

test("only Walk Relay files, size-capped; history can't be deleted and is pruned", async () => {
  const t = await owner();
  for (const k of ["auth/members/x.json", "v1/../auth/x", "v1/other/x.zip", "v1/journeys/ABC.zip", "evil.exe"]) {
    const r = await req("/o/" + k, { method: "PUT", headers: auth(t), body: "x" });
    assert.ok([403, 404].includes(r.status), `${k} -> ${r.status}`);
  }
  assert.equal((await req("/l?prefix=auth/", { headers: auth(t) })).status, 403);
  assert.equal((await req("/l?prefix=", { headers: auth(t) })).status, 403);
  const big = new Uint8Array(25 * 1024 * 1024 + 1);
  assert.equal((await req("/o/v1/journeys/beef.zip", { method: "PUT", headers: auth(t), body: big })).status, 413);

  const hist = (i) => `/o/v1/history/abcd/${String(1700000000000 + i).padStart(13, "0")}-63.zip`;
  for (let i = 0; i < 23; i++) assert.equal((await req(hist(i), { method: "PUT", headers: auth(t), body: "v" + i })).status, 200);
  const h = await (await req("/l?prefix=v1/history/abcd/", { headers: auth(t) })).json();
  assert.equal(h.length, 20, "pruned to 20");
  assert.equal((await req(hist(22), { method: "DELETE", headers: auth(t) })).status, 403);
  assert.equal((await req("/o/v1/journeys/abcd.zip", { method: "DELETE", headers: auth(t) })).status, 204);
});

test("invite page opens the app and carries no secrets", async () => {
  const t = await owner();
  const inv = await invite(t);
  const r = await req(`/j/${inv.id}`);
  assert.equal(r.status, 200);
  const html = await r.text();
  assert.ok(html.includes(`walkrelay://join#walkrelay.example.workers.dev/${inv.id}`));
  assert.ok(html.includes('og:title'));
  assert.ok(!html.includes(t));
  assert.equal((await req("/j/nope")).status, 404);
});
