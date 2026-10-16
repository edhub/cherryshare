// 页面逻辑：跟守护进程要 JSON，画出来。没有构建步骤，也不引任何框架。
//
// 只有两个接口：`_api/list`（可带 `?path=` 进子目录）和文件本身（就是链接）。

const ID = location.pathname.split("/")[2] ?? "";
const root = document.getElementById("root");
const viewer = document.getElementById("viewer");
const viewerTitle = document.getElementById("viewer-title");
const viewerBody = document.getElementById("viewer-body");
const mountPanel = document.getElementById("mount-panel");

/** 文件名之类要放进 HTML 的，先转义。 */
function esc(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function size(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function when(sec) {
  const d = new Date(sec * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 文件在篮子内的相对路径 → URL。每一段各自编码，斜杠留着。 */
function urlOf(path, name) {
  const full = path ? `${path}/${name}` : name;
  return `/b/${ID}/${full.split("/").map(encodeURIComponent).join("/")}`;
}

/** 按扩展名决定点名字干什么。看不出来的不动（点了没反应）。 */
function kindOf(name) {
  const ext = (name.split(".").pop() ?? "").toLowerCase();
  if (["mp4", "m4v", "mov", "webm", "ogv"].includes(ext)) return "video";
  if (["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "svg"].includes(ext)) return "image";
  if ([
    "txt", "md", "log", "json", "csv", "tsv", "yml", "yaml", "toml", "ini", "conf", "cfg",
    "sh", "bash", "zsh", "py", "rs", "go", "js", "mjs", "ts", "css", "html", "xml", "sql",
  ].includes(ext)) return "text";
  return "other";
}

/** 复制：http 不算安全上下文，现代那个 Clipboard API 用不了，退回老办法。 */
async function copy(text, button) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  const old = button.textContent;
  button.textContent = "复制好了";
  setTimeout(() => { button.textContent = old; }, 1200);
}

/** 来访者是什么系统——决定"复制"按钮给哪一段（Windows 上没有 NFS，落到第一段）。 */
function visitorOS() {
  const p = String(navigator.userAgentData?.platform ?? navigator.platform ?? "").toLowerCase();
  if (p.includes("linux")) return "Linux";
  return "macOS";
}

// ---- 在线看：视频、图片、纯文本 ----

const TEXT_LIMIT = 2 * 1024 * 1024;

function closeViewer() {
  // 清掉内容：视频得真的停下来，不然关掉了还在后面响。
  viewerBody.innerHTML = "";
  viewer.close();
}

function showViewer(title, html) {
  viewerTitle.textContent = title;
  viewerBody.innerHTML = html;
  viewer.showModal();
}

async function openFile(entry, url) {
  const kind = kindOf(entry.name);
  if (kind === "video") {
    showViewer(entry.name, `<video class="w-full rounded-box bg-black" controls autoplay src="${url}"></video>`);
  } else if (kind === "image") {
    showViewer(entry.name, `<img class="max-h-[70vh] mx-auto rounded-box" alt="${esc(entry.name)}" src="${url}">`);
  } else if (kind === "text") {
    if (entry.size > TEXT_LIMIT) {
      showViewer(entry.name, `<p class="opacity-70">文件太大（${size(entry.size)}），别在这儿看了。</p>`);
      return;
    }
    showViewer(entry.name, `<p class="opacity-60 text-sm">读取中…</p>`);
    try {
      const text = await (await fetch(url)).text();
      // 纯文本：转义了放进 <pre>，不解释任何标记。
      viewerBody.innerHTML = `<pre class="text-xs whitespace-pre-wrap break-words max-h-[70vh] overflow-auto bg-base-200 rounded-box p-3">${esc(text)}</pre>`;
    } catch (e) {
      viewerBody.innerHTML = `<p class="text-error">读不到：${esc(e)}</p>`;
    }
  }
}

viewer.addEventListener("close", () => { viewerBody.innerHTML = ""; });

// ---- 列表 ----

function fileRow(data, entry) {
  const url = urlOf(data.path, entry.name);
  const icon = entry.kind === "dir" ? "📁" : "📄";

  if (entry.kind === "dir") {
    const href = `?path=${encodeURIComponent(data.path ? `${data.path}/${entry.name}` : entry.name)}`;
    return `<tr>
      <td><a class="link link-hover font-medium" href="${href}">${icon} ${esc(entry.name)}</a></td>
      <td class="text-right opacity-70 whitespace-nowrap">文件夹</td>
      <td class="text-right opacity-60 whitespace-nowrap hidden sm:table-cell">${when(entry.mtime)}</td>
    </tr>`;
  }

  // 名字本身按类型给动作；下载是旁边那个单独的按钮，不藏在名字里。
  const kind = kindOf(entry.name);
  const name = kind === "other"
    ? `<span class="font-medium">${icon} ${esc(entry.name)}</span>`
    : `<a class="link link-hover font-medium" href="${url}" data-open="1">${icon} ${esc(entry.name)}</a>`;
  return `<tr>
    <td>
      ${name}
      <a class="btn btn-ghost btn-xs ml-1" href="${url}" download="${esc(entry.name)}" title="下载">下载</a>
    </td>
    <td class="text-right tabular-nums opacity-70 whitespace-nowrap">${size(entry.size)}</td>
    <td class="text-right opacity-60 whitespace-nowrap hidden sm:table-cell">${when(entry.mtime)}</td>
  </tr>`;
}

function crumbsOf(data) {
  if (!data.path) return "";
  const segs = data.path.split("/");
  const parts = [`<a class="link link-hover" href="?">${esc(data.name)}</a>`];
  segs.forEach((seg, i) => {
    const upto = segs.slice(0, i + 1).join("/");
    parts.push(`<a class="link link-hover" href="?path=${encodeURIComponent(upto)}">${esc(seg)}</a>`);
  });
  return `<div class="text-sm opacity-70">${parts.join('<span class="opacity-40 mx-1">/</span>')}</div>`;
}

function listCard(data) {
  const rows = data.cherries.map((e) => fileRow(data, e)).join("");
  return `
    <section class="card bg-base-100 shadow-sm">
      <div class="card-body">
        ${crumbsOf(data)}
        <table class="table table-sm">
          <thead><tr>
            <th>名字</th><th class="text-right">大小</th>
            <th class="text-right hidden sm:table-cell">改过</th>
          </tr></thead>
          <tbody>${rows || `<tr><td colspan="3" class="opacity-60">这里什么都没有。</td></tr>`}</tbody>
        </table>
      </div>
    </section>`;
}

// ---- 挂载说明（右上角的复制按钮 + 展开面板） ----

let mounts = [];
let expanded = false;

function pickedMount() {
  return mounts.find((m) => m.label === visitorOS()) ?? mounts[0];
}

function mountPanelHtml() {
  const tabs = mounts.map((m, i) => `
    <input type="radio" name="os" role="tab" class="tab" aria-label="${esc(m.label)}" ${i === 0 ? "checked" : ""}>
    <div role="tabpanel" class="tab-content bg-base-100 border-base-300 rounded-box p-4">
      <pre class="text-xs whitespace-pre-wrap break-all">${esc(m.command)}</pre>
      <div class="mt-3 flex gap-2 items-center">
        <button class="btn btn-sm btn-primary" data-copy="${esc(m.command)}">复制这行</button>
        <span class="text-xs opacity-60">卸掉：<code>${esc(m.unmount)}</code></span>
      </div>
    </div>`).join("");
  return `
    <section class="card bg-base-100 shadow-sm">
      <div class="card-body">
        <h2 class="card-title text-base">想当磁盘用</h2>
        <p class="text-sm opacity-70">
          在终端里粘一行就行，装完就在 <code>${esc(mounts[0]?.mountpoint ?? "")}</code> 下面，
          跟本地目录一样能用（Finder 里也能看到）。
        </p>
        <div role="tablist" class="tabs tabs-lift mt-2">${tabs}</div>
      </div>
    </section>`;
}

function bindCopyButtons(scope) {
  scope.querySelectorAll("[data-copy]").forEach((b) =>
    b.addEventListener("click", () => copy(b.dataset.copy, b))
  );
}

function toggleMount(force) {
  expanded = force ?? !expanded;
  mountPanel.classList.toggle("hidden", !expanded);
  document.getElementById("toggle-mount").setAttribute("aria-expanded", String(expanded));
  document.getElementById("chevron").textContent = expanded ? "▴" : "▾";
}

// ---- 状态 ----

function fail(message, status) {
  root.innerHTML = `<div class="alert alert-error"><span>${esc(message)}（HTTP ${status}）</span></div>`;
}

async function load(path = "") {
  const query = path ? `?path=${encodeURIComponent(path)}` : "";
  let res;
  try {
    res = await fetch(`/b/${ID}/_api/list${query}`);
  } catch (e) {
    fail(`拿不到篮子内容：${e}`, "—");
    return;
  }
  if (!res.ok) {
    fail((await res.text()).trim(), res.status);
    return;
  }
  const data = await res.json();
  document.title = `${data.name} — CherryShare`;
  document.getElementById("basket").textContent = data.name;

  mounts = data.mounts;
  mountPanel.innerHTML = mountPanelHtml();
  bindCopyButtons(mountPanel);
  const quick = document.getElementById("copy-mount");
  quick.disabled = false;
  quick.onclick = (e) => {
    const m = pickedMount();
    if (m) copy(m.command, e.currentTarget);
  };

  root.innerHTML = listCard(data);
  root.querySelectorAll("a[data-open]").forEach((a) => {
    const entry = data.cherries.find((c) => urlOf(data.path, c.name) === a.getAttribute("href"));
    if (!entry) return;
    a.addEventListener("click", (ev) => {
      ev.preventDefault();
      openFile(entry, a.getAttribute("href"));
    });
  });
}

document.getElementById("toggle-mount").addEventListener("click", () => toggleMount());
addEventListener("popstate", () => load(new URLSearchParams(location.search).get("path") ?? ""));
load(new URLSearchParams(location.search).get("path") ?? "");
