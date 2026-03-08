const defaultWsUrl = `${window.location.protocol === "https:" ? "wss" : "ws"}://${window.location.host}/ws`;
const defaultStunUrl = "stun:turn.makudoku.com:3478";
const urlParams = new URLSearchParams(window.location.search);
const enableIceRecoveryRenegotiation = urlParams.get("ice_recovery") === "1";

const el = {
  room: document.querySelector("#room"),
  displayName: document.querySelector("#display-name"),
  wsUrl: document.querySelector("#ws-url"),
  connect: document.querySelector("#connect"),
  renegotiate: document.querySelector("#renegotiate"),
  leave: document.querySelector("#leave"),
  status: document.querySelector("#status"),
  log: document.querySelector("#log"),
  localVideo: document.querySelector("#local-video"),
  videos: document.querySelector("#videos"),
  participantsList: document.querySelector("#participants-list"),
  publicationsList: document.querySelector("#publications-list"),
  subscriptionsList: document.querySelector("#subscriptions-list"),
};

el.wsUrl.value = defaultWsUrl;
if (!el.displayName.value) {
  el.displayName.value = `guest-${Math.floor(Math.random() * 9000 + 1000)}`;
}

let ws = null;
let pc = null;
let localStream = null;
let participantId = null;
let revision = null;
let rpcId = 1;
let pending = new Map();
let drainTimer = null;
let syncTimer = null;
let renegotiating = false;
let renegotiateRequested = false;
let renegotiateReason = "queued";
let renegotiateNeedsIceRestart = false;
let recoveryTimer = null;
let remoteCombinedStream = null;

function setStatus(text) {
  el.status.textContent = text;
}

function log(message, data) {
  const ts = new Date().toLocaleTimeString();
  const suffix = data === undefined ? "" : ` ${JSON.stringify(data)}`;
  el.log.textContent += `[${ts}] ${message}${suffix}\n`;
  el.log.scrollTop = el.log.scrollHeight;
}

function setButtons(connected) {
  el.connect.disabled = connected;
  el.leave.disabled = !connected;
  el.renegotiate.disabled = !connected;
}

function rpc(method, params = {}) {
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    return Promise.reject(new Error("websocket is not open"));
  }
  const id = rpcId++;
  const payload = { jsonrpc: "2.0", id, method, params };
  ws.send(JSON.stringify(payload));
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject, method });
    window.setTimeout(() => {
      const item = pending.get(id);
      if (!item) return;
      pending.delete(id);
      reject(new Error(`rpc timeout: ${method}`));
    }, 10000);
  });
}

function drainPending(error) {
  for (const [id, entry] of pending.entries()) {
    pending.delete(id);
    entry.reject(error);
  }
}

function ensureRemoteTile() {
  let section = document.querySelector(`[data-stream-id="remote-main"]`);
  if (section) return section.querySelector("video");
  section = document.createElement("section");
  section.className = "tile";
  section.dataset.streamId = "remote-main";
  const header = document.createElement("header");
  header.textContent = "Remote";
  const video = document.createElement("video");
  video.autoplay = true;
  video.playsInline = true;
  section.append(header, video);
  el.videos.appendChild(section);
  return video;
}

function buildIceServers() {
  const params = urlParams;
  const stun = params.get("stun") || defaultStunUrl;
  const turnUrl = params.get("turn") || "turn:turn.makudoku.com:3478?transport=udp";
  const turnUser = params.get("turn_user");
  const turnPass = params.get("turn_pass");

  const servers = [];
  if (stun) {
    servers.push({ urls: stun });
  }
  if (turnUser && turnPass) {
    servers.push({
      urls: [turnUrl],
      username: turnUser,
      credential: turnPass,
    });
  }
  return servers;
}

function renderSimpleList(ul, items) {
  ul.textContent = "";
  if (!items || items.length === 0) {
    const li = document.createElement("li");
    li.textContent = "(none)";
    ul.appendChild(li);
    return;
  }
  for (const item of items) {
    const li = document.createElement("li");
    li.textContent = item;
    ul.appendChild(li);
  }
}

function renderMeetingLists(participants, publications, subscriptions) {
  renderSimpleList(
    el.participantsList,
    (participants || []).map(
      (p) => `${p.display_name || "anonymous"} (${p.participant_id.slice(0, 8)})`
    )
  );
  renderSimpleList(
    el.publicationsList,
    (publications || []).map(
      (p) => `${p.track_id} • ${p.media_kind} • ${p.publisher_id.slice(0, 8)}`
    )
  );
  renderSimpleList(el.subscriptionsList, subscriptions || []);
}

async function setupPeerConnection() {
  const iceServers = buildIceServers();
  pc = new RTCPeerConnection({ iceServers });
  log("ice servers configured", {
    stun: iceServers.filter((s) => String(s.urls).startsWith("stun:")).length,
    turn: iceServers.filter((s) => String(s.urls).startsWith("turn:")).length,
  });

  pc.addEventListener("connectionstatechange", () => {
    setStatus(`Peer ${pc.connectionState}`);
    log("pc connectionstate", { state: pc.connectionState });
  });

  pc.addEventListener("iceconnectionstatechange", () => {
    log("pc ice state", { state: pc.iceConnectionState });
    if (
      enableIceRecoveryRenegotiation &&
      (pc.iceConnectionState === "disconnected" || pc.iceConnectionState === "failed")
    ) {
      if (!recoveryTimer) {
        recoveryTimer = window.setTimeout(() => {
          recoveryTimer = null;
          void renegotiate("ice recovery", { iceRestart: true });
        }, 1500);
      }
    } else if (pc.iceConnectionState === "connected") {
      if (recoveryTimer) {
        window.clearTimeout(recoveryTimer);
        recoveryTimer = null;
      }
    }
  });

  pc.addEventListener("signalingstatechange", () => {
    log("pc signaling state", { state: pc.signalingState });
    if (pc.signalingState === "stable" && renegotiateRequested) {
      const reason = renegotiateReason;
      const iceRestart = renegotiateNeedsIceRestart;
      renegotiateRequested = false;
      renegotiateNeedsIceRestart = false;
      void renegotiate(`${reason} (deferred)`, { iceRestart });
    }
  });

  pc.addEventListener("icecandidate", async (event) => {
    if (!event.candidate || !event.candidate.candidate) return;
    try {
      await rpc("trickle_ice", {
        mline_index: event.candidate.sdpMLineIndex ?? 0,
        candidate: event.candidate.candidate,
      });
    } catch (error) {
      log("trickle_ice failed", { error: error.message });
    }
  });

  pc.addEventListener("track", (event) => {
    if (!remoteCombinedStream) {
      remoteCombinedStream = new MediaStream();
    }
    // Avoid duplicate track insertion on repeated track events.
    const exists = remoteCombinedStream
        .getTracks()
        .some((t) => t.id === event.track.id);
    if (!exists) {
      remoteCombinedStream.addTrack(event.track);
    }
    const video = ensureRemoteTile();
    video.srcObject = remoteCombinedStream;
    log("remote track", { kind: event.track.kind, id: event.track.id });
  });

  localStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: true });
  el.localVideo.srcObject = localStream;
  for (const track of localStream.getTracks()) {
    pc.addTrack(track, localStream);
  }
}

async function renegotiate(reason, options = {}) {
  const iceRestart = Boolean(options.iceRestart);
  if (!pc) return;
  if (renegotiating) {
    renegotiateRequested = true;
    renegotiateReason = reason;
    renegotiateNeedsIceRestart = renegotiateNeedsIceRestart || iceRestart;
    return;
  }
  if (pc.signalingState !== "stable") {
    renegotiateRequested = true;
    renegotiateReason = reason;
    renegotiateNeedsIceRestart = renegotiateNeedsIceRestart || iceRestart;
    log("renegotiate deferred (not stable)", { reason, state: pc.signalingState });
    return;
  }
  renegotiating = true;
  try {
    const offer = await pc.createOffer({ iceRestart });
    await pc.setLocalDescription(offer);
    const res = await rpc("sdp_offer", {
      revision,
      offer_sdp: offer.sdp,
    });
    revision = res.revision;
    log("remote answer summary", summarizeSdp(res.answer_sdp));
    await pc.setRemoteDescription({ type: "answer", sdp: res.answer_sdp });
    log("renegotiated", { reason, revision, iceRestart });
  } catch (error) {
    log("renegotiate failed", { reason, error: error.message });
    if (pc && pc.localDescription?.sdp) {
      log("local offer summary (failed cycle)", summarizeSdp(pc.localDescription.sdp));
    }
    try {
      if (pc.signalingState === "have-local-offer") {
        await pc.setLocalDescription({ type: "rollback" });
        log("rolled back local offer after failure");
      }
    } catch (rollbackError) {
      log("rollback failed", { error: rollbackError.message });
    }
    renegotiateRequested = true;
    renegotiateReason = reason;
    renegotiateNeedsIceRestart = renegotiateNeedsIceRestart || iceRestart;
  } finally {
    renegotiating = false;
  }
}

function summarizeSdp(sdp) {
  return (sdp || "")
    .split(/\r?\n/)
    .filter(
      (line) =>
        line.startsWith("m=") ||
        line.startsWith("a=mid:") ||
        line.startsWith("a=setup:") ||
        line.startsWith("a=send") ||
        line.startsWith("a=recv") ||
        line.startsWith("a=rtpmap:")
    );
}

async function syncSubscriptions() {
  if (!participantId) return;
  try {
    const parts = await rpc("list_participants", {});
    const pubs = await rpc("list_publications", {});
    const subs = await rpc("list_subscriptions", {});
    renderMeetingLists(parts.participants, pubs.publications, subs.track_ids);
    const byPublisher = new Map();
    for (const p of pubs.publications || []) {
      if (p.publisher_id === participantId) continue;
      const list = byPublisher.get(p.publisher_id) || [];
      list.push(p);
      byPublisher.set(p.publisher_id, list);
    }
    const selectedPublisher = [...byPublisher.keys()].sort()[0] || null;
    const wanted = new Set(
      selectedPublisher
        ? (byPublisher.get(selectedPublisher) || []).map((p) => p.track_id)
        : []
    );
    const current = new Set(subs.track_ids || []);
    const toAdd = [...wanted].filter((id) => !current.has(id));
    const toRemove = [...current].filter((id) => !wanted.has(id));

    let changed = false;
    if (toAdd.length > 0) {
      const res = await rpc("subscribe", { track_ids: toAdd });
      revision = res.revision;
      changed = true;
      log("subscribe", { publisher_id: selectedPublisher, track_ids: toAdd, revision });
    }
    if (toRemove.length > 0) {
      const res = await rpc("unsubscribe", { track_ids: toRemove });
      revision = res.revision;
      changed = true;
      log("unsubscribe", { track_ids: toRemove, revision });
    }
    if (changed) {
      if (pc && pc.iceConnectionState !== "connected") {
        renegotiateRequested = true;
        renegotiateReason = "subscription update";
        log("subscription renegotiation deferred (ice not connected)", {
          iceState: pc ? pc.iceConnectionState : "unknown",
        });
      } else {
        await renegotiate("subscription update");
      }
    }
  } catch (error) {
    log("subscription sync failed", { error: error.message });
  }
}

async function startDrainLoop() {
  drainTimer = window.setInterval(async () => {
    if (!pc) return;
    try {
      const res = await rpc("drain_ice", { max: 50 });
      const candidates = res.candidates || [];
      for (const c of candidates) {
        if (!c.candidate) continue;
        await pc.addIceCandidate({
          sdpMLineIndex: c.mline_index,
          candidate: c.candidate,
        });
      }
    } catch (error) {
      log("drain_ice failed", { error: error.message });
    }
  }, 1000);
}

async function connect() {
  const wsUrl = el.wsUrl.value.trim();
  const room = el.room.value.trim();
  const display_name = el.displayName.value.trim() || undefined;
  if (!wsUrl || !room) {
    setStatus("Room and WS URL are required");
    return;
  }

  setStatus("Connecting");
  el.log.textContent = "";

  ws = new WebSocket(wsUrl);
  ws.onmessage = (event) => {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      log("non-json message", { data: String(event.data) });
      return;
    }
    if (msg.id === undefined || msg.id === null) {
      log("notification", msg);
      return;
    }
    const req = pending.get(msg.id);
    if (!req) return;
    pending.delete(msg.id);
    if (msg.error) {
      req.reject(new Error(`${msg.error.code}: ${msg.error.message}`));
    } else {
      req.resolve(msg.result ?? {});
    }
  };

  ws.onclose = () => {
    drainPending(new Error("websocket closed"));
    setStatus("Disconnected");
    setButtons(false);
  };

  ws.onerror = () => {
    log("ws error");
  };

  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    window.setTimeout(() => reject(new Error("ws connect timeout")), 8000);
  });

  const joined = await rpc("join", { room, display_name });
  participantId = joined.participant_id;
  revision = joined.revision;
  log("joined", joined);

  await setupPeerConnection();

  const tracks = [];
  for (const track of localStream.getTracks()) {
    tracks.push({
      track_id: `${participantId}-${track.kind}`,
      media_kind: track.kind,
      mid: null,
    });
  }
  const published = await rpc("publish_tracks", { tracks });
  revision = published.revision;
  log("publish_tracks", { revision, tracks: tracks.map((t) => t.track_id) });

  await renegotiate("initial publish");
  await startDrainLoop();
  await syncSubscriptions();
  syncTimer = window.setInterval(syncSubscriptions, 2000);

  setButtons(true);
  setStatus(`Joined ${room} as ${participantId.slice(0, 8)}`);
}

async function leave() {
  if (syncTimer) {
    window.clearInterval(syncTimer);
    syncTimer = null;
  }
  if (drainTimer) {
    window.clearInterval(drainTimer);
    drainTimer = null;
  }

  if (pc) {
    pc.close();
    pc = null;
  }
  if (localStream) {
    for (const track of localStream.getTracks()) track.stop();
    localStream = null;
  }
  el.localVideo.srcObject = null;

  for (const tile of [...document.querySelectorAll(".tile[data-stream-id]")]) {
    tile.remove();
  }
  remoteCombinedStream = null;

  try {
    if (ws && ws.readyState === WebSocket.OPEN) {
      await rpc("leave", {});
    }
  } catch (error) {
    log("leave rpc failed", { error: error.message });
  }

  if (ws) {
    ws.close();
    ws = null;
  }

  participantId = null;
  revision = null;
  renderMeetingLists([], [], []);
  setButtons(false);
  setStatus("Idle");
}

el.connect.addEventListener("click", () => {
  connect().catch(async (error) => {
    log("connect failed", { error: error.message });
    await leave();
  });
});

el.renegotiate.addEventListener("click", () => {
  syncSubscriptions().catch((error) => {
    log("sync now failed", { error: error.message });
  });
});

el.leave.addEventListener("click", () => {
  leave();
});

window.addEventListener("beforeunload", () => {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.close();
  }
});
