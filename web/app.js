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
    // 正在按大小排的话，量回来的数得马上算进去——不然顺序跟眼前看到的数字对不上。
    if (sort.key === "size") sortRows();
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

// ---- 排序 ----

// 排序方式记在一个标签页里：换目录、刷新都还算数，标签页关了就没。跟目录大小一样
// 放 sessionStorage（见上面 SIZES_KEY）。
const SORT_STORE = "cherry-sort";

/** 名字按字节序比：跟 `ls`、跟挂载点里列出来的一致（`file10` 排在 `file2` 后面）。 */
function byName(a, b) {
  if (a.name < b.name) return -1;
  return a.name > b.name ? 1 : 0;
}

/**
 * 一行拿来排序的大小。文件夹的大小得单独问服务端（`_api/size`，只问看得见的那些），
 * 问回来的算数；还没问到的按最大算——它们在「大到小」时排在前面，量过的往后让。
 */
function sizeOf(entry, dir) {
  if (entry.kind !== "dir") return entry.size;
  const got = sizes[dir ? `${dir}/${entry.name}` : entry.name];
  return got && got.stop === "done" ? got.bytes : Number.POSITIVE_INFINITY;
}

/** 能点的三列。第一次点给最想先看到的那个方向：名字从 A 头，大小从大到小，改过从新到旧。 */
const SORT_COLS = {
  name: { 首点: 1, 比: byName },
  // 文件夹的大小看 sizeOf：量到的按量到的排，没量到的按最大算。比出来相等就保持原样
  // （排一次是稳定的），所以还没量到的那些文件夹彼此之间仍是名字序。
  size: {
    首点: -1,
    比: (a, b, dir) => {
      const x = sizeOf(a, dir);
      const y = sizeOf(b, dir);
      return x === y ? 0 : x < y ? -1 : 1;
    },
  },
  mtime: { 首点: -1, 比: (a, b) => a.mtime - b.mtime },
};

function readSort() {
  try {
    const [key, dir] = (sessionStorage.getItem(SORT_STORE) ?? "").split(":");
    if (SORT_COLS[key]) return { key, dir: Number(dir) < 0 ? -1 : 1 };
  } catch {
    // 存不了（隐私模式之类）：那就不记，每次都用默认的。
  }
  return null;
}

let sort = readSort() ?? { key: "name", dir: 1 };
let rows = [];
let tbody = null;
let dirPath = "";

/** 排一次。文件夹永远在前面（跟 Finder 里看到的一样），所以只在同类之间比。 */
function sortRows() {
  if (tbody) {
    const cmp = SORT_COLS[sort.key].比;
    const 有序 = rows.slice().sort((x, y) => {
      const 组 = (x.entry.kind === "dir" ? 0 : 1) - (y.entry.kind === "dir" ? 0 : 1);
      return 组 || cmp(x.entry, y.entry, dirPath) * sort.dir;
    });
    const frag = document.createDocumentFragment();
    for (const r of 有序) frag.append(r.tr);
    tbody.append(frag);
  }
  // 点到哪一列、哪一头，表头上得看得出来。
  for (const b of document.querySelectorAll("[data-sort]")) {
    const on = b.dataset.sort === sort.key;
    b.querySelector(".sort-mark").textContent = on ? (sort.dir > 0 ? "▲" : "▼") : "";
    b.closest("th").setAttribute(
      "aria-sort",
      on ? (sort.dir > 0 ? "ascending" : "descending") : "none",
    );
  }
}

function bindSort() {
  for (const b of document.querySelectorAll("[data-sort]")) {
    b.addEventListener("click", () => {
      const key = b.dataset.sort;
      sort = sort.key === key ? { key, dir: -sort.dir } : { key, dir: SORT_COLS[key].首点 };
      try {
        sessionStorage.setItem(SORT_STORE, `${sort.key}:${sort.dir}`);
      } catch {
        // 同上：记不住就记不住。
      }
      sortRows();
    });
  }
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

/** 表头上那个按钮：点一下按这一列排，再点一下翻方向。 */
function sortHead(key, label) {
  return `<button type="button" data-sort="${key}"
    class="inline-flex items-center gap-1 cursor-pointer select-none hover:opacity-70"
  >${label}<span class="sort-mark text-xs opacity-60"></span></button>`;
}

function listCard(data) {
  const body = data.cherries.map((e) => fileRow(data, e)).join("");
  return `
    <section class="card bg-base-100 shadow-sm">
      <div class="card-body">
        ${crumbsOf(data)}
        <table class="table table-sm">
          <thead><tr>
            <th>${sortHead("name", "名字")}</th>
            <th>${sortHead("size", "大小")}</th>
            <th class="text-right hidden sm:table-cell">${sortHead("mtime", "改过")}</th>
            <th></th>
          </tr></thead>
          <tbody>${body || `<tr><td colspan="4" class="opacity-60">这里什么都没有。</td></tr>`}</tbody>
        </table>
      </div>
    </section>`;
}

// ---- 挂载说明（右上角的复制按钮 + 展开面板） ----

let mounts = [];
let host = null;
let altHost = null;
let useAltHost = false;
let expanded = false;

function pickedMount() {
  return mounts.find((m) => m.label === visitorOS()) ?? mounts[0];
}

function cmdOf(m) {
  return useAltHost && m.alt_command ? m.alt_command : m.command;
}

/** 切换按钮上的字：要换成的地址直接写出来——title 手机上看不见。 */
function flipHostText(target) {
  return `${target.kind === "ip" ? "换成 IP" : "换成主机名"} ${target.addr}`;
}

function mountPanelHtml() {
  const next = useAltHost ? host : altHost;
  const flipBtn = next
    ? `<button type="button" class="btn btn-sm btn-ghost" data-flip-host title="名字过不了路由和隧道，IP 会随 DHCP 变——这条不能用就换那条">${esc(flipHostText(next))}</button>`
    : "";
  const tabs = mounts.map((m, i) => {
    const cmd = cmdOf(m);
    return `
    <input type="radio" name="os" role="tab" class="tab" aria-label="${esc(m.label)}" ${i === 0 ? "checked" : ""}>
    <div role="tabpanel" class="tab-content bg-base-100 border-base-300 rounded-box p-4">
      <pre class="text-xs whitespace-pre-wrap break-all">${esc(cmd)}</pre>
      <div class="mt-3 flex gap-2 items-center">
        <button class="btn btn-sm btn-primary" data-copy="${esc(cmd)}">复制这行</button>
        ${flipBtn}
        <span class="text-xs opacity-60">卸掉：<code>${esc(m.unmount)}</code></span>
      </div>
    </div>`;
  }).join("");
  return `
    <section class="card bg-base-100 shadow-sm">
      <div class="card-body">
        <h2 class="card-title text-base">挂载说明</h2>
        <p class="text-sm opacity-70">
          在终端里粘一行就行，装完就在 <code>${esc(pickedMount()?.mountpoint ?? "")}</code> 下面，
          跟本地目录一样能用（Finder 里也能看到）。<strong>挂上的是只读的</strong>：能看、能拷，
          改不了——要改就先拷到自己那边。
        </p>
        <div role="tablist" class="tabs tabs-lift mt-2">${tabs}</div>
      </div>
    </section>`;
}

/** 原地换掉两个标签页里的命令和按钮字，不动当前选中的系统标签页。 */
function refreshMountCommands() {
  const next = useAltHost ? host : altHost;
  mountPanel.querySelectorAll("[role=tabpanel]").forEach((panel, i) => {
    const m = mounts[i];
    if (!m) return;
    const cmd = cmdOf(m);
    panel.querySelector("pre").textContent = cmd;
    panel.querySelector("[data-copy]").dataset.copy = cmd;
    const flip = panel.querySelector("[data-flip-host]");
    if (flip && next) flip.textContent = flipHostText(next);
  });
}

function bindMountButtons(scope) {
  scope.querySelectorAll("[data-copy]").forEach((b) =>
    b.addEventListener("click", () => copy(b.dataset.copy, b))
  );
  scope.querySelectorAll("[data-flip-host]").forEach((b) =>
    b.addEventListener("click", () => {
      useAltHost = !useAltHost;
      refreshMountCommands();
    })
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
  // 名字是用户在 config.toml 里写的自由文本，一律走 textContent：写个
  // `<img onerror=...>` 就能在这台内网 http 服务上执行脚本，而访问者是别人。
  const owner = data.owner || "CherryShare";
  document.title = `${data.name} — ${owner}`;
  document.getElementById("basket").textContent = data.name;
  document.getElementById("owner").textContent = owner;

  mounts = data.mounts;
  host = data.host;
  altHost = data.alt_host;
  mountPanel.innerHTML = mountPanelHtml();
  bindMountButtons(mountPanel);
  const quick = document.getElementById("copy-mount");
  quick.disabled = false;
  quick.onclick = (e) => {
    const m = pickedMount();
    if (m) copy(cmdOf(m), e.currentTarget);
  };

  root.innerHTML = listCard(data);
  // 行节点留一份：排序是挪这些节点，不是重新生成——已经问回来的大小跟着行一起走。
  tbody = root.querySelector("tbody");
  dirPath = data.path;
  rows = data.cherries.length
    ? [...tbody.children].map((tr, i) => ({ entry: data.cherries[i], tr }))
    : [];
  bindSort();
  sortRows();
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
