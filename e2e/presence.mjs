// Guest-only smoke test for browser-presence de-duplication.
// Run against a live server: SERVER_URL=http://127.0.0.1:3001 yarn e2e:presence
import { io } from "socket.io-client";

const SERVER_URL = process.env.SERVER_URL ?? "http://127.0.0.1:3001";
const SPACE_ID = process.env.SPACE_ID ?? "1";
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const waitFor = (socket, event, predicate = () => true, timeout = 15_000) =>
  new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.off(event, handler);
      reject(new Error(`timeout waiting for "${event}"`));
    }, timeout);
    const handler = (...args) => {
      if (!predicate(...args)) return;
      clearTimeout(timer);
      socket.off(event, handler);
      resolve(args);
    };
    socket.on(event, handler);
  });

const sockets = [];
const claimIds = new WeakMap();

const nextClaimId = (socket) => {
  const claimId = (claimIds.get(socket) ?? 0) + 1;
  claimIds.set(socket, claimId);
  return claimId;
};

const openSocket = async () => {
  const socket = io(SERVER_URL, {
    autoConnect: false,
    transports: ["websocket"],
    forceNew: true,
  });
  sockets.push(socket);
  const nonce = waitFor(socket, "auth nonce");
  socket.connect();
  await nonce;
  return socket;
};

const joinPresence = async (socket, nickname, presenceToken) => {
  const claimId = nextClaimId(socket);
  const snapshot = waitFor(socket, "existing clients");
  const ready = waitFor(
    socket,
    "presence ready",
    (payload) => payload.roomId === SPACE_ID && payload.claimId === claimId
  );
  socket.emit("set user data", {
    nickname,
    avatar: `https://example.com/${nickname}.vrm`,
    presenceToken,
  });
  socket.emit("join-space", { roomId: SPACE_ID, presenceToken, claimId });
  const [[clients]] = await Promise.all([snapshot, ready]);
  return clients;
};

const openPresence = async (nickname, presenceToken) => {
  const socket = await openSocket();
  const clients = await joinPresence(socket, nickname, presenceToken);
  return { socket, clients };
};

const reclaimPresence = async (socket, presenceToken) => {
  const claimId = nextClaimId(socket);
  const snapshot = waitFor(socket, "existing clients");
  const ready = waitFor(
    socket,
    "presence ready",
    (payload) => payload.roomId === SPACE_ID && payload.claimId === claimId
  );
  socket.emit("join-space", { roomId: SPACE_ID, presenceToken, claimId });
  const [[clients]] = await Promise.all([snapshot, ready]);
  return clients;
};

const assertTokenIsPrivate = (payload) => {
  const encoded = JSON.stringify(payload);
  if (encoded.includes("presenceToken") || encoded.includes("shared-token-")) {
    throw new Error("private presence token leaked into an outbound payload");
  }
};

try {
  const observer = await openPresence(
    "Observer",
    "observer-token-0123456789abcdef"
  );
  assertTokenIsPrivate(observer.clients);

  const observerNewA = waitFor(
    observer.socket,
    "new user",
    (payload) => payload.userData?.nickname === "TabA"
  );
  const tabA = await openPresence("TabA", "shared-token-0123456789abcdef");
  const [newAPayload] = await observerNewA;
  assertTokenIsPrivate(newAPayload);
  const { id: tabAId } = newAPayload;

  const observerDeletedA = waitFor(
    observer.socket,
    "delete",
    (id) => id === tabAId
  );
  const observerNewB = waitFor(
    observer.socket,
    "new user",
    (payload) => payload.userData?.nickname === "TabB"
  );
  const tabASuperseded = waitFor(
    tabA.socket,
    "presence superseded",
    (payload) => payload.roomId === SPACE_ID
  );
  const tabB = await openPresence("TabB", "shared-token-0123456789abcdef");
  await observerDeletedA;
  await tabASuperseded;
  const [newBPayload] = await observerNewB;
  assertTokenIsPrivate(newBPayload);
  assertTokenIsPrivate(tabB.clients);
  const { id: tabBId } = newBPayload;

  if (tabAId === tabBId) throw new Error("expected distinct socket ids");
  if (tabAId in tabB.clients || !(tabBId in tabB.clients)) {
    throw new Error("snapshot did not retain self while removing the superseded tab");
  }
  if (!(observer.socket.id in tabB.clients)) {
    throw new Error("snapshot lost an unrelated observer");
  }

  const observedMoves = [];
  observer.socket.on("move", (payload) => observedMoves.push(payload));

  tabA.socket.emit("move", { position: [10, 0, 10], rotation: 0, seq: 1 });
  await delay(200);
  if (observedMoves.some((payload) => payload.id === tabAId)) {
    throw new Error("superseded tab still broadcasts movement");
  }

  tabB.socket.emit("move", { position: [20, 0, 20], rotation: 0, seq: 1 });
  await delay(200);
  if (!observedMoves.some((payload) => payload.id === tabBId)) {
    throw new Error("active tab movement was not broadcast");
  }
  assertTokenIsPrivate(observedMoves);

  // A focused superseded tab reclaims with the same transport id and without
  // resending profile data. Reclaiming B afterwards must accept seq=1 again.
  const observerDeletedB = waitFor(observer.socket, "delete", (id) => id === tabBId);
  const observerRejoinedA = waitFor(
    observer.socket,
    "new user",
    (payload) => payload.id === tabAId
  );
  const tabBSuperseded = waitFor(
    tabB.socket,
    "presence superseded",
    (payload) => payload.roomId === SPACE_ID
  );
  const reclaimedAClients = await reclaimPresence(
    tabA.socket,
    "shared-token-0123456789abcdef"
  );
  await Promise.all([observerDeletedB, observerRejoinedA, tabBSuperseded]);
  assertTokenIsPrivate(reclaimedAClients);
  if (tabBId in reclaimedAClients) {
    throw new Error("reclaim snapshot retained the superseded active tab");
  }

  const observerDeletedRejoinedA = waitFor(
    observer.socket,
    "delete",
    (id) => id === tabAId
  );
  const observerRejoinedB = waitFor(
    observer.socket,
    "new user",
    (payload) => payload.id === tabBId
  );
  const tabAResuperseded = waitFor(
    tabA.socket,
    "presence superseded",
    (payload) => payload.roomId === SPACE_ID
  );
  await reclaimPresence(tabB.socket, "shared-token-0123456789abcdef");
  await Promise.all([
    observerDeletedRejoinedA,
    observerRejoinedB,
    tabAResuperseded,
  ]);

  const movesBeforeReclaim = observedMoves.length;
  tabB.socket.emit("move", { position: [21, 0, 21], rotation: 0, seq: 1 });
  await delay(200);
  if (
    !observedMoves
      .slice(movesBeforeReclaim)
      .some((payload) => payload.id === tabBId)
  ) {
    throw new Error("same-socket reclaim did not reset the client movement sequence");
  }

  // Start two first-time tabs together. The full logical+adapter transition is
  // serialized, so observer events must converge to exactly one visible tab.
  const visibleRaceClients = new Map();
  const recordNewUser = (payload) => {
    if (payload.userData?.nickname?.startsWith("Race")) {
      visibleRaceClients.set(payload.id, payload.userData.nickname);
      assertTokenIsPrivate(payload);
    }
  };
  const recordDelete = (id) => visibleRaceClients.delete(id);
  observer.socket.on("new user", recordNewUser);
  observer.socket.on("delete", recordDelete);

  const [raceA, raceB] = await Promise.all([openSocket(), openSocket()]);
  const raceASnapshot = joinPresence(
    raceA,
    "RaceA",
    "race-shared-token-0123456789abcdef"
  );
  const raceBSnapshot = joinPresence(
    raceB,
    "RaceB",
    "race-shared-token-0123456789abcdef"
  );
  const raceSnapshots = await Promise.all([raceASnapshot, raceBSnapshot]);
  raceSnapshots.forEach(assertTokenIsPrivate);
  await delay(200);

  if (visibleRaceClients.size !== 1) {
    throw new Error(
      `simultaneous joins left ${visibleRaceClients.size} visible browser presences`
    );
  }

  const activeRaceId = visibleRaceClients.keys().next().value;
  const movesBeforeRace = observedMoves.length;
  raceA.emit("move", { position: [30, 0, 30], rotation: 0, seq: 1 });
  raceB.emit("move", { position: [40, 0, 40], rotation: 0, seq: 1 });
  await delay(200);
  const raceMoves = observedMoves
    .slice(movesBeforeRace)
    .filter((payload) => payload.id === raceA.id || payload.id === raceB.id);
  if (raceMoves.length !== 1 || raceMoves[0].id !== activeRaceId) {
    throw new Error("simultaneous join winner does not match movement authority");
  }
  assertTokenIsPrivate(raceMoves);

  // A newer navigation intent must cancel a slower/older join even when the
  // newer room is invalid. Replaying the old generation must not resurrect
  // the socket in the previous room.
  const ordered = await openSocket();
  ordered.emit("set user data", {
    nickname: "Ordered",
    avatar: "https://example.com/ordered.vrm",
    presenceToken: "ordered-token-0123456789abcdef",
  });
  const invalidClaimError = waitFor(
    ordered,
    "room error",
    (payload) => payload.roomId === "999" && payload.claimId === 2
  );
  ordered.emit("join-space", {
    roomId: SPACE_ID,
    presenceToken: "ordered-token-0123456789abcdef",
    claimId: 1,
  });
  ordered.emit("join-space", {
    roomId: "999",
    presenceToken: "ordered-token-0123456789abcdef",
    claimId: 2,
  });
  await invalidClaimError;
  await delay(100);

  const movesBeforeStaleReplay = observedMoves.length;
  ordered.emit("join-space", {
    roomId: SPACE_ID,
    presenceToken: "ordered-token-0123456789abcdef",
    claimId: 1,
  });
  ordered.emit("move", { position: [50, 0, 50], rotation: 0, seq: 1 });
  await delay(200);
  if (
    observedMoves
      .slice(movesBeforeStaleReplay)
      .some((payload) => payload.id === ordered.id)
  ) {
    throw new Error("stale room claim resurrected a detached socket");
  }

  console.log("presence e2e: pass");
} finally {
  for (const socket of sockets) socket.disconnect();
}
