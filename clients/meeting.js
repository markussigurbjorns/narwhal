const defaultWsUrl = `${window.location.protocol === "https:" ? "wss" : "ws"}://${window.location.host}/ws`;
const defaultStunUrl = "stun:turn.makudoku.com:3478";
const urlParams = new URLSearchParams(window.location.search);
const enableIceRecoveryRenegotiation = urlParams.get("ice_recovery") === "1";
const enableSimulcastByDefault = urlParams.get("simulcast") !== "0";

const el = {
  room: document.querySelector("#room"),
  displayName: document.querySelector("#display-name"),
  wsUrl: document.querySelector("#ws-url"),
  capturePreset: document.querySelector("#capture-preset"),
  enableSimulcast: document.querySelector("#enable-simulcast"),
  connect: document.querySelector("#connect"),
  renegotiate: document.querySelector("#renegotiate"),
  leave: document.querySelector("#leave"),
  policyMode: document.querySelector("#policy-mode"),
  policyMeta: document.querySelector("#policy-meta"),
  status: document.querySelector("#status"),
  log: document.querySelector("#log"),
  localVideo: document.querySelector("#local-video"),
  videos: document.querySelector("#videos"),
  participantsList: document.querySelector("#participants-list"),
  publicationsList: document.querySelector("#publications-list"),
  publisherSelect: document.querySelector("#publisher-select"),
  trackOptions: document.querySelector("#track-options"),
  applySubscriptions: document.querySelector("#apply-subscriptions"),
  requestedSubscriptionsList: document.querySelector("#requested-subscriptions-list"),
  effectiveSubscriptionsList: document.querySelector("#effective-subscriptions-list"),
  streamsList: document.querySelector("#streams-list"),
  mediaStatsList: document.querySelector("#media-stats-list"),
};

el.wsUrl.value = defaultWsUrl;
el.enableSimulcast.checked = enableSimulcastByDefault;
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
let currentPolicyMode = null;
let latestPublications = [];
let latestRequestedTrackIds = [];
let selectedPublisherId = null;
let statsTimer = null;

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
  el.policyMode.disabled = !connected;
  el.enableSimulcast.disabled = connected;
  el.publisherSelect.disabled = !connected;
  el.applySubscriptions.disabled = !connected;
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
  section.className = "tile remote-primary";
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

function getCaptureConstraints() {
  const [width, height] = (el.capturePreset.value || "1280x720")
    .split("x")
    .map((value) => Number.parseInt(value, 10));
  return {
    width: { ideal: width },
    height: { ideal: height },
    frameRate: { ideal: 30, max: 30 },
  };
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

function renderMeetingLists(participants, publications, requestedSubscriptions, effectiveSubscriptions, streams) {
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
  renderSimpleList(el.requestedSubscriptionsList, requestedSubscriptions || []);
  renderSimpleList(el.effectiveSubscriptionsList, effectiveSubscriptions || []);
  renderSimpleList(
    el.streamsList,
    (streams || []).map((stream) => {
      const bits = [
        stream.track_id,
        stream.media_kind,
        stream.encoding_id,
        `ssrc=${stream.ssrc}`,
      ];
      if (stream.mid) bits.push(`mid=${stream.mid}`);
      if (stream.rid) bits.push(`rid=${stream.rid}`);
      if (stream.spatial_layer !== null && stream.spatial_layer !== undefined) {
        bits.push(`spatial=${stream.spatial_layer}`);
      }
      return bits.join(" • ");
    })
  );
}

function renderMediaStats(items) {
  renderSimpleList(el.mediaStatsList, items);
}

function renderSubscriptionControls(participants, publications, requestedTrackIds) {
  latestPublications = publications || [];
  latestRequestedTrackIds = requestedTrackIds || [];

  const remotePublishers = new Map();
  for (const publication of latestPublications) {
    if (publication.publisher_id === participantId) continue;
    if (!remotePublishers.has(publication.publisher_id)) {
      const participant = (participants || []).find(
        (item) => item.participant_id === publication.publisher_id
      );
      remotePublishers.set(publication.publisher_id, {
        participantId: publication.publisher_id,
        displayName:
          participant?.display_name || `participant ${publication.publisher_id.slice(0, 8)}`,
      });
    }
  }

  const publisherIds = [...remotePublishers.keys()].sort();
  if (!selectedPublisherId || !remotePublishers.has(selectedPublisherId)) {
    selectedPublisherId = publisherIds[0] || null;
  }

  el.publisherSelect.textContent = "";
  if (publisherIds.length === 0) {
    const option = document.createElement("option");
    option.value = "";
    option.textContent = "No remote publishers";
    el.publisherSelect.appendChild(option);
    el.publisherSelect.disabled = true;
    el.applySubscriptions.disabled = true;
  } else {
    for (const publisherId of publisherIds) {
      const option = document.createElement("option");
      option.value = publisherId;
      option.textContent = remotePublishers.get(publisherId).displayName;
      if (publisherId === selectedPublisherId) {
        option.selected = true;
      }
      el.publisherSelect.appendChild(option);
    }
    el.publisherSelect.disabled = false;
    el.applySubscriptions.disabled = false;
  }

  const selectedTracks = latestPublications.filter(
    (publication) => publication.publisher_id === selectedPublisherId
  );
  el.trackOptions.textContent = "";
  if (selectedTracks.length === 0) {
    const empty = document.createElement("p");
    empty.className = "meta";
    empty.textContent = "No remote tracks";
    el.trackOptions.appendChild(empty);
    return;
  }

  for (const publication of selectedTracks) {
    const label = document.createElement("label");
    label.className = "track-option";
    const input = document.createElement("input");
    input.type = "checkbox";
    input.value = publication.track_id;
    input.checked = latestRequestedTrackIds.includes(publication.track_id);
    const text = document.createElement("span");
    text.textContent = `${publication.media_kind} • ${publication.track_id}`;
    label.append(input, text);
    el.trackOptions.appendChild(label);
  }
}

async function applySubscriptionSelection() {
  if (!participantId || !selectedPublisherId) return;
  const selectedTrackIds = [...el.trackOptions.querySelectorAll('input[type="checkbox"]:checked')]
    .map((input) => input.value);
  const publisherTrackIds = latestPublications
    .filter((publication) => publication.publisher_id === selectedPublisherId)
    .map((publication) => publication.track_id);

  const current = new Set(latestRequestedTrackIds);
  const wantedForPublisher = new Set(selectedTrackIds);

  const toAdd = selectedTrackIds.filter((trackId) => !current.has(trackId));
  const toRemove = publisherTrackIds.filter(
    (trackId) => current.has(trackId) && !wantedForPublisher.has(trackId)
  );

  let changed = false;
  let needsRenegotiation = false;
  if (toAdd.length > 0) {
    const res = await rpc("subscribe", { track_ids: toAdd });
    revision = res.revision;
    needsRenegotiation = needsRenegotiation || Boolean(res.needs_renegotiation);
    changed = true;
    log("subscribe", { publisher_id: selectedPublisherId, track_ids: toAdd, revision });
  }
  if (toRemove.length > 0) {
    const res = await rpc("unsubscribe", { track_ids: toRemove });
    revision = res.revision;
    needsRenegotiation = needsRenegotiation || Boolean(res.needs_renegotiation);
    changed = true;
    log("unsubscribe", { publisher_id: selectedPublisherId, track_ids: toRemove, revision });
  }

  await syncSubscriptions();

  if (!changed || !needsRenegotiation) return;
  if (pc && pc.iceConnectionState !== "connected") {
    renegotiateRequested = true;
    renegotiateReason = "subscription update";
    log("subscription renegotiation deferred (ice not connected)", {
      iceState: pc ? pc.iceConnectionState : "unknown",
    });
    return;
  }
  await renegotiate("subscription update");
}

function setPolicyMode(mode) {
  currentPolicyMode = mode || null;
  if (mode) {
    el.policyMode.value = mode;
    el.policyMeta.textContent = `Policy: ${mode}`;
  } else {
    el.policyMeta.textContent = "Policy: (not connected)";
  }
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

  localStream = await navigator.mediaDevices.getUserMedia({
    audio: true,
    video: getCaptureConstraints(),
  });
  el.localVideo.srcObject = localStream;
  for (const track of localStream.getTracks()) {
    if (track.kind === "video" && el.enableSimulcast.checked) {
      try {
        pc.addTransceiver(track, {
          direction: "sendrecv",
          streams: [localStream],
          sendEncodings: [
            { rid: "q", scaleResolutionDownBy: 4.0, maxBitrate: 150_000 },
            { rid: "h", scaleResolutionDownBy: 2.0, maxBitrate: 500_000 },
            { rid: "f", scaleResolutionDownBy: 1.0, maxBitrate: 1_500_000 },
          ],
        });
        log("video sender configured", {
          simulcast: true,
          rids: ["q", "h", "f"],
          preset: el.capturePreset.value,
        });
        continue;
      } catch (error) {
        log("video simulcast setup failed; falling back to single encoding", {
          error: error.message,
        });
      }
    }
    pc.addTrack(track, localStream);
    if (track.kind === "video") {
      log("video sender configured", { simulcast: false, preset: el.capturePreset.value });
    }
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
        line.startsWith("a=rid:") ||
        line.startsWith("a=simulcast:") ||
        line.startsWith("a=extmap:") ||
        line.startsWith("a=send") ||
        line.startsWith("a=recv") ||
        line.startsWith("a=rtpmap:")
    );
}

async function collectMediaStats() {
  if (!pc) {
    renderMediaStats([]);
    return;
  }

  const stats = await pc.getStats();
  const items = [];
  const remoteVideo = document.querySelector('[data-stream-id="remote-main"] video');
  if (remoteVideo) {
    const dims = `${remoteVideo.videoWidth || 0}x${remoteVideo.videoHeight || 0}`;
    items.push(`remote element • size=${dims}`);
  }
  for (const report of stats.values()) {
    if (report.type === "outbound-rtp" && report.kind === "video" && !report.isRemote) {
      const bits = ["outbound video"];
      if (report.rid) bits.push(`rid=${report.rid}`);
      if (report.frameWidth && report.frameHeight) {
        bits.push(`size=${report.frameWidth}x${report.frameHeight}`);
      }
      if (report.framesPerSecond) bits.push(`fps=${report.framesPerSecond}`);
      if (report.qualityLimitationReason) {
        bits.push(`limit=${report.qualityLimitationReason}`);
      }
      items.push(bits.join(" • "));
    }
    if (report.type === "inbound-rtp" && report.kind === "video" && !report.isRemote) {
      const bits = ["inbound video"];
      if (report.frameWidth && report.frameHeight) {
        bits.push(`size=${report.frameWidth}x${report.frameHeight}`);
      }
      if (report.framesPerSecond) bits.push(`fps=${report.framesPerSecond}`);
      if (report.jitter) bits.push(`jitter=${Number(report.jitter).toFixed(4)}`);
      items.push(bits.join(" • "));
    }
  }
  renderMediaStats(items);
}

async function syncSubscriptions() {
  if (!participantId) return;
  try {
    const [
      parts,
      pubs,
      subs,
      streams,
      policy,
    ] = await Promise.all([
      rpc("list_participants", {}),
      rpc("list_publications", {}),
      rpc("list_subscriptions", {}),
      rpc("list_streams", {}),
      rpc("get_policy_mode", {}),
    ]);
    setPolicyMode(policy.mode);
    renderMeetingLists(
      parts.participants,
      pubs.publications,
      subs.requested_track_ids || [],
      subs.effective_track_ids || subs.track_ids || [],
      streams.streams || []
    );
    renderSubscriptionControls(
      parts.participants,
      pubs.publications || [],
      subs.requested_track_ids || []
    );
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

function startStatsLoop() {
  if (statsTimer) {
    window.clearInterval(statsTimer);
  }
  statsTimer = window.setInterval(() => {
    collectMediaStats().catch((error) => {
      log("stats failed", { error: error.message });
    });
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
  setPolicyMode("standard");

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
  startStatsLoop();
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
  if (statsTimer) {
    window.clearInterval(statsTimer);
    statsTimer = null;
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
  currentPolicyMode = null;
  latestPublications = [];
  latestRequestedTrackIds = [];
  selectedPublisherId = null;
  setPolicyMode(null);
  renderMeetingLists([], [], [], [], []);
  renderMediaStats([]);
  renderSubscriptionControls([], [], []);
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

el.publisherSelect.addEventListener("change", () => {
  selectedPublisherId = el.publisherSelect.value || null;
  renderSubscriptionControls([], latestPublications, latestRequestedTrackIds);
});

el.applySubscriptions.addEventListener("click", () => {
  applySubscriptionSelection().catch((error) => {
    log("apply subscriptions failed", { error: error.message });
  });
});

el.leave.addEventListener("click", () => {
  leave();
});

el.policyMode.addEventListener("change", () => {
  if (!participantId || !ws || ws.readyState !== WebSocket.OPEN) return;
  const mode = el.policyMode.value;
  rpc("set_policy_mode", { mode })
    .then(async (res) => {
      revision = res.revision;
      setPolicyMode(res.mode);
      log("set_policy_mode", { mode: res.mode, revision });
      await syncSubscriptions();
    })
    .catch((error) => {
      log("set_policy_mode failed", { error: error.message });
      if (currentPolicyMode) {
        el.policyMode.value = currentPolicyMode;
      }
    });
});

window.addEventListener("beforeunload", () => {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.close();
  }
});
