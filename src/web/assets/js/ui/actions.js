import { elements, state, syncSelectionUi } from "../core/state.js";
import { clearConsole } from "./render.js";

export function bindActions(handlers) {
  elements.searchInput.addEventListener("input", async (event) => {
    state.search = event.target.value.trim();
    await handlers.loadNovels();
  });

  document.querySelectorAll(".sort-button").forEach((button) => {
    button.addEventListener("click", async () => {
      const nextColumn = Number(button.dataset.column);
      if (state.sortColumn === nextColumn) {
        state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
      } else {
        state.sortColumn = nextColumn;
        state.sortDir = "asc";
      }
      await handlers.loadNovels();
    });
  });

  elements.langJa.addEventListener("click", async () => {
    await handlers.changeLanguage("ja");
  });
  elements.langEn.addEventListener("click", async () => {
    await handlers.changeLanguage("en");
  });

  elements.selectAll.addEventListener("change", () => {
    const checked = elements.selectAll.checked;
    state.novels.forEach((novel) => {
      if (checked) {
        state.selected.add(novel.id);
      } else {
        state.selected.delete(novel.id);
      }
    });
    handlers.renderNovels();
  });

  elements.novelsTbody.addEventListener("change", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLInputElement) || !target.classList.contains("row-checkbox")) {
      return;
    }
    const id = Number(target.dataset.id);
    if (target.checked) {
      state.selected.add(id);
    } else {
      state.selected.delete(id);
    }
    syncSelectionUi();
  });

  elements.novelsTbody.addEventListener("click", async (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) {
      return;
    }
    const actionButton = target.closest("[data-action]");
    if (actionButton instanceof HTMLElement) {
      await handlers.handleRowAction(actionButton.dataset.action, Number(actionButton.dataset.id));
      return;
    }

    const tagButton = target.closest("[data-filter-tag]");
    if (tagButton instanceof HTMLElement) {
      await handlers.filterByTag(tagButton.dataset.filterTag || "");
    }
  });

  document.getElementById("refresh-button").addEventListener("click", () => handlers.refreshAll(true));
  document.getElementById("download-button").addEventListener("click", handlers.queueDownload);
  document.getElementById("update-button").addEventListener("click", handlers.queueUpdate);
  document.getElementById("convert-button").addEventListener("click", handlers.queueConvert);
  document.getElementById("freeze-button").addEventListener("click", () => handlers.batchMutation("freeze"));
  document.getElementById("unfreeze-button").addEventListener("click", () => handlers.batchMutation("unfreeze"));
  document.getElementById("remove-button").addEventListener("click", handlers.removeSelected);
  document.getElementById("clear-selection-button").addEventListener("click", handlers.clearSelection);
  document.getElementById("queue-clear-button").addEventListener("click", handlers.clearQueue);
  document.getElementById("save-notepad-button").addEventListener("click", handlers.saveNotepad);
  document.getElementById("console-clear-button").addEventListener("click", clearConsole);
}
