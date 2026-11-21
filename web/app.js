"use strict";

const byId = (id) => document.getElementById(id);
const registration = byId("registration");
const employeeInput = byId("employee");
const workbench = byId("workspace");
const workbenchView = byId("workbench-view");
const libraryList = byId("library-list");
const refreshButton = byId("refresh-libraries");
const searchLibrary = byId("search-library");
const importSection = byId("import-section");
const importFields = byId("import-fields");
const fileInput = byId("markdown-files");
const directoryInput = byId("markdown-directory");
const pathInput = byId("markdown-path");
const importButton = byId("submit-import");
const uploadProgress = byId("upload-progress");
const dialog = byId("confirmation");
const cancelConfirmation = byId("cancel-confirmation");
const confirmButton = byId("confirm-action");
const operations = new Map();
const operationsKey = "knowledge-workbench.pending-imports";

let employee = null;
let sessionVersion = 0;
let refreshVersion = 0;
let importTarget = null;
let importTrigger = null;
let pendingConfirmation = null;

function element(tag, text) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  return node;
}

function message(id, text, state = "info") {
  const node = byId(id);
  node.dataset.state = state;
  node.textContent = text;
}

function jsonBody(data) {
  return { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data) };
}

async function request(url, options, readJson = true) {
  const version = sessionVersion;
  const response = await fetch(url, { cache: "no-store", ...options });
  if (!response.ok) {
    const body = await response.json().catch(() => null);
    throw responseError(version, response.status, body, response.headers.get("X-Operation-Id"));
  }
  return readJson ? response.json() : undefined;
}

function responseError(version, status, body, operationId) {
  if (status === 401 && employee !== null && sessionVersion === version) {
    showRegistration("登记会话已失效，请重新登记。未完成的导入可在登记后继续查看。");
  }
  return Object.assign(new Error((body && body.message) || `HTTP ${status}`), {
    status, kind: body ? body.error : undefined, operationId,
  });
}

function uploadImport(url, body) {
  const version = sessionVersion;
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", url);
    xhr.responseType = "json";
    xhr.upload.addEventListener("progress", (event) => {
      if (!event.lengthComputable) return;
      const percent = Math.floor(100 * event.loaded / event.total);
      if (uploadProgress.hasAttribute("value") && uploadProgress.value === percent) return;
      uploadProgress.value = percent;
      message("import-status", `正在上传文件：${percent}%`);
    });
    xhr.upload.addEventListener("load", () => {
      uploadProgress.hidden = true;
      message("import-status", "文件已发送，等待服务端校验…");
    });
    xhr.addEventListener("load", () => {
      if (xhr.status >= 200 && xhr.status < 300 && xhr.response && xhr.response.id) {
        resolve(xhr.response);
      } else {
        reject(responseError(version, xhr.status, xhr.response, xhr.getResponseHeader("X-Operation-Id")));
      }
    });
    xhr.addEventListener("error", () => reject(new Error("upload connection failed")));
    xhr.send(body);
  });
}

function showError(id, action, error) {
  const descriptions = {
    invalid_input: "输入内容不符合要求",
    not_found: "找不到指定的知识库或操作",
    conflict: "操作冲突，请刷新列表后再试",
    unauthorized: "请先重新登记工号",
    forbidden: "服务拒绝此请求，请从工作台的原始网址重新打开页面",
    storage: "知识库或操作记录存储不可用",
    unknown: "无法确认操作结果，请先刷新列表核对实际状态",
  };
  const detail = error.status ? (descriptions[error.kind] || "服务未能完成请求") : "无法取得有效响应，请检查服务连接";
  const serverMessage = error.status ? `（${error.message}）` : "";
  message(id, `${action}：${detail}${serverMessage}。`, "error");
}

function showRegistration(note = "") {
  if (employee !== null) employeeInput.value = employee;
  employee = null;
  sessionVersion++;
  const wasConfirming = !dialog.hidden;
  finishConfirmation(false, false);
  for (const job of operations.values()) clearTimeout(job.timer);
  byId("current-employee").hidden = true;
  byId("session-check").hidden = true;
  workbench.hidden = true;
  registration.hidden = false;
  message("registration-status", note, note ? "error" : "info");
  if (!workbenchView.hidden && (wasConfirming || document.activeElement === document.body)) employeeInput.focus();
}

function activateSession(session, focusWorkbench = false) {
  employee = session.employee;
  sessionVersion++;
  const identity = byId("current-employee");
  identity.textContent = `自报工号：${employee}`;
  identity.hidden = false;
  byId("session-check").hidden = true;
  registration.hidden = true;
  workbench.hidden = false;
  if (focusWorkbench) byId("library-name").focus();
  void refreshLibraries();
  for (const job of operations.values()) {
    if (!job.done) void pollOperation(job);
  }
}

function libraryUrl(name, action = "") {
  return `/api/library${action}?name=${encodeURIComponent(name)}`;
}

function renderLibraryPaths(name) {
  const details = element("details");
  details.className = "library-paths";
  const summary = element("summary", "导入路径");
  const status = element("p");
  status.className = "message";
  status.setAttribute("role", "status");
  details.append(summary, status);
  let loading = false;
  let loaded = false;
  details.addEventListener("toggle", async () => {
    if (!details.open || loading || loaded) return;
    loading = true;
    status.dataset.state = "info";
    status.textContent = "正在读取导入路径…";
    try {
      const { paths, complete } = await request(`/api/library/paths?name=${encodeURIComponent(name)}`);
      if (!details.isConnected) return;
      summary.textContent = `导入路径（${paths.length}）`;
      status.textContent = complete ? "" : "旧版本仅能恢复已索引的路径，可能缺少空文件。";
      if (paths.length) {
        const list = element("ul");
        for (const path of paths) list.append(element("li", path));
        details.append(list);
      }
      loaded = true;
    } catch {
      status.dataset.state = "error";
      status.textContent = "读取导入路径失败。请收起后重新展开。";
    } finally {
      loading = false;
    }
  });
  return details;
}

async function refreshLibraries() {
  const version = ++refreshVersion;
  refreshButton.disabled = true;
  message("libraries-status", "正在读取知识库列表…");
  try {
    const libraries = await request("/api/libraries");
    if (version !== refreshVersion || employee === null) return;
    const previous = searchLibrary.value;
    const available = new Set(libraries.map(({ name }) => name));
    searchLibrary.length = 0;
    searchLibrary.add(new Option("全部知识库", ""));
    for (const { name, published } of libraries) {
      searchLibrary.add(new Option(published ? name : `${name}（未发布）`, name));
    }
    if (available.has(previous)) searchLibrary.value = previous;
    if (importTarget !== null && !available.has(importTarget)) closeImport(false);
    libraryList.textContent = "";
    for (const library of libraries) libraryList.append(renderLibrary(library));
    const note = libraries.length ? `共 ${libraries.length} 个知识库。` : "还没有知识库。创建一个知识库后即可导入 Markdown。";
    message("libraries-status", previous && !available.has(previous) ? `${note} 原选知识库已不在列表中，搜索范围已恢复为全部知识库。` : note);
  } catch (error) {
    if (version === refreshVersion) {
      const action = libraryList.childElementCount ? "刷新列表失败，当前显示的是上次读取的数据" : "读取知识库列表失败";
      showError("libraries-status", action, error);
    }
  } finally {
    if (version === refreshVersion) refreshButton.disabled = false;
  }
}

function renderLibrary({ name, published }) {
  const row = element("li");
  row.className = "library-row";
  const info = element("div");
  const publication = element("p", published ? "已发布" : "未发布");
  publication.className = "publication";
  publication.dataset.published = published;
  info.append(element("strong", name), publication);
  const actions = element("div");
  actions.className = "row-actions";
  const upload = element("button", published ? "重新导入" : "导入");
  upload.type = "button";
  upload.dataset.action = "import";
  upload.setAttribute("aria-label", `${upload.textContent}：${name}`);
  upload.addEventListener("click", () => openImport(name, upload));
  const remove = element("button", "删除");
  remove.type = "button";
  remove.className = "danger-quiet";
  remove.dataset.action = "delete";
  remove.setAttribute("aria-label", `删除：${name}`);
  remove.addEventListener("click", () => void deleteLibrary(name, remove));
  actions.append(upload, remove);
  row.append(info, actions);
  if (published) row.append(renderLibraryPaths(name));
  return row;
}

function confirmAction(title, note, label, trigger, danger = false) {
  if (pendingConfirmation) return Promise.resolve(false);
  byId("confirmation-title").textContent = title;
  byId("confirmation-message").textContent = note;
  confirmButton.textContent = label;
  confirmButton.className = danger ? "danger" : "primary";
  return new Promise((resolve) => {
    pendingConfirmation = { resolve, trigger };
    dialog.hidden = false;
    cancelConfirmation.focus();
  });
}

function finishConfirmation(confirmed, restoreFocus = true) {
  if (!pendingConfirmation) return;
  const { resolve, trigger } = pendingConfirmation;
  pendingConfirmation = null;
  dialog.hidden = true;
  if (restoreFocus) (trigger.isConnected ? trigger : refreshButton).focus();
  resolve(confirmed);
}

cancelConfirmation.addEventListener("click", () => finishConfirmation(false));
confirmButton.addEventListener("click", () => finishConfirmation(true));
dialog.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    finishConfirmation(false);
  } else if (event.key === "Tab" && !event.ctrlKey && !event.altKey && !event.metaKey) {
    event.preventDefault();
    (document.activeElement === cancelConfirmation ? confirmButton : cancelConfirmation).focus();
  }
});

async function deleteLibrary(name, button) {
  if (!await confirmAction("删除知识库", `确定删除「${name}」？该库将不再出现在搜索中。不会影响其他知识库，也不会修改本地原始文件。`, "确认删除", button, true) || employee === null) return;
  button.disabled = true;
  message("library-action-status", `正在删除「${name}」…`);
  try {
    await request(libraryUrl(name), { method: "DELETE" }, false);
    message("library-action-status", `已删除「${name}」。`, "success");
    await refreshLibraries();
  } catch (error) {
    showError("library-action-status", `删除「${name}」未确认`, error);
    if (employee !== null) await refreshLibraries();
  } finally {
    button.disabled = false;
  }
}

function openImport(name, trigger) {
  if (importFields.disabled) return;
  if (importTarget !== name) byId("import-form").reset();
  importTarget = name;
  importTrigger = trigger;
  importSection.hidden = false;
  byId("import-library").textContent = `目标知识库：${name}`;
  message("import-status", "");
  showSelection();
  fileInput.focus();
}

function closeImport(restoreFocus) {
  importSection.hidden = true;
  byId("import-form").reset();
  importTarget = null;
  if (restoreFocus) (importTrigger && importTrigger.isConnected ? importTrigger : refreshButton).focus();
  importTrigger = null;
}

function selectedDocuments() {
  const fromDirectory = directoryInput.files.length > 0;
  const documents = [];
  let skipped = 0;
  for (const file of (fromDirectory ? directoryInput : fileInput).files) {
    const path = file.webkitRelativePath || file.name;
    if (/\.md$/iu.test(path)) documents.push({ path, file });
    else skipped++;
  }
  return { fromDirectory, documents, skipped };
}

function showSelection() {
  const byPath = pathInput.value.trim() !== "";
  importButton.textContent = byPath ? "按路径导入" : "上传并导入";
  if (byPath) {
    message("import-selection", "");
    return;
  }
  const { fromDirectory, documents, skipped } = selectedDocuments();
  if (!fromDirectory && skipped) {
    message("import-selection", "所选文件中包含非 .md 文件。请重新选择；不会只导入其中一部分。", "error");
  } else if (documents.length) {
    const excluded = skipped ? `将跳过 ${skipped} 个非 .md 文件。` : "";
    message("import-selection", `已选 ${documents.length} 个 Markdown 文件。${excluded}`);
  } else {
    message("import-selection", skipped ? `该文件夹没有 .md 文件（${skipped} 个文件不受支持）。` : "", skipped ? "error" : "info");
  }
}

function savePendingOperations() {
  try {
    sessionStorage.setItem(operationsKey, JSON.stringify(Array.from(operations.values())
      .filter((job) => !job.done).map(({ id, name }) => ({ id, name }))));
  } catch {
    // Status tracking still works in the current page if the browser blocks storage.
  }
}

function trackOperation(id, name) {
  const row = element("li");
  row.dataset.operationId = id;
  const status = element("p");
  status.className = "message";
  status.setAttribute("role", "status");
  const progress = element("progress");
  progress.className = "import-progress";
  progress.setAttribute("aria-label", `已建立索引的文件：${name}`);
  progress.hidden = true;
  const code = element("code", id);
  code.className = "operation-id";
  code.setAttribute("aria-label", `操作编号：${id}`);
  const retry = element("button", "重新检查状态");
  retry.type = "button";
  retry.hidden = true;
  const job = { id, name, row, status, progress, retry, timer: 0, checking: false, done: false };
  retry.addEventListener("click", () => void pollOperation(job));
  row.append(element("strong", name), code, status, progress, retry);
  byId("operation-list").prepend(row);
  byId("operations-section").hidden = false;
  operations.set(id, job);
  return job;
}

const outcomes = {
  succeeded: ["导入成功，已发布新版本。", "success"],
  failed: ["导入失败，未发布新版本。", "error"],
  interrupted: ["导入中断，未发布新版本。", "error"],
  unknown: ["结果未知，服务无法确认本次导入是否已发布。请先核对库状态再决定是否重导。", "error"],
};

function operationStatus(job, outcome, text, state, indexing = null) {
  job.row.dataset.outcome = outcome;
  if (job.status.textContent !== text) job.status.textContent = text;
  job.status.dataset.state = state;
  if (indexing) {
    job.progress.max = indexing.files_total;
    job.progress.value = indexing.files_done;
  }
  job.progress.hidden = indexing === null;
}

function showImportProgress(job, { employee, progress }) {
  let indexing = null;
  let text = "导入进行中。";
  const phase = progress && progress.phase;
  if (phase === "queued") {
    text = "文件已接收，等待建立索引…";
  } else if (phase === "indexing") {
    const { files_done, files_total, passages } = progress;
    text = files_done === files_total ? `正在保存索引（${files_total} 个文件）…` :
      `正在建立索引：已处理 ${files_done} / ${files_total} 个文件，已写入 ${passages} 个文段。`;
    if (files_done < files_total) indexing = progress;
  } else if (phase === "publishing") {
    text = "正在校验并发布新版本…";
  }
  operationStatus(job, "in_progress", `${text}（提交工号：${employee}）`, "info", indexing);
}

async function pollOperation(job) {
  if (employee === null || job.checking) return;
  clearTimeout(job.timer);
  job.checking = true;
  job.retry.hidden = true;
  const version = sessionVersion;
  try {
    const status = await request(`/api/operations/${encodeURIComponent(job.id)}`);
    if (version !== sessionVersion) return;
    if (status.outcome === "in_progress") {
      showImportProgress(job, status);
      job.timer = setTimeout(() => void pollOperation(job), 1000);
    } else {
      const [text, state] = outcomes[status.outcome];
      operationStatus(job, status.outcome, `${text}（提交工号：${status.employee}）`, state);
      job.done = status.outcome !== "unknown";
      job.retry.hidden = job.done;
      savePendingOperations();
      void refreshLibraries();
    }
  } catch (error) {
    if (version === sessionVersion) {
      const reason = error.kind === "storage" ? "操作记录存储不可用" : "未取得操作状态";
      operationStatus(job, "unavailable", `暂时无法检查：${reason}。这不代表导入失败。`, "error");
      job.retry.hidden = false;
    }
  } finally {
    job.checking = false;
    if (version !== sessionVersion && employee !== null && !job.done) void pollOperation(job);
  }
}

function sourceDetail(label, text, code = false) {
  const row = element("div");
  const value = element("dd");
  if (code) value.append(element("code", text));
  else value.textContent = text;
  row.append(element("dt", label), value);
  return row;
}

function evidenceResult(hit) {
  const item = element("li");
  const article = element("article");
  const heading = element("h4", hit.path);
  heading.className = "source-title";
  const source = element("dl");
  source.className = "source-details";
  source.append(sourceDetail("知识库", hit.library), sourceDetail("行号", `L${hit.line_start}-L${hit.line_end}`, true),
    sourceDetail("SHA256", hit.sha256, true), sourceDetail("匹配类型", hit.category === "exact" ? "精确" : "相关"));
  const original = element("pre", hit.text);
  original.className = "evidence";
  article.append(heading, source, original);
  item.append(article);
  return item;
}

const activity = byId("activity-view");
const activityLink = byId("activity-link");
const workbenchLink = byId("workbench-link");
const activityColumns = byId("activity-columns");
const activityRefresh = byId("refresh-activity");
const activityRoster = byId("activity-roster");
const activityActions = { create: "创建", import: "导入", delete: "删除", search: "搜索" };
const activityOutcomes = {
  in_progress: "进行中", succeeded: "成功", failed: "失败", interrupted: "中断", unknown: "结果未知",
};
const activityTime = new Intl.DateTimeFormat("zh-CN", {
  year: "numeric", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", second: "2-digit",
  hourCycle: "h23", timeZoneName: "short",
});

function timestamp(ms) {
  const date = new Date(ms);
  const time = element("time", activityTime.format(date));
  time.dateTime = date.toISOString();
  return time;
}

function operationTable(events, employeeNumber) {
  const table = element("table");
  table.className = "activity-events";
  table.setAttribute("aria-label", `自报工号 ${employeeNumber} 的操作历史`);
  const header = table.createTHead().insertRow();
  for (const title of ["开始时间", "操作", "目标库", "结果"]) {
    const th = element("th", title);
    th.scope = "col";
    header.append(th);
  }
  const body = table.createTBody();
  for (const event of events) {
    const row = body.insertRow();
    row.insertCell().append(timestamp(event.started_at_ms));
    row.insertCell().textContent = activityActions[event.action];
    row.insertCell().textContent = event.library == null ? (event.action === "search" ? "全部知识库" : "—") : event.library;
    const outcome = element("span", activityOutcomes[event.outcome]);
    outcome.className = "event-outcome";
    outcome.dataset.outcome = event.outcome;
    row.insertCell().append(outcome);
  }
  return table;
}

function activityEmployee({ employee: number, last_registered_at_ms }, expanded) {
  const row = element("li");
  const details = element("details");
  details.dataset.employee = number;
  const summary = element("summary");
  summary.className = "roster-grid";
  const registrationTime = timestamp(last_registered_at_ms);
  const registrationLabel = element("span", "最后登记时间：");
  registrationLabel.className = "visually-hidden";
  const arrow = element("span");
  arrow.className = "row-arrow";
  arrow.setAttribute("aria-hidden", "true");
  summary.append(element("strong", number), registrationLabel, registrationTime, arrow);
  const detail = element("div");
  detail.className = "roster-detail";
  const status = element("p");
  status.id = `activity-status-${number}`;
  status.className = "message";
  status.setAttribute("role", "status");
  const history = element("div");
  detail.append(status, history);
  details.append(summary, detail);
  row.append(details);
  let loading = false;
  details.addEventListener("toggle", async () => {
    if (!details.isConnected || !details.open || loading) return;
    loading = true;
    history.textContent = "";
    message(status.id, "正在读取操作历史…");
    try {
      const events = await request(`/api/activity/${encodeURIComponent(number)}`, { credentials: "omit" });
      if (!details.isConnected) return;
      if (events.length) history.append(operationTable(events, number));
      message(status.id, events.length ? `共 ${events.length} 条操作。` : "该工号暂无操作记录。");
    } catch (error) {
      if (details.isConnected) showError(status.id, "读取操作历史失败，可收起后重新展开", error);
    } finally {
      loading = false;
    }
  });
  details.open = expanded;
  return row;
}

async function refreshActivity() {
  if (activityRefresh.disabled) return;
  activityRefresh.disabled = true;
  message("activity-status", "正在读取最近登记…");
  try {
    const employees = await request("/api/activity", { credentials: "omit" });
    const expanded = new Set(Array.from(
      activityRoster.querySelectorAll("details[open]"), (details) => details.dataset.employee));
    const focusedRow = activityRoster.contains(document.activeElement)
      ? document.activeElement.closest("details") : null;
    const focused = focusedRow && focusedRow.dataset.employee;
    let nextFocus;
    const rows = document.createDocumentFragment();
    for (const employee of employees) {
      const row = activityEmployee(employee, expanded.has(employee.employee));
      if (employee.employee === focused) nextFocus = row.querySelector("summary");
      rows.append(row);
    }
    activityRoster.textContent = "";
    activityRoster.append(rows);
    activityColumns.hidden = !employees.length;
    if (focused) (nextFocus || byId("roster-title")).focus();
    message("activity-status", employees.length ? `共 ${employees.length} 个自报工号。` : "最近 30 天暂无成功登记。");
  } catch (error) {
    const action = activityRoster.childElementCount
      ? "刷新活动记录失败，当前显示的是上次读取的数据"
      : "读取活动记录失败，请刷新重试";
    showError("activity-status", action, error);
  } finally {
    activityRefresh.disabled = false;
  }
}

function renderView(moveFocus = false) {
  const showActivity = location.hash === "#activity";
  const changed = activity.hidden === showActivity;
  if (changed) finishConfirmation(false, false);
  activity.hidden = !showActivity;
  workbenchView.hidden = showActivity;
  const currentLink = showActivity ? activityLink : workbenchLink;
  const otherLink = showActivity ? workbenchLink : activityLink;
  currentLink.setAttribute("aria-current", "page");
  otherLink.removeAttribute("aria-current");
  document.title = showActivity ? "活动记录 · 知识库工作台" : "知识库工作台";
  if (moveFocus && changed) {
    window.scrollTo(0, 0);
    (showActivity ? activity : workbenchView).querySelector("h1").focus({ preventScroll: true });
  }
  if (showActivity && changed) void refreshActivity();
}

document.querySelector(".skip-link").addEventListener("click", (event) => {
  event.preventDefault();
  byId("main-content").focus();
});
window.addEventListener("hashchange", () => renderView(true));
activityRefresh.addEventListener("click", () => void refreshActivity());
renderView();

async function start() {
  try {
    for (const { id, name } of JSON.parse(sessionStorage.getItem(operationsKey) || "[]")) {
      trackOperation(id, name);
    }
  } catch {
    // The browser may block storage or discard data for this tab.
  }
  try {
    activateSession(await request("/api/session"));
  } catch (error) {
    showRegistration();
    if (error.status !== 401) showError("registration-status", "检查登记会话失败，可以重新登记", error);
  }
}

byId("registration-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = event.currentTarget.querySelector("button[type=submit]");
  button.disabled = true;
  message("registration-status", "正在登记…");
  try {
    activateSession(await request("/api/session", jsonBody({ employee: employeeInput.value })), true);
  } catch (error) {
    showError("registration-status", "登记失败", error);
  } finally {
    button.disabled = false;
  }
});

refreshButton.addEventListener("click", () => void refreshLibraries());

byId("create-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId("library-name");
  const button = event.currentTarget.querySelector("button[type=submit]");
  const name = input.value;
  input.disabled = button.disabled = true;
  message("library-action-status", `正在创建「${name}」…`);
  try {
    await request("/api/libraries", jsonBody({ name }), false);
    input.value = "";
    message("library-action-status", `已创建「${name}」。`, "success");
    await refreshLibraries();
  } catch (error) {
    showError("library-action-status", `创建「${name}」未确认`, error);
    if (employee !== null) await refreshLibraries();
  } finally {
    input.disabled = button.disabled = false;
  }
});

for (const [input, other] of [[fileInput, directoryInput], [directoryInput, fileInput]]) {
  input.addEventListener("change", () => {
    if (input.files.length) {
      other.value = "";
      pathInput.value = "";
    }
    message("import-status", "");
    showSelection();
  });
}
pathInput.addEventListener("input", () => {
  if (pathInput.value.trim()) fileInput.value = directoryInput.value = "";
  message("import-status", "");
  showSelection();
});
byId("cancel-import").addEventListener("click", () => closeImport(true));

byId("import-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = importTarget;
  if (name === null || importFields.disabled) return;
  const sourcePath = pathInput.value;
  const byPath = sourcePath.trim() !== "";
  const { fromDirectory, documents, skipped } = selectedDocuments();
  if (!byPath && (!documents.length || (!fromDirectory && skipped))) {
    message("import-status", "请选择 Markdown 文件或文件夹，或输入服务器上的绝对路径。", "error");
    return;
  }
  const excluded = skipped ? `文件夹内的 ${skipped} 个非 .md 文件将跳过。` : "";
  const source = byPath
    ? `从「${sourcePath}」导入 Markdown 到「${name}」。`
    : `将 ${documents.length} 个 Markdown 文件导入「${name}」。${excluded}`;
  const note = `${source}如果该库已有发布版本，导入成功后将整库替换，本次未导入的旧文档将不再可查。`;
  const action = byPath ? "确认导入" : "确认上传";
  if (!await confirmAction("确认整库导入", note, action, importButton) || employee === null || importTarget !== name) return;
  importFields.disabled = true;
  if (byPath) {
    message("import-status", "服务器正在读取并校验文件…");
  } else {
    uploadProgress.removeAttribute("value");
    uploadProgress.hidden = false;
    message("import-status", "正在上传文件…");
  }
  try {
    let accepted;
    if (byPath) {
      accepted = await request(libraryUrl(name, "/import-path"), jsonBody({ path: sourcePath }));
    } else {
      const body = new FormData();
      for (const { path, file } of documents) {
        body.append("path", path);
        body.append("file", file);
      }
      accepted = await uploadImport(libraryUrl(name, "/import"), body);
    }
    const { id } = accepted;
    const job = trackOperation(id, name);
    operationStatus(job, "in_progress", "已接收，正在检查导入状态…", "info");
    savePendingOperations();
    if (importTarget === name) {
      byId("import-form").reset();
      showSelection();
      message("import-status", "文件已校验，请在下方查看导入状态。", "success");
    }
    void pollOperation(job);
  } catch (error) {
    showError("import-status", byPath ? "路径导入未确认" : "上传未确认", error);
    if (error.operationId) {
      const job = trackOperation(error.operationId, name);
      operationStatus(job, "unavailable", "导入响应未确认，正在检查操作状态…", "info");
      savePendingOperations();
      void pollOperation(job);
    }
  } finally {
    uploadProgress.hidden = true;
    importFields.disabled = false;
  }
});

byId("search-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const data = { query: byId("search-query").value };
  if (searchLibrary.value) data.library = searchLibrary.value;
  if (byId("search-path").value) data.path = byId("search-path").value;
  const fields = byId("search-fields");
  const summary = byId("search-summary");
  const results = byId("search-results");
  fields.disabled = true;
  summary.hidden = true;
  results.textContent = "";
  message("search-status", "正在搜索…");
  try {
    const reply = await request("/api/search", jsonBody(data));
    const { scope, status, matches, retrieval, reranked } = reply;
    const mode = retrieval === "hybrid" ? "混合检索" : "关键词检索";
    const path = scope.path ? ` · 路径：${scope.path}` : "";
    const library = scope.library == null ? "全部知识库" : scope.library;
    summary.textContent = `范围：${library}${path} · ${mode} · ${reranked ? "已精排" : "未精排"}`;
    summary.hidden = false;
    if (status === "no_published_index") {
      message("search-status", "所选知识库范围内没有已发布的索引。请先导入文档并等待发布。");
    } else if (!matches.length) {
      message("search-status", "已检索发布版本，但没有匹配结果。可调整关键词或放宽路径限制。");
    } else {
      for (const match of matches) results.append(evidenceResult(match));
      message("search-status", `找到 ${matches.length} 条原文证据。`, "success");
    }
  } catch (error) {
    showError("search-status", "搜索失败", error);
  } finally {
    fields.disabled = false;
  }
});

void start();
