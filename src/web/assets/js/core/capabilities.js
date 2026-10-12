import { State } from './state.js';

const CONTROLS = {
  csv_export: '#action-tool-csv-download',
  csv_import: '#action-tool-csv-import',
  general_lastup_update: '#btn-gl-narou, #btn-gl-other, #action-update-general-lastup, #gl-update-submit',
  mail: '#download-mail, #action-other-mail',
  send: '#btn-send, .button-send',
  shutdown: '#action-option-shutdown',
  reboot: '#action-option-server-reboot',
  open_folder: '#action-other-folder',
  backup: '#action-other-backup',
  backup_bookmark: '#action-send-backup-bookmark',
  library_backup: '#library-backup-create',
  inspect: '#action-other-inspect',
  setting_burn: '#action-other-setting-burn',
  diff_history: '#action-other-diff',
  self_update: '#about-update, #update-variant-standard, #update-variant-gpl',
};

export function canUseAction(action) {
  return State.capabilities?.actions?.[action] === true;
}

export function markActionCapability(element, action) {
  if (!element) return;
  element.dataset.capability = action;
  const supported = canUseAction(action);
  element.setAttribute('aria-disabled', supported ? 'false' : 'true');
  const wasBlocked = element.dataset.capabilityBlocked === 'true';
  if (!supported || wasBlocked) {
    const selectionBlocked = element.classList.contains('enable-selected') && State.selectedIds.size === 0;
    element.classList.toggle('disabled', !supported || selectionBlocked);
    if ('disabled' in element) element.disabled = !supported || selectionBlocked;
  }
  element.dataset.capabilityBlocked = String(!supported);
  if (!supported) {
    if (!element.dataset.capabilityTitle) element.dataset.capabilityTitle = element.title || '';
    element.title = 'この環境では利用できません';
    if (element.type === 'checkbox') element.checked = false;
  } else if (element.dataset.capabilityTitle !== undefined) {
    element.title = element.dataset.capabilityTitle;
  }
}

export function applyActionCapabilities() {
  for (const [action, selector] of Object.entries(CONTROLS)) {
    document.querySelectorAll(selector).forEach(element => markActionCapability(element, action));
  }
  document.querySelectorAll('[data-capability]').forEach(element => {
    markActionCapability(element, element.dataset.capability);
  });
  const history = State.capabilities?.console_history;
  const historyButton = document.getElementById('console-history');
  if (historyButton) {
    historyButton.setAttribute('aria-disabled', history?.available ? 'false' : 'true');
    historyButton.title = history?.available
      ? `保持済みの履歴を取得 (最大 ${history.capacity} 件)` : 'この環境では履歴を取得できません';
  }
  const clearButton = document.getElementById('console-trash');
  if (clearButton) clearButton.setAttribute('aria-disabled', history?.clear ? 'false' : 'true');
}

export function setCapabilities(capabilities) {
  State.capabilities = capabilities;
  applyActionCapabilities();
}

export function initCapabilityGuard(notify) {
  document.addEventListener('click', event => {
    const target = event.target.closest?.('[data-capability], #console-history, #console-trash');
    if (!target) return;
    const supported = target.dataset.capability
      ? canUseAction(target.dataset.capability)
      : target.id === 'console-history' ? State.capabilities?.console_history?.available
      : State.capabilities?.console_history?.clear;
    if (!supported) {
      event.preventDefault();
      event.stopImmediatePropagation();
      notify(State.capabilities ? 'この環境では利用できません' : '実行環境の情報を読み込み中です');
    }
  }, true);
}
