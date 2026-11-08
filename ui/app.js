const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

function el(tag, attrs = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") n.className = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v);
  }
  for (const k of kids) if (k != null) n.append(k);
  return n;
}

function ago(ms) {
  if (!ms) return "";
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function setMsg(id, text, kind) {
  const m = $(id);
  m.textContent = text || "";
  m.className = "msg" + (kind ? " " + kind : "");
}

async function busy(btn, fn) {
  btn.disabled = true;
  try { return await fn(); } finally { btn.disabled = false; }
}

// ---------------------------------------------------------------- setup

$("join").addEventListener("click", (e) =>
  busy(e.target, async () => {
    setMsg("join-msg", "Checking the code…");
    try {
      await invoke("join_group", { code: $("code").value, name: $("name").value });
      setMsg("join-msg", "");
      refresh();
    } catch (err) {
      setMsg("join-msg", String(err), "err");
    }
  })
);

$("create").addEventListener("click", (e) =>
  busy(e.target, async () => {
    setMsg("create-msg", "Testing those details…");
    try {
      const code = await invoke("create_group_code", {
        endpoint: $("c-endpoint").value,
        bucket: $("c-bucket").value,
        keyId: $("c-key").value,
        secret: $("c-secret").value,
      });
      setMsg("create-msg", "It works!", "ok");
      $("created-code").value = code;
      $("created").hidden = false;
    } catch (err) {
      setMsg("create-msg", String(err), "err");
    }
  })
);

$("use-created").addEventListener("click", () => {
  $("code").value = $("created-code").value;
  $("name").focus();
  window.scrollTo({ top: 0, behavior: "smooth" });
});

// ---------------------------------------------------------------- home

$("sync-now").addEventListener("click", () => invoke("sync_now"));
$("open-folder").addEventListener("click", () => invoke("open_save_folder").catch((e) => alert(e)));
$("open-backups").addEventListener("click", () => invoke("open_backups_folder").catch((e) => alert(e)));
$("show-code").addEventListener("click", async () => {
  const code = await invoke("get_group_code");
  $("my-code").value = code || "";
  $("my-code").hidden = !code;
});
$("load-deleted").addEventListener("click", async (e) => {
  await busy(e.target, async () => {
    try {
      const list = await invoke("list_deleted");
      $("deleted").replaceChildren(
        ...(list.length
          ? list.map((j) =>
              el("li", {},
                el("div", { class: "grow" }, el("div", { class: "title", title: j.stem }, j.name)),
                el("button", { onclick: () => openVersions(j) }, "Versions")
              )
            )
          : [el("li", { class: "muted" }, "Nothing deleted.")])
      );
    } catch (err) {
      $("deleted").replaceChildren(el("li", { class: "msg err" }, String(err)));
    }
  });
});
$("leave").addEventListener("click", async () => {
  if (!confirm("Stop syncing on this computer? Your saves stay where they are.")) return;
  await invoke("leave_group");
  refresh();
});

function renderHome(state) {
  const s = state.status;
  const head = $("headline");
  const sub = $("subline");
  const hostingPeer = s.peers.find((p) => p.hosting);

  if (s.hosting) {
    head.textContent = `You're hosting “${s.hosting}”`;
    head.className = "headline ok";
    sub.textContent = "It uploads automatically when you close Big Walk.";
  } else if (hostingPeer) {
    head.textContent = `${hostingPeer.player} is hosting “${hostingPeer.hosting}”`;
    head.className = "headline ok";
    sub.textContent = "Join their session. Don't host the same journey yourself.";
  } else if (s.last_error) {
    head.textContent = "Can't sync right now";
    head.className = "headline warn";
    sub.textContent = s.last_error;
  } else if (!s.last_sync_at) {
    head.textContent = "Starting…";
    head.className = "headline";
    sub.textContent = "";
  } else if (s.save_dirs.length === 0) {
    head.textContent = "Big Walk save folder not found";
    head.className = "headline warn";
    sub.textContent = "Launch Big Walk once so it creates its save folder.";
  } else if (s.journeys.every((j) => j.in_sync)) {
    head.textContent = "✓ All journeys in sync";
    head.className = "headline ok";
    sub.textContent = `Anyone can host. Last checked ${ago(s.last_sync_at)}.`;
  } else {
    head.textContent = s.game_running ? "Big Walk is open" : "Syncing…";
    head.className = "headline";
    sub.textContent = s.game_running ? "Updates wait until you close the game." : "";
  }

  // Friends
  $("peers-card").hidden = s.peers.length === 0;
  $("peers").replaceChildren(
    ...s.peers.map((p) =>
      el("li", {},
        el("span", { class: "dot " + (p.hosting ? "ok" : "") }),
        el("div", { class: "grow" },
          el("div", { class: "title" }, p.player),
          el("div", { class: "meta" }, p.hosting ? `Hosting “${p.hosting}”` : "In Big Walk")
        )
      )
    )
  );

  // Journeys
  const list = $("journeys");
  if (s.journeys.length === 0) {
    list.replaceChildren(el("li", { class: "muted" }, "No journeys yet. Host one in Big Walk and it shows up here."));
  } else {
    list.replaceChildren(
      ...s.journeys.map((j) => {
        const meta = [];
        if (j.last_uploaded_by) meta.push(`by ${j.last_uploaded_by}`);
        if (j.saved_at) meta.push(`played ${ago(j.saved_at)}`);
        const state = j.in_sync ? "In sync" : j.on_this_computer ? "Syncing" : "Downloading";
        return el("li", {},
          el("span", { class: "dot " + (j.in_sync ? "ok" : "warn"), title: state }),
          el("div", { class: "grow" },
            el("div", { class: "title", title: j.stem }, j.name),
            el("div", { class: "meta" }, meta.join(" · ") || state)
          ),
          el("button", { onclick: () => openVersions(j) }, "Versions")
        );
      })
    );
  }

  $("dirs").replaceChildren(
    ...(s.save_dirs.length ? s.save_dirs : state.candidate_dirs.map((d) => d + "  (not found)")).map((d) => el("li", {}, d))
  );
  $("who").textContent = `Signed in as ${state.player_name}.`;
  $("version").textContent = `Walk Relay ${state.app_version}`;
}

// ---------------------------------------------------------------- versions

let versionsFor = null;

async function openVersions(j) {
  versionsFor = j;
  $("v-title").textContent = `Versions of “${j.name}”`;
  $("v-list").replaceChildren(el("li", { class: "muted" }, "Loading…"));
  setMsg("v-msg", "");
  $("versions").showModal();
  try {
    const versions = await invoke("list_versions", { stem: j.stem });
    if (!versions.length) {
      $("v-list").replaceChildren(el("li", { class: "muted" }, "No earlier versions yet."));
      return;
    }
    $("v-list").replaceChildren(
      ...versions.map((v, i) =>
        el("li", {},
          el("div", { class: "grow" },
            el("div", { class: "title" }, new Date(v.at).toLocaleString()),
            el("div", { class: "meta" },
              `${v.kind === "conflict" ? "Set-aside copy from" : "Uploaded by"} ${v.player}`,
              i === 0 && v.kind === "history" ? " · current" : ""
            )
          ),
          v.kind === "conflict" ? el("span", { class: "badge" }, "other version") : null,
          el("button", { onclick: (e) => restore(e.target, v) }, "Restore")
        )
      )
    );
  } catch (err) {
    $("v-list").replaceChildren();
    setMsg("v-msg", String(err), "err");
  }
}

async function restore(btn, v) {
  if (!confirm(`Make the version from ${new Date(v.at).toLocaleString()} the current one for everyone?`)) return;
  await busy(btn, async () => {
    try {
      await invoke("restore_version", { stem: versionsFor.stem, key: v.key });
      setMsg("v-msg", "Restored. Everyone gets it on their next sync.", "ok");
    } catch (err) {
      setMsg("v-msg", String(err), "err");
    }
  });
}

$("v-close").addEventListener("click", () => $("versions").close());

// ---------------------------------------------------------------- loop

async function refresh() {
  try {
    const state = await invoke("get_state");
    $("setup").hidden = state.configured;
    $("home").hidden = !state.configured;
    if (state.configured) renderHome(state);
  } catch (err) {
    console.error(err);
  }
}

refresh();
setInterval(refresh, 2000);
