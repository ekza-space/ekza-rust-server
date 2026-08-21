// End-to-end check of the realtime server against a live Solana cluster.
//
// Env:
//   SERVER_URL        default http://127.0.0.1:3001
//   OWNER_KEYPAIR     path to a keypair JSON that holds SPACE_ID's NFT
//   SPACE_ID          default 3
//   PHASE             "write" (default) runs the full scenario and leaves a
//                     marker object in the room; "verify" only checks that the
//                     marker survived (run after a server restart).
import { io } from "socket.io-client";
import { Keypair } from "@solana/web3.js";
import nacl from "tweetnacl";
import bs58 from "bs58";
import { readFileSync } from "fs";

const SERVER_URL = process.env.SERVER_URL ?? "http://127.0.0.1:3001";
const SPACE_ID = process.env.SPACE_ID ?? "3";
const PHASE = process.env.PHASE ?? "write";
const MARKER_ID = "e2e-marker";

const owner = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(readFileSync(process.env.OWNER_KEYPAIR, "utf8")))
);
const stranger = Keypair.generate();

let failures = 0;
const ok = (cond, label) => {
  console.log(`${cond ? "ok  " : "FAIL"}  ${label}`);
  if (!cond) failures++;
};

function connect() {
  const s = io(SERVER_URL, { transports: ["websocket"], forceNew: true });
  const inbox = [];
  s.onAny((ev, ...args) => inbox.push({ ev, args, t: Date.now() }));
  const waitFor = (ev, pred = () => true, ms = 4000) =>
    new Promise((resolve, reject) => {
      const hit = inbox.find((m) => m.ev === ev && pred(...m.args));
      if (hit) return resolve(hit.args);
      const timer = setTimeout(() => reject(new Error(`timeout waiting for "${ev}"`)), ms);
      const h = (...args) => {
        if (pred(...args)) {
          clearTimeout(timer);
          s.off(ev, h);
          resolve(args);
        }
      };
      s.on(ev, h);
    });
  const drain = () => inbox.splice(0);
  /** Like waitFor, but ignores messages already received. */
  const waitForNew = (ev, pred = () => true, ms = 4000) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timeout waiting for new "${ev}"`)), ms);
      const h = (...args) => {
        if (pred(...args)) {
          clearTimeout(timer);
          s.off(ev, h);
          resolve(args);
        }
      };
      s.on(ev, h);
    });
  return { s, waitFor, waitForNew, drain, inbox };
}

async function auth(c, kp) {
  const [{ nonce, message }] = await c.waitFor("auth nonce");
  const sig = nacl.sign.detached(new TextEncoder().encode(message), kp.secretKey);
  const pending = c.waitForNew("auth result");
  c.s.emit("auth", { pubkey: kp.publicKey.toBase58(), signature: bs58.encode(sig) });
  const [res] = await pending;
  return { res, nonce };
}

const state = (objects) => ({
  version: 1,
  environmentId: "studio-grid",
  objects,
  updatedAt: Date.now(),
});
const cube = (id, extra = {}) => ({
  id,
  kind: "cube",
  label: "E2E",
  position: [1, 1, 1],
  rotation: [0, 0, 0],
  scale: [1, 1, 1],
  color: "#f97316",
  ...extra,
});

async function writePhase() {
  const c = connect();
  await c.waitFor("auth nonce");
  ok(true, "server sends auth nonce on connect");

  // Room id validation.
  c.s.emit("join-space", "999999");
  let [err] = await c.waitFor("room error", (e) => e.roomId === "999999");
  ok(err.code === "invalid_room", `join 999999 → ${err.code}`);
  c.s.emit("join-space", "abc");
  [err] = await c.waitFor("room error", (e) => e.roomId === "abc");
  ok(err.code === "invalid_room", `join abc → ${err.code}`);

  // Join real space as guest.
  c.s.emit("join-space", SPACE_ID);
  const [initial] = await c.waitFor("room program state", (p) => p.roomId === SPACE_ID);
  ok(typeof initial.serverRevision === "number", `joined #${SPACE_ID} at revision ${initial.serverRevision}`);
  const [access0] = await c.waitFor("room access", (a) => a.roomId === SPACE_ID);
  ok(access0.canEdit === false, "guest cannot edit");

  // Guest write → auth required.
  c.s.emit("room program update", { roomId: SPACE_ID, state: state([cube("x")]), serverRevision: initial.serverRevision });
  [err] = await c.waitFor("room error", (e) => e.code === "auth_required");
  ok(true, "guest write → auth_required");

  // Bad signature.
  let pendingBad = c.waitForNew("auth result");
  c.s.emit("auth", { pubkey: owner.publicKey.toBase58(), signature: bs58.encode(new Uint8Array(64)) });
  let [res] = await pendingBad;
  ok(res.ok === false && res.error === "invalid_signature", `bad signature → ${res.error}`);

  // Owner auth: nonce was NOT consumed by the failed attempt; sign the original.
  ({ res } = await auth(c, owner));
  ok(res.ok === true && res.wallet === owner.publicKey.toBase58(), "owner authenticated");
  const [access1] = await c.waitFor("room access", (a) => a.roomId === SPACE_ID && a.canEdit === true);
  ok(access1.canEdit === true && access1.holder === owner.publicKey.toBase58(), "owner canEdit=true, holder matches");

  // Stale revision rejected.
  c.drain();
  c.s.emit("room program update", { roomId: SPACE_ID, state: state([cube("x")]), serverRevision: initial.serverRevision + 50 });
  let [reply] = await c.waitFor("room program state", (p) => p.rejected);
  ok(reply.rejected === "stale_revision" && reply.serverRevision === initial.serverRevision, "stale revision → rejected with current state");

  // XSS link rejected.
  c.s.emit("room program update", {
    roomId: SPACE_ID,
    state: state([cube("l", { kind: "link", linkUrl: "javascript:alert(1)" })]),
    serverRevision: initial.serverRevision,
  });
  [err] = await c.waitFor("room error", (e) => e.code === "invalid_state");
  ok(/link/.test(err.detail ?? ""), `javascript: link → invalid_state (${err.detail})`);

  // Inline model rejected.
  c.s.emit("room program update", {
    roomId: SPACE_ID,
    state: state([cube("m", { kind: "model", modelDataUrl: "data:model/gltf-binary;base64,AAAA" })]),
    serverRevision: initial.serverRevision,
  });
  [err] = await c.waitFor("room error", (e) => e.code === "invalid_state" && /model/.test(e.detail ?? ""));
  ok(true, "data: model → invalid_state");

  // Valid write applied + broadcast.
  c.drain();
  c.s.emit("room program update", { roomId: SPACE_ID, state: state([cube(MARKER_ID)]), serverRevision: initial.serverRevision });
  [reply] = await c.waitFor("room program state", (p) => !p.rejected && p.serverRevision === initial.serverRevision + 1);
  ok(reply.sourceClientId === c.s.id && reply.state.objects[0]?.id === MARKER_ID, `write applied → revision ${reply.serverRevision}`);

  // Stranger: authenticated but not holder.
  const d = connect();
  const { res: sres } = await auth(d, stranger);
  ok(sres.ok === true, "stranger authenticated");
  d.s.emit("join-space", SPACE_ID);
  const [saccess] = await d.waitFor("room access", (a) => a.roomId === SPACE_ID);
  ok(saccess.canEdit === false, "stranger canEdit=false");
  d.s.emit("room program update", { roomId: SPACE_ID, state: state([]), serverRevision: reply.serverRevision });
  [err] = await d.waitFor("room error", (e) => e.code === "forbidden");
  ok(true, "stranger write → forbidden");

  // Stranger still sees the owner's update live.
  c.s.emit("room program update", { roomId: SPACE_ID, state: state([cube(MARKER_ID), cube("second")]), serverRevision: reply.serverRevision });
  const [seen] = await d.waitFor("room program state", (p) => p.serverRevision === reply.serverRevision + 1);
  ok(seen.state.objects.length === 2, "stranger received owner's broadcast");

  c.s.disconnect();
  d.s.disconnect();
}

async function verifyPhase() {
  const c = connect();
  await c.waitFor("auth nonce");
  c.s.emit("join-space", SPACE_ID);
  const [st] = await c.waitFor("room program state", (p) => p.roomId === SPACE_ID);
  ok(st.state.objects.some((o) => o.id === MARKER_ID), `marker survived restart (revision ${st.serverRevision}, ${st.state.objects.length} objects)`);
  c.s.disconnect();
}

try {
  if (PHASE === "verify") await verifyPhase();
  else await writePhase();
} catch (e) {
  console.error("FAIL ", e.message);
  failures++;
}
console.log(failures ? `E2E FAIL (${failures})` : "E2E PASS");
process.exit(failures ? 1 : 0);
