// Serves the Worker locally (simulated R2) for the Rust integration test, and
// prints its URL and an owner token. Usage: node worker/test/serve.mjs
import { Miniflare } from "miniflare";
const mf = new Miniflare({
  modules: true,
  scriptPath: new URL("../src/index.js", import.meta.url).pathname,
  r2Buckets: ["BUCKET"],
  compatibilityDate: "2025-06-01",
  host: "127.0.0.1",
  port: 0,
});
const url = (await mf.ready).toString().replace(/\/$/, "");
const bucket = await mf.getR2Bucket("BUCKET");
const nonce = "ab".repeat(16);
await bucket.put(`auth/bootstrap/${nonce}`, "");
const r = await fetch(`${url}/setup`, { method: "POST", body: JSON.stringify({ nonce, name: "Owner" }) });
const { token } = await r.json();
console.log(JSON.stringify({ url, token }));
