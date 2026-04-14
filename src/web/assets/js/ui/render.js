import { elements, state, syncSelectionUi } from "../core/state.js";
import { t } from "./i18n.js";

export function renderNovels() {
  elements.novelsTbody.innerHTML = "";

  if (state.novels.length === 0) {
    const empty = document.createElement("tr");
    empty.innerHTML = `<td colspan="10"><div class="empty-state">${escapeHtml(t("emptyState"))}</div></td>`;
    elements.novelsTbody.appendChild(empty);
    syncSelectionUi();
    return;
  }

  for (const novel of state.novels) {
    const fragment = elements.rowTemplate.content.cloneNode(true);
    const row = fragment.querySelector("tr");
    const checkbox = fragment.querySelector(".row-checkbox");
    checkbox.dataset.id = String(novel.id);
    checkbox.checked = state.selected.has(novel.id);
    row.classList.toggle("is-selected", checkbox.checked);

    fragment.querySelector(".row-id").textContent = novel.id;
    fragment.querySelector(".row-title").textContent = novel.title;
    fragment.querySelector(".row-author").textContent = novel.author;
    fragment.querySelector(".row-site").textContent = novel.sitename;
    fragment.querySelector(".row-updated").textContent = novel.last_update;
    fragment.querySelector(".row-latest").textContent = novel.general_lastup || "—";

    const tagCell = fragment.querySelector(".row-tags");
    for (const tag of novel.tags) {
      tagCell.appendChild(createTagElement(tag));
    }

    const statusCell = fragment.querySelector(".row-status");
    statusCell.appendChild(createStatusBadge(novel));

    const actions = fragment.querySelector(".row-actions");
    actions.appendChild(createActionButton(t("actionUpdate"), "update", novel.id));
    actions.appendChild(createActionButton(t("actionConvert"), "convert", novel.id));
    actions.appendChild(
      createActionButton(
        novel.frozen ? t("actionUnfreeze") : t("actionFreeze"),
        novel.frozen ? "unfreeze" : "freeze",
        novel.id,
      ),
    );
    actions.appendChild(createActionButton(t("actionTags"), "tags", novel.id));
    actions.appendChild(createActionButton(t("actionRemove"), "remove", novel.id));

    elements.novelsTbody.appendChild(fragment);
  }

  syncSelectionUi();
}

export function renderTags(tags) {
  elements.tagList.innerHTML = "";
  tags.forEach((tag) => elements.tagList.appendChild(createTagElement(tag, true)));
}

export function updateQueueUi(queue) {
  const pending = queue.pending || 0;
  const completed = queue.completed || 0;
  const failed = queue.failed || 0;

  elements.queuePending.textContent = pending;
  elements.queueCompleted.textContent = completed;
  elements.queueFailed.textContent = failed;
  elements.queuePendingDetail.textContent = pending;
  elements.queueCompletedDetail.textContent = completed;
  elements.queueFailedDetail.textContent = failed;
}

export function appendEvent(level, message) {
  const entry = document.createElement("div");
  entry.className = "event-entry";
  const now = new Date().toLocaleTimeString("ja-JP", { hour12: false });
  entry.innerHTML = `<strong>[${escapeHtml(String(level).toUpperCase())}]</strong> ${escapeHtml(now)} ${escapeHtml(message || "")}`;
  elements.eventLog.appendChild(entry);
  elements.eventLog.scrollTop = elements.eventLog.scrollHeight;

  state.eventCount += 1;
  if (state.eventCount > 300 && elements.eventLog.firstChild) {
    elements.eventLog.removeChild(elements.eventLog.firstChild);
    state.eventCount -= 1;
  }
}

export function clearConsole() {
  elements.eventLog.innerHTML = "";
  state.eventCount = 0;
}

function createStatusBadge(novel) {
  const badge = document.createElement("span");
  badge.className = "status-badge";
  const labels = [];

  if (novel.frozen) {
    badge.classList.add("frozen");
    labels.push(t("statusFrozen"));
  }
  if (novel.end) {
    badge.classList.add("end");
    labels.push(t("statusEnd"));
  }
  if (labels.length === 0) {
    labels.push(t("statusActive"));
  }

  badge.textContent = labels.join(" / ");
  return badge;
}

function createActionButton(label, action, id) {
  const button = document.createElement("button");
  button.className = "row-action";
  button.type = "button";
  button.dataset.action = action;
  button.dataset.id = String(id);
  button.textContent = label;
  return button;
}

function createTagElement(tag, clickable = false) {
  const element = document.createElement(clickable ? "button" : "span");
  element.className = clickable ? "tag-link" : "tag-pill";
  element.textContent = tag;
  if (clickable) {
    element.type = "button";
    element.dataset.filterTag = tag;
  }
  return element;
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
