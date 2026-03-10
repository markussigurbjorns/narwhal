const defaultBaseUrl = `${window.location.protocol}//${window.location.host}`;

const el = {
  baseUrl: document.querySelector("#base-url"),
  room: document.querySelector("#room"),
  policyMode: document.querySelector("#policy-mode"),
  refresh: document.querySelector("#refresh"),
  applyPolicy: document.querySelector("#apply-policy"),
  status: document.querySelector("#status"),
  subscribers: document.querySelector("#subscribers"),
  streams: document.querySelector("#streams"),
  plans: document.querySelector("#plans"),
  edges: document.querySelector("#edges"),
  rawJson: document.querySelector("#raw-json"),
};

el.baseUrl.value = defaultBaseUrl;

function setStatus(text) {
  el.status.textContent = text;
}

function renderList(target, items) {
  target.textContent = "";
  if (!items || items.length === 0) {
    const li = document.createElement("li");
    li.textContent = "(none)";
    target.appendChild(li);
    return;
  }
  for (const item of items) {
    const li = document.createElement("li");
    li.textContent = item;
    target.appendChild(li);
  }
}

function controlUrl(path) {
  return new URL(path, el.baseUrl.value.trim() || defaultBaseUrl).toString();
}

async function fetchJson(url, options = {}) {
  const response = await fetch(url, options);
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`${response.status} ${response.statusText}: ${text}`);
  }
  return response.json();
}

function renderInspection(data) {
  el.policyMode.value = data.mode || "standard";
  renderList(el.subscribers, data.subscriber_ids || []);
  renderList(
    el.streams,
    (data.stream_infos || []).map((stream) => `${stream.media_key} • ${stream.caps}`)
  );
  renderList(
    el.plans,
    (data.subscriber_plans || []).map((plan) => {
      const audio = (plan.audio_track_ids || []).join(", ") || "none";
      const video = (plan.video || [])
        .map((item) => `${item.track_id}:${item.target}`)
        .join(", ") || "none";
      return `${plan.subscriber_id} • audio=[${audio}] • video=[${video}]`;
    })
  );
  renderList(
    el.edges,
    (data.graph_edges || []).map((edge) => {
      const video = edge.video_target ? ` • target=${edge.video_target}` : "";
      return `${edge.track_id} -> ${edge.subscriber_id} • ${edge.kind}${video}`;
    })
  );
  el.rawJson.textContent = JSON.stringify(data, null, 2);
}

async function refreshInspection() {
  const room = el.room.value.trim();
  if (!room) {
    setStatus("Room is required");
    return;
  }
  setStatus("Refreshing");
  try {
    const data = await fetchJson(controlUrl(`/control/rooms/${encodeURIComponent(room)}/broadcast`));
    renderInspection(data);
    setStatus(`Loaded ${room}`);
  } catch (error) {
    setStatus(error.message);
  }
}

async function applyPolicy() {
  const room = el.room.value.trim();
  if (!room) {
    setStatus("Room is required");
    return;
  }
  setStatus("Applying policy");
  try {
    await fetchJson(controlUrl(`/control/rooms/${encodeURIComponent(room)}/broadcast/policy`), {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ mode: el.policyMode.value }),
    });
    await refreshInspection();
    setStatus(`Policy updated for ${room}`);
  } catch (error) {
    setStatus(error.message);
  }
}

el.refresh.addEventListener("click", () => {
  refreshInspection().catch((error) => {
    setStatus(error.message);
  });
});

el.applyPolicy.addEventListener("click", () => {
  applyPolicy().catch((error) => {
    setStatus(error.message);
  });
});

refreshInspection().catch((error) => {
  setStatus(error.message);
});
