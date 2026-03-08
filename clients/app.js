const defaultBase = `${window.location.protocol}//${window.location.host}`;
const defaultStunUrl = "stun:turn.makudoku.com:3478";

function createLogger(el, statusEl, prefix) {
  return {
    setStatus(message) {
      statusEl.textContent = message;
    },
    log(message, data) {
      const ts = new Date().toLocaleTimeString();
      const suffix = data === undefined ? "" : ` ${JSON.stringify(data)}`;
      el.textContent += `[${ts}] ${prefix} ${message}${suffix}\n`;
      el.scrollTop = el.scrollHeight;
    },
    clear() {
      el.textContent = "";
    },
  };
}

function summarizeSdp(label, sdp, log) {
  const lines = sdp
    .split(/\r?\n/)
    .filter((line) =>
      line.startsWith("m=") ||
      line.startsWith("a=mid:") ||
      line.startsWith("a=send") ||
      line.startsWith("a=recv") ||
      line.startsWith("a=group:BUNDLE")
    );
  log.log(label, { lines });
}

function logFullSdp(label, sdp, log) {
  log.log(label, { sdp });
}

async function ensurePlaying(video, log) {
  try {
    await video.play();
    log.log("video.play() resolved");
  } catch (error) {
    log.log("video.play() failed", { error: error.message });
  }
}

function buildUrl(base, path) {
  return new URL(path, base).toString();
}

function getSearchParams() {
  return new URLSearchParams(window.location.search);
}

function buildIceServers(params = getSearchParams()) {
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

function currentView(params = getSearchParams()) {
  const view = params.get("view");
  if (view === "subscribe" && params.get("room")) {
    return view;
  }
  if (view === "publish") {
    return view;
  }
  return "publish";
}

function syncViewUrl(view, extra = {}) {
  const url = new URL(window.location.href);
  const params = url.searchParams;

  if (view === "subscribe") {
    params.set("view", view);
  } else {
    params.delete("view");
  }

  for (const [key, value] of Object.entries(extra)) {
    if (value) {
      params.set(key, value);
    } else {
      params.delete(key);
    }
  }

  window.history.replaceState({}, "", url);
}

function buildSubscriberPageUrl(base, room) {
  const url = new URL(window.location.href);
  url.search = "";
  url.hash = "";
  url.searchParams.set("view", "subscribe");
  url.searchParams.set("room", room);
  url.searchParams.set("base", base);
  return url.toString();
}

async function readError(response) {
  const text = await response.text();
  throw new Error(`${response.status} ${response.statusText}: ${text}`);
}

async function postSdp(url, sdp) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/sdp" },
    body: sdp,
  });

  if (!response.ok) {
    await readError(response);
  }

  return {
    answer: await response.text(),
    location: response.headers.get("location"),
  };
}

async function patchIce(resourceUrl, candidate) {
  const response = await fetch(resourceUrl, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      mline_index: candidate.sdpMLineIndex ?? 0,
      candidate: candidate.candidate,
    }),
  });

  if (!response.ok) {
    await readError(response);
  }
}

async function getIce(resourceUrl) {
  const response = await fetch(`${resourceUrl}/ice`);
  if (!response.ok) {
    await readError(response);
  }
  return response.json();
}

async function deleteSession(resourceUrl) {
  const response = await fetch(resourceUrl, { method: "DELETE" });
  if (!response.ok && response.status !== 404) {
    await readError(response);
  }
}

function resolveBase(input) {
  return input.value.trim() || defaultBase;
}

function startIcePolling(pc, resourceUrl, log) {
  const seen = new Set();
  let timer = null;

  async function poll() {
    try {
      const candidates = await getIce(resourceUrl);
      for (const candidate of candidates) {
        const key = `${candidate.mline_index}:${candidate.candidate}`;
        if (seen.has(key)) {
          continue;
        }
        seen.add(key);
        await pc.addIceCandidate({
          sdpMLineIndex: candidate.mline_index,
          candidate: candidate.candidate,
        });
        log.log("received remote ICE", candidate);
      }
    } catch (error) {
      log.log("ICE poll failed", { error: error.message });
    }
  }

  timer = window.setInterval(poll, 1000);
  poll();

  return () => {
    if (timer !== null) {
      window.clearInterval(timer);
    }
  };
}

function startStatsPolling(pc, log, prefix) {
  let timer = null;

  async function poll() {
    try {
      const stats = await pc.getStats();
      const snapshot = [];

      stats.forEach((report) => {
        if (report.type === "inbound-rtp" && !report.isRemote) {
          snapshot.push({
            kind: report.kind,
            bytesReceived: report.bytesReceived,
            packetsReceived: report.packetsReceived,
            packetsLost: report.packetsLost,
            framesDecoded: report.framesDecoded,
            keyFramesDecoded: report.keyFramesDecoded,
            frameWidth: report.frameWidth,
            frameHeight: report.frameHeight,
          });
        }

        if (report.type === "transport") {
          snapshot.push({
            kind: "transport",
            iceState: report.iceState,
            dtlsState: report.dtlsState,
            bytesReceived: report.bytesReceived,
            bytesSent: report.bytesSent,
          });
        }
      });

      if (snapshot.length > 0) {
        log.log(`${prefix} stats`, snapshot);
      }
    } catch (error) {
      log.log(`${prefix} stats failed`, { error: error.message });
    }
  }

  timer = window.setInterval(poll, 2000);
  poll();

  return () => {
    if (timer !== null) {
      window.clearInterval(timer);
    }
  };
}

function setMediaState(buttons, running) {
  buttons.start.disabled = running;
  buttons.stop.disabled = !running;
}

async function flushPendingCandidates(resourceUrl, pendingCandidates, log) {
  while (pendingCandidates.length > 0) {
    const candidate = pendingCandidates.shift();
    await patchIce(resourceUrl, candidate);
    log.log("sent local ICE", {
      mline_index: candidate.sdpMLineIndex,
      candidate: candidate.candidate,
    });
  }
}

function wireNavigation() {
  const title = document.querySelector("#page-title");
  const description = document.querySelector("#page-description");
  const views = {
    publish: document.querySelector("#view-publish"),
    subscribe: document.querySelector("#view-subscribe"),
  };

  const copy = {
    publish: {
      title: "Create A Room",
      description: "Start publishing into a room, then send the generated viewer link to subscribers.",
    },
    subscribe: {
      title: "Subscriber View",
      description: "Connect to a room as a WHEP client through a publisher-generated link.",
    },
  };

  function show(view) {
    for (const [name, el] of Object.entries(views)) {
      el.classList.toggle("active", name === view);
    }

    title.textContent = copy[view].title;
    description.textContent = copy[view].description;
    if (view === "publish") {
      syncViewUrl("publish", { room: "", base: "" });
    }
  }

  show(currentView());

  return { show };
}

function wirePublisher() {
  const roomInput = document.querySelector("#pub-room");
  const baseInput = document.querySelector("#pub-base");
  const startButton = document.querySelector("#pub-start");
  const stopButton = document.querySelector("#pub-stop");
  const shareLinkInput = document.querySelector("#pub-share-link");
  const shareStatus = document.querySelector("#pub-share-status");
  const copyLinkButton = document.querySelector("#pub-copy-link");
  const openLinkButton = document.querySelector("#pub-open-link");
  const video = document.querySelector("#local-video");
  const log = createLogger(
    document.querySelector("#pub-log"),
    document.querySelector("#pub-status"),
    "PUB"
  );

  baseInput.value = defaultBase;

  let pc = null;
  let stream = null;
  let resourceUrl = null;
  let stopIcePolling = null;
  let pendingCandidates = [];

  function updateShareLink() {
    const room = roomInput.value.trim();
    const base = resolveBase(baseInput);

    if (!room) {
      shareLinkInput.value = "";
      shareStatus.textContent = "Set a room to generate a subscriber link.";
      openLinkButton.disabled = true;
      copyLinkButton.disabled = true;
      return;
    }

    shareLinkInput.value = buildSubscriberPageUrl(base, room);
    shareStatus.textContent = "Share this viewer link before or after you start publishing.";
    openLinkButton.disabled = false;
    copyLinkButton.disabled = false;
  }

  async function stop() {
    if (stopIcePolling) {
      stopIcePolling();
      stopIcePolling = null;
    }

    if (resourceUrl) {
      try {
        await deleteSession(resourceUrl);
        log.log("deleted WHIP session");
      } catch (error) {
        log.log("delete failed", { error: error.message });
      }
    }

    if (pc) {
      pc.close();
      pc = null;
    }

    if (stream) {
      for (const track of stream.getTracks()) {
        track.stop();
      }
      stream = null;
    }

    video.srcObject = null;
    resourceUrl = null;
    pendingCandidates = [];
    log.setStatus("Idle");
    setMediaState({ start: startButton, stop: stopButton }, false);
  }

  roomInput.addEventListener("input", updateShareLink);
  baseInput.addEventListener("input", updateShareLink);

  copyLinkButton.addEventListener("click", async () => {
    if (!shareLinkInput.value) {
      return;
    }

    try {
      await navigator.clipboard.writeText(shareLinkInput.value);
      shareStatus.textContent = "Subscriber link copied.";
    } catch (error) {
      shareStatus.textContent = `Copy failed: ${error.message}`;
    }
  });

  openLinkButton.addEventListener("click", () => {
    if (!shareLinkInput.value) {
      return;
    }

    window.open(shareLinkInput.value, "_blank", "noopener,noreferrer");
  });

  startButton.addEventListener("click", async () => {
    try {
      log.clear();
      log.setStatus("Requesting camera and microphone");
      setMediaState({ start: startButton, stop: stopButton }, true);

      stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: true });
      video.srcObject = stream;

      pc = new RTCPeerConnection({ iceServers: buildIceServers() });
      stream.getTracks().forEach((track) => pc.addTrack(track, stream));

      pc.addEventListener("connectionstatechange", () => {
        log.setStatus(`Peer connection: ${pc.connectionState}`);
        log.log("connection state", { state: pc.connectionState });
      });

      pc.addEventListener("iceconnectionstatechange", () => {
        log.log("ice connection state", { state: pc.iceConnectionState });
      });

      pc.addEventListener("icecandidate", async (event) => {
        if (!event.candidate || !event.candidate.candidate) {
          return;
        }

        if (!resourceUrl) {
          pendingCandidates.push(event.candidate);
          log.log("queued local ICE", {
            mline_index: event.candidate.sdpMLineIndex,
            candidate: event.candidate.candidate,
          });
          return;
        }

        try {
          await patchIce(resourceUrl, event.candidate);
          log.log("sent local ICE", {
            mline_index: event.candidate.sdpMLineIndex,
            candidate: event.candidate.candidate,
          });
        } catch (error) {
          log.log("ICE patch failed", { error: error.message });
        }
      });

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      log.log("created local offer");
      summarizeSdp("local offer summary", offer.sdp, log);
      logFullSdp("local offer full", offer.sdp, log);

      const base = resolveBase(baseInput);
      const room = roomInput.value.trim();
      const { answer, location } = await postSdp(buildUrl(base, `/whip/${room}`), offer.sdp);
      resourceUrl = buildUrl(base, location);
      log.log("created WHIP session", { location: resourceUrl });
      await flushPendingCandidates(resourceUrl, pendingCandidates, log);

      await pc.setRemoteDescription({ type: "answer", sdp: answer });
      log.log("applied remote answer");
      summarizeSdp("remote answer summary", answer, log);
      logFullSdp("remote answer full", answer, log);

      stopIcePolling = startIcePolling(pc, resourceUrl, log);
      log.setStatus("Publishing");
      updateShareLink();
    } catch (error) {
      log.log("start failed", { error: error.message });
      await stop();
    }
  });

  stopButton.addEventListener("click", () => {
    stop();
  });

  window.addEventListener("beforeunload", () => {
    stop();
  });

  updateShareLink();
}

function wireSubscriber() {
  const roomInput = document.querySelector("#sub-room");
  const baseInput = document.querySelector("#sub-base");
  const startButton = document.querySelector("#sub-start");
  const stopButton = document.querySelector("#sub-stop");
  const roomSummary = document.querySelector("#sub-room-summary");
  const video = document.querySelector("#remote-video");
  const log = createLogger(
    document.querySelector("#sub-log"),
    document.querySelector("#sub-status"),
    "SUB"
  );

  const params = getSearchParams();
  roomInput.value = params.get("room") || "test-room";
  baseInput.value = params.get("base") || defaultBase;

  let pc = null;
  let resourceUrl = null;
  let stopIcePolling = null;
  let stopStatsPolling = null;
  let pendingCandidates = [];

  function updateRoomSummary() {
    const room = roomInput.value.trim();
    const base = resolveBase(baseInput);
    roomSummary.textContent = room
      ? `Ready to subscribe to room "${room}" on ${base}.`
      : "No room selected yet.";

    if (currentView() === "subscribe") {
      syncViewUrl("subscribe", { room, base });
    }
  }

  async function stop() {
    if (stopIcePolling) {
      stopIcePolling();
      stopIcePolling = null;
    }

    if (stopStatsPolling) {
      stopStatsPolling();
      stopStatsPolling = null;
    }

    if (resourceUrl) {
      try {
        await deleteSession(resourceUrl);
        log.log("deleted WHEP session");
      } catch (error) {
        log.log("delete failed", { error: error.message });
      }
    }

    if (pc) {
      pc.close();
      pc = null;
    }

    video.srcObject = null;
    resourceUrl = null;
    pendingCandidates = [];
    log.setStatus("Idle");
    setMediaState({ start: startButton, stop: stopButton }, false);
  }

  roomInput.addEventListener("input", updateRoomSummary);
  baseInput.addEventListener("input", updateRoomSummary);

  startButton.addEventListener("click", async () => {
    try {
      log.clear();
      log.setStatus("Creating recvonly peer");
      setMediaState({ start: startButton, stop: stopButton }, true);

      pc = new RTCPeerConnection({ iceServers: buildIceServers() });
      pc.addTransceiver("audio", { direction: "recvonly" });
      pc.addTransceiver("video", { direction: "recvonly" });

      const remoteStream = new MediaStream();
      video.srcObject = remoteStream;
      void ensurePlaying(video, log);

      video.addEventListener("loadedmetadata", () => {
        log.log("remote video loadedmetadata", {
          width: video.videoWidth,
          height: video.videoHeight,
          readyState: video.readyState,
        });
        void ensurePlaying(video, log);
      });

      video.addEventListener("playing", () => {
        log.log("remote video playing", {
          width: video.videoWidth,
          height: video.videoHeight,
          readyState: video.readyState,
        });
      });

      pc.addEventListener("track", (event) => {
        remoteStream.addTrack(event.track);
        log.log("received remote track", {
          kind: event.track.kind,
          id: event.track.id,
        });
        event.track.addEventListener("mute", () => {
          log.log("remote track muted", {
            kind: event.track.kind,
            id: event.track.id,
          });
        });
        event.track.addEventListener("unmute", () => {
          log.log("remote track unmuted", {
            kind: event.track.kind,
            id: event.track.id,
          });
        });
        event.track.addEventListener("ended", () => {
          log.log("remote track ended", {
            kind: event.track.kind,
            id: event.track.id,
          });
        });
        void ensurePlaying(video, log);
      });

      pc.addEventListener("connectionstatechange", () => {
        log.setStatus(`Peer connection: ${pc.connectionState}`);
        log.log("connection state", { state: pc.connectionState });
      });

      pc.addEventListener("iceconnectionstatechange", () => {
        log.log("ice connection state", { state: pc.iceConnectionState });
      });

      pc.addEventListener("icecandidate", async (event) => {
        if (!event.candidate || !event.candidate.candidate) {
          return;
        }

        if (!resourceUrl) {
          pendingCandidates.push(event.candidate);
          log.log("queued local ICE", {
            mline_index: event.candidate.sdpMLineIndex,
            candidate: event.candidate.candidate,
          });
          return;
        }

        try {
          await patchIce(resourceUrl, event.candidate);
          log.log("sent local ICE", {
            mline_index: event.candidate.sdpMLineIndex,
            candidate: event.candidate.candidate,
          });
        } catch (error) {
          log.log("ICE patch failed", { error: error.message });
        }
      });

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      log.log("created local offer");
      summarizeSdp("local offer summary", offer.sdp, log);
      logFullSdp("local offer full", offer.sdp, log);

      const base = resolveBase(baseInput);
      const room = roomInput.value.trim();
      const { answer, location } = await postSdp(buildUrl(base, `/whep/${room}`), offer.sdp);
      resourceUrl = buildUrl(base, location);
      log.log("created WHEP session", { location: resourceUrl });
      await flushPendingCandidates(resourceUrl, pendingCandidates, log);

      await pc.setRemoteDescription({ type: "answer", sdp: answer });
      log.log("applied remote answer");
      summarizeSdp("remote answer summary", answer, log);
      logFullSdp("remote answer full", answer, log);

      stopIcePolling = startIcePolling(pc, resourceUrl, log);
      stopStatsPolling = startStatsPolling(pc, log, "subscriber");
      log.setStatus("Subscribed");
      updateRoomSummary();
    } catch (error) {
      log.log("start failed", { error: error.message });
      await stop();
    }
  });

  stopButton.addEventListener("click", () => {
    stop();
  });

  window.addEventListener("beforeunload", () => {
    stop();
  });

  updateRoomSummary();
}

const navigation = wireNavigation();
wirePublisher();
wireSubscriber();
