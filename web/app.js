// 页面逻辑：跟守护进程要 JSON，画出来。没有构建步骤，也不引任何框架。
//
// 接口就三个：`_api/list`（可带 `?path=` 进子目录）、`_api/size`（一个目录多大），
// 以及文件本身（就是链接）。目录想打包就加 `?dl=zip`。

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

// 关掉时清空：视频得真的停下来，不然关掉了还在后面响。
viewer.addEventListener("close", () => {
  viewerBody.innerHTML = "";
});

// ---- 目录大小（现算，缓在 session 里）----
//
// 只对目录行问，不问当前这一层。服务端有预算，超了就说「文件太多」。
// 结果是缓在 sessionStorage 的：来回跳转不用重问，刷新（含强刷）就当重新看一遍。

const SIZES_KEY = `cherry-sizes-${ID}`;

/** 取缓存。判断是不是刷新靠 Navigation Timing；判不出来就**整个不用缓存**——
 *  宁可没缓存，也不要出现「刷新了还是旧数字」。 */
function readSizes() {
  const nav = performance.getEntriesByType?.("navigation")?.[0];
  const type = nav ? nav.type : performance.navigation ? (performance.navigation.type === 1 ? "reload" : "") : "";
  if (type === "") return {};
  try {
    if (type === "reload") sessionStorage.removeItem(SIZES_KEY);
    return JSON.parse(sessionStorage.getItem(SIZES_KEY) ?? "{}");
  } catch {
    // 存不了（隐私模式之类）：那就不用缓存，每次重问。
    return {};
  }
}

const sizes = readSizes();

function rememberSize(path, val) {
  sizes[path] = val;
  try {
    sessionStorage.setItem(SIZES_KEY, JSON.stringify(sizes));
  } catch {
    // 存不下（隐私模式、配额满了）就算了，下次再问一遍就是。
  }
}

/** 一次最多问几个。服务端也封着顶（`MAX_SIZES`），两边一个量级。 */
const SIZE_CONCURRENCY = 6;
const sizeQueue = [];
let sizeRunning = 0;

function askSize(path, fill) {
  if (sizes[path]) {
    fill(sizes[path]);
    return;
  }
  sizeQueue.push({ path, fill });
  pumpSizes();
}

function pumpSizes() {
  while (sizeRunning < SIZE_CONCURRENCY && sizeQueue.length) {
    const { path, fill } = sizeQueue.shift();
    sizeRunning++;
    fetch(`/b/${ID}/_api/size?path=${encodeURIComponent(path)}`)
      .then((r) => (r.ok ? r.json() : null))
      .then((v) => {
        if (!v) {
          fill(null);
          return;
        }
        // 「文件太多」也记住：它不会变小。但网络出错那种不记，下次再试。
        if (v.stop === "done" || v.stop === "too_many") rememberSize(path, v);
        fill(v);
      })
      .catch(() => fill(null))
      .finally(() => {
        sizeRunning--;
        pumpSizes();
      });
  }
}

/** 滚到看得见才去问。`rootMargin` 给一点提前量，滚下去的时候数字已经在了。 */
function watchForSizes(tds) {
  if (typeof IntersectionObserver === "undefined") {
    // 老浏览器：没有「看得见」这个概念可用，那就全都问（服务端照样有预算）。
    tds.forEach(fillSize);
    return;
  }
  const watcher = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        watcher.unobserve(e.target);
        fillSize(e.target);
      }
    },
    { rootMargin: "200px" },
  );
  tds.forEach((td) => watcher.observe(td));
}

function fillSize(td) {
  askSize(td.dataset.size, (v) => {
    td.innerHTML = sizeCell(v);
  });
}

/** 万以上按「万」写，一眼看得出量级。 */
function count(n) {
  if (n < 10000) return String(n);
  return `${(n / 10000).toFixed(1).replace(/\.0$/, "")} 万`;
}

function sizeCell(v) {
  if (v === undefined) return `<span class="opacity-40">…</span>`;
  if (v === null) return `<span class="opacity-60" title="没算出来，刷新页面会再试一次">算不了</span>`;
  if (v.stop !== "done") {
    return `<span class="opacity-60" title="超过 1 万个文件就不细算了，免得占着对方的电脑">文件太多</span>`;
  }
  return `${size(v.bytes)}<span class="opacity-50 text-xs">（${count(v.files)} 个文件）</span>`;
}

// ---- 列表 ----

function fileRow(data, entry) {
  const url = urlOf(data.path, entry.name);
  const icon = entry.kind === "dir" ? "📁" : "📄";

  if (entry.kind === "dir") {
    const href = `?path=${encodeURIComponent(data.path ? `${data.path}/${entry.name}` : entry.name)}`;
    const inside = data.path ? `${data.path}/${entry.name}` : entry.name;
    // 名字点进去；把整个文件夹打包下载是右边那一列。大小那一格先空着：服务端得走一遍
    // 才知道（`_api/size`），回来再填。
    return `<tr>
      <td><a class="link link-hover font-medium" href="${href}">${icon} ${esc(entry.name)}</a></td>
      <td class="tabular-nums opacity-70 whitespace-nowrap" data-size="${esc(inside)}">${sizeCell(undefined)}</td>
      <td class="text-right opacity-60 whitespace-nowrap hidden sm:table-cell">${when(entry.mtime)}</td>
      <td class="text-right whitespace-nowrap">
        <a class="btn btn-ghost btn-xs" href="${url}?dl=zip" title="把这个文件夹打包成一个 zip 下载">下载</a>
      </td>
    </tr>`;
  }

  // 名字本身按类型给动作（视频图片在线看、文本按纯文本看）；下载不藏在名字里，
  // 它是右边那一列，位置固定——眼睛能学会「要拿就在最右边」。
  const kind = kindOf(entry.name);
  const name = kind === "other"
    ? `<span class="font-medium">${icon} ${esc(entry.name)}</span>`
    : `<a class="link link-hover font-medium" href="${url}" data-open="1">${icon} ${esc(entry.name)}</a>`;
  return `<tr>
    <td>${name}</td>
    <td class="tabular-nums opacity-70 whitespace-nowrap">${size(entry.size)}</td>
    <td class="text-right opacity-60 whitespace-nowrap hidden sm:table-cell">${when(entry.mtime)}</td>
    <td class="text-right whitespace-nowrap">
      <a class="btn btn-ghost btn-xs" href="${url}" download="${esc(entry.name)}" title="下载">下载</a>
    </td>
  </tr>`;
}

function crumbsOf(data) {
  const segs = data.path ? data.path.split("/") : [];
  const parts = [`<a class="link link-hover" href="?">${esc(data.name)}</a>`];
  segs.forEach((seg, i) => {
    const upto = segs.slice(0, i + 1).join("/");
    parts.push(`<a class="link link-hover" href="?path=${encodeURIComponent(upto)}">${esc(seg)}</a>`);
  });
  // 当前这一层整个打包。根那层也要带尾斜杠，不然会落到页面那条路由上。
  const here = `/b/${ID}/${segs.map(encodeURIComponent).join("/")}${segs.length ? "/" : ""}?dl=zip`;
  return `<div class="flex items-center gap-2">
    <div class="text-sm opacity-70 min-w-0 truncate">${parts.join('<span class="opacity-40 mx-1">/</span>')}</div>
    <a class="btn btn-ghost btn-xs ml-auto shrink-0" href="${here}" title="把这一层整个打包成一个 zip 下载">下载</a>
  </div>`;
}

function listCard(data) {
  const rows = data.cherries.map((e) => fileRow(data, e)).join("");
  return `
    <section class="card bg-base-100 shadow-sm">
      <div class="card-body">
        ${crumbsOf(data)}
        <table class="table table-sm">
          <thead><tr>
            <th>名字</th><th>大小</th>
            <th class="text-right hidden sm:table-cell">改过</th>
            <th></th>
          </tr></thead>
          <tbody>${rows || `<tr><td colspan="4" class="opacity-60">这里什么都没有。</td></tr>`}</tbody>
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
        <h2 class="card-title text-base">挂载说明</h2>
        <p class="text-sm opacity-70">
          在终端里粘一行就行，装完就在 <code>${esc(pickedMount()?.mountpoint ?? "")}</code> 下面，
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
  // 目录行的大小：只问看得见的（一层里几百个目录时，没滚到的没必要算——服务端算一趟
  // 最多花半秒）。命中缓存的那一行会立刻填上。
  watchForSizes([...root.querySelectorAll("[data-size]")]);
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
